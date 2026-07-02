//! The agent loop: read → think → tool-call → verify, until the model stops
//! or calls `attempt_completion`. Ported from mangopi-cli's `agent_loop`,
//! retuned for streaming + the shared `Output` sink.

use crate::context::{image_content, ContextManager};
use crate::output::{StreamSinkAdapter, ToolEnvAdapter};
use crate::Output;
use anyhow::Result;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use wisp_llm::{Content, Provider};
use wisp_tools::Registry;

pub async fn agent_loop(
    ctx: &mut ContextManager,
    provider: &dyn Provider,
    tools: &Registry,
    root: &Path,
    output: &dyn Output,
    user_input: &str,
    max_iter: usize,
    cancel: Option<&AtomicBool>,
) -> Result<()> {
    ctx.append_user(user_input);

    let env = ToolEnvAdapter::new(root.to_path_buf(), output);
    let mut iteration = 0usize;
    loop {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            anyhow::bail!("stopped by user");
        }
        iteration += 1;
        let messages = ctx.prepare_for_api(provider, output).await;
        let mut sink = StreamSinkAdapter::new(output);
        let comp = provider.stream(&messages, &tools.schemas(), &mut sink).await?;

        ctx.append_assistant(comp.content.clone(), comp.tool_calls.clone(), comp.reasoning.clone());
        output.usage(
            iteration,
            comp.usage.input_tokens,
            comp.usage.output_tokens,
            ctx.total_tokens(),
            ctx.max_context,
        );

        if comp.tool_calls.is_empty() {
            break;
        }

        let mut completed = false;
        for tc in &comp.tool_calls {
            let name = tc.function.name.clone();
            let args = tc.args_value();
            let result = tools.run(&name, &args, &env).await;
            let content = if let Some(img) = &result.image {
                image_content(&img.label, &img.data_url)
            } else {
                Content::text(result.content.clone())
            };
            output.tool_result(&name, result.success, &result.content);
            ctx.append_tool(&tc.id, &name, content);
            if name == "attempt_completion" {
                completed = true;
                break;
            }
        }
        if completed { break; }
        if iteration >= max_iter { break; }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            anyhow::bail!("stopped by user");
        }
    }
    Ok(())
}
