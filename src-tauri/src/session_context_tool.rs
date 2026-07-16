//! Live per-session authorization for agent tools that accept `context_id`.

use wisp_llm::ToolSchema;
use wisp_tools::{Tool, ToolEnv, ToolResult};

pub struct SessionExecutionContextTool {
    inner: Box<dyn Tool>,
    store: wisp_store::Store,
    frame_id: String,
}

impl SessionExecutionContextTool {
    pub fn new(
        inner: Box<dyn Tool>,
        store: wisp_store::Store,
        frame_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            store,
            frame_id: frame_id.into(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for SessionExecutionContextTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn schema(&self) -> ToolSchema {
        self.inner.schema()
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        self.inner.preview(args)
    }

    async fn before(&self, args: &serde_json::Value, env: &dyn ToolEnv) {
        self.inner.before(args, env).await;
    }

    async fn run(&self, args: &serde_json::Value, env: &dyn ToolEnv) -> ToolResult {
        let context_id = args
            .get("context_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .unwrap_or("local");
        if context_id != "local" {
            match self
                .store
                .session_execution_context_enabled(&self.frame_id, context_id)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return ToolResult::fail(format!(
                        "Execution context {context_id} is not selected for this session"
                    ))
                }
                Err(error) => {
                    return ToolResult::fail(format!(
                        "Unable to check execution context access: {error}"
                    ))
                }
            }
        }
        self.inner.run(args, env).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_context"
        }

        fn schema(&self) -> ToolSchema {
            ToolSchema::new(
                "echo_context",
                "test",
                serde_json::json!({ "type": "object" }),
            )
        }

        async fn run(&self, _args: &serde_json::Value, _env: &dyn ToolEnv) -> ToolResult {
            ToolResult::ok("ran")
        }
    }

    struct TestEnv(PathBuf);

    #[async_trait::async_trait]
    impl ToolEnv for TestEnv {
        fn project_root(&self) -> &Path {
            &self.0
        }

        async fn confirm(&self, _message: &str) -> bool {
            true
        }

        async fn emit(&self, _event: wisp_tools::ToolEvent) {}
    }

    #[tokio::test]
    async fn remote_context_requires_selection_in_the_same_session() {
        let path =
            std::env::temp_dir().join(format!("wisp_session_tool_{}.sqlite", uuid::Uuid::new_v4()));
        let store = wisp_store::Store::open(&path).await.unwrap();
        store.create_project("p", "Project", "").await.unwrap();
        store.create_frame("f", "p", "OPERON", "m").await.unwrap();
        store
            .upsert_execution_context(&wisp_store::ExecutionContext::new("ssh:gpu", "GPU").unwrap())
            .await
            .unwrap();
        let tool = SessionExecutionContextTool::new(Box::new(EchoTool), store.clone(), "f");
        let env = TestEnv(PathBuf::from("."));

        let denied = tool
            .run(&serde_json::json!({ "context_id": "ssh:gpu" }), &env)
            .await;
        assert!(!denied.success);
        assert!(denied.content.contains("not selected for this session"));

        store
            .set_session_execution_context_enabled("f", "ssh:gpu", true)
            .await
            .unwrap();
        assert!(
            tool.run(&serde_json::json!({ "context_id": "ssh:gpu" }), &env)
                .await
                .success
        );
        assert!(tool.run(&serde_json::json!({}), &env).await.success);

        let _ = std::fs::remove_file(path);
    }
}
