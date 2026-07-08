//! `python` — persistent Python REPL tool backed by `KernelClient`.

use crate::kernel::{KernelClient, KernelResp};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use wisp_llm::ToolSchema;
use wisp_tools::{Tool, ToolEnv, ToolResult};

pub struct ReplTool {
    client: Arc<Mutex<KernelClient>>,
}

const PYTHON_TOOL_DESCRIPTION: &str = "Execute Python code in a persistent REPL. Variables, imports, and loaded data persist across calls. Return values of expressions are printed. Use this for analysis, data loading, plotting, and computation when required packages already exist. Do not use this as a package installer; if dependencies are missing, set up a project-local pixi environment or use local-env-setup first.";

impl ReplTool {
    pub fn new(client: KernelClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
        }
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
        let id = uuid::Uuid::new_v4().to_string();
        let mut client = self.client.lock().await;
        match client.execute(&id, &code, env).await {
            Ok(resp) => {
                let success = resp.error.is_none();
                ToolResult {
                    success,
                    content: Self::format(&resp),
                    image: None,
                }
            }
            Err(e) => ToolResult::fail(format!("python error: {e}")),
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
