//! `search` — glob file search, sorted by mtime (newest first).

use crate::env::{ToolEnv, ToolResult};
use crate::tool::{arg_str, arg_str_opt, Tool};
use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use wisp_llm::ToolSchema;

const MAX_RESULTS: usize = 500;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const TRUNCATED: &str = "... results truncated";

pub struct SearchTool;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "search",
            "Search for files using a glob pattern (e.g. '**/*.rs'). Results are sorted by modification time, newest first.",
            json!({
                "type": "object",
                "properties": {
                    "pat": { "type": "string", "description": "Glob pattern to match file paths (e.g. '**/*.py')" },
                    "path": { "type": "string", "description": "Directory to start search from (default: project root)" }
                },
                "required": ["pat"]
            }),
        )
    }
    fn preview(&self, args: &serde_json::Value) -> String {
        arg_str(args, "pat").unwrap_or_default()
    }
    async fn run(&self, args: &serde_json::Value, env: &dyn ToolEnv) -> ToolResult {
        let pat = match arg_str(args, "pat") {
            Ok(p) => p,
            Err(e) => return ToolResult::fail(e),
        };
        let base = arg_str_opt(args, "path")
            .unwrap_or_else(|| env.project_root().to_string_lossy().to_string());
        let full = Path::new(&base)
            .join(&pat)
            .to_string_lossy()
            .replace("\\\\", "\\");
        let mut hits: Vec<(std::time::SystemTime, String)> = vec![];
        let mut hit_bytes = 0;
        let mut truncated = false;
        for entry in glob::glob(&full).ok().into_iter().flatten().flatten() {
            let path = entry.to_string_lossy().to_string();
            if hits.len() >= MAX_RESULTS
                || hit_bytes + path.len() + 1 > MAX_OUTPUT_BYTES - TRUNCATED.len() - 1
            {
                truncated = true;
                break;
            }
            let mtime = std::fs::metadata(&entry)
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            hit_bytes += path.len() + 1;
            hits.push((mtime, path));
        }
        hits.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out = if hits.is_empty() {
            "none".to_string()
        } else {
            hits.into_iter()
                .map(|(_, p)| p)
                .collect::<Vec<_>>()
                .join("\n")
        };
        if truncated {
            if out != "none" {
                out.push('\n');
            } else {
                out.clear();
            }
            out.push_str(TRUNCATED);
        }
        ToolResult::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ToolEvent;
    use std::path::{Path, PathBuf};

    struct TestEnv(PathBuf);

    #[async_trait::async_trait]
    impl ToolEnv for TestEnv {
        fn project_root(&self) -> &Path {
            &self.0
        }
        async fn confirm(&self, _message: &str) -> bool {
            true
        }
        async fn emit(&self, _event: ToolEvent) {}
    }

    #[tokio::test]
    async fn caps_results_before_collecting_the_whole_glob() {
        let tmp = std::env::temp_dir().join(format!("wisp_search_cap_{}", std::process::id()));
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();
        for i in 0..=MAX_RESULTS {
            std::fs::write(tmp.join(format!("{i:04}.txt")), b"").unwrap();
        }

        let result = SearchTool
            .run(&json!({ "pat": "*.txt" }), &TestEnv(tmp.clone()))
            .await;
        assert!(result.success);
        assert!(result.content.contains("results truncated"));
        assert_eq!(result.content.lines().count(), MAX_RESULTS + 1);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
