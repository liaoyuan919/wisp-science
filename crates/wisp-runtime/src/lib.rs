//! Managed project-scoped language runtimes and agent tool adapters.

pub mod env;
pub mod kernel;
pub mod manager;
pub mod tool;

pub use env::{bundled_mock_mcp_path, bundled_worker_path, resolve_bundled_script, PythonEnv};
pub use kernel::{KernelClient, KernelReady, KernelResp};
pub use manager::{
    LaunchedRuntime, RuntimeEvent, RuntimeExecution, RuntimeInfo, RuntimeKernel, RuntimeKey,
    RuntimeLanguage, RuntimeLauncher, RuntimeManager, RuntimeMetadata, RuntimeOutput,
    RuntimeStatus, LOCAL_CONTEXT_ID,
};
pub use tool::ReplTool;
