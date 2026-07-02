//! uv-managed Python environment + persistent REPL kernel + `python` tool.

pub mod env;
pub mod kernel;
pub mod tool;

pub use env::{bundled_mock_mcp_path, bundled_worker_path, resolve_bundled_script, PythonEnv};
pub use kernel::{KernelClient, KernelResp};
pub use tool::ReplTool;
