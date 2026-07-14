//! `python` — persistent Python REPL tool backed by `RuntimeManager`.

use crate::{KernelResp, RuntimeEvent, RuntimeKey, RuntimeManager};
use async_trait::async_trait;
use serde_json::json;
use wisp_llm::ToolSchema;
use wisp_tools::{Tool, ToolEnv, ToolEvent, ToolResult};

pub struct ReplTool {
    manager: RuntimeManager,
    key: RuntimeKey,
}

const PYTHON_TOOL_DESCRIPTION: &str = "Execute Python code in a persistent REPL. Variables, imports, and loaded data persist across calls. Return values of expressions are printed. Use this for analysis, data loading, plotting, and computation when required packages already exist. Do not use this as a package installer; if dependencies are missing, set up a project-local pixi environment or use local-env-setup first.";

impl ReplTool {
    pub fn new(manager: RuntimeManager, key: RuntimeKey) -> Self {
        Self { manager, key }
    }

    fn format(resp: &KernelResp) -> String {
        let mut out = String::new();
        if !resp.stdout.is_empty() {
            out.push_str(&resp.stdout);
        }
        if !resp.stderr.is_empty() {
            if !out.is_empty() {
                out.push_str("\n");
            }
            out.push_str("[stderr] ");
            out.push_str(&resp.stderr);
        }
        if let Some(err) = &resp.error {
            if !out.is_empty() {
                out.push_str("\n");
            }
            out.push_str("[error] ");
            out.push_str(err);
        }
        if out.is_empty() {
            out = "(no output)".into();
        }
        out
    }
}

#[async_trait]
impl Tool for ReplTool {
    fn name(&self) -> &str {
        "python"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "python",
            PYTHON_TOOL_DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "code": { "type": "string", "description": "Python code to execute (statements or a single expression)" }
                },
                "required": ["code"]
            }),
        )
    }
    fn preview(&self, args: &serde_json::Value) -> String {
        args.get("code")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
    async fn run(&self, args: &serde_json::Value, env: &dyn ToolEnv) -> ToolResult {
        let code = match args.get("code").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => return ToolResult::fail("missing required argument 'code'"),
        };
        let mut execution = match self
            .manager
            .execute(&self.key, env.project_root(), code)
            .await
        {
            Ok(execution) => execution,
            Err(error) => return ToolResult::fail(format!("python error: {error}")),
        };
        let mut cancel_poll = tokio::time::interval(std::time::Duration::from_millis(50));
        loop {
            tokio::select! {
                event = execution.recv() => match event {
                    Some(RuntimeEvent::Stdout(chunk)) => {
                        env.emit(ToolEvent::Stdout { chunk }).await;
                    }
                    Some(RuntimeEvent::Finished(Ok(response))) => {
                        let success = response.error.is_none();
                        return ToolResult {
                            success,
                            content: Self::format(&response),
                            image: None,
                        };
                    }
                    Some(RuntimeEvent::Finished(Err(error))) => {
                        return ToolResult::fail(format!("python error: {error}"));
                    }
                    None => {
                        return ToolResult::fail("python error: runtime ended before returning a result");
                    }
                },
                _ = cancel_poll.tick() => {
                    if env.is_cancelled() {
                        // Dropping this receiver abandons only the caller. The
                        // manager-owned protocol task still drains the cell.
                        return ToolResult::fail("python error: interrupted by user");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PYTHON_TOOL_DESCRIPTION;

    #[test]
    fn python_description_keeps_package_setup_out_of_the_repl() {
        assert!(PYTHON_TOOL_DESCRIPTION.contains("Do not use this as a package installer"));
        assert!(PYTHON_TOOL_DESCRIPTION.contains("project-local pixi"));
        assert!(PYTHON_TOOL_DESCRIPTION.contains("local-env-setup"));
    }
}
