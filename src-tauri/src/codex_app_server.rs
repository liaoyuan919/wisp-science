//! Typed client and configuration resolver for `codex app-server --stdio`.
//!
//! This module deliberately keeps the wire protocol at the Wisp boundary.  UI
//! code consumes [`RuntimeSnapshot`] and [`ResolvedTurnConfig`]; the exact same
//! resolved value is then passed to [`build_turn_start_params`].  That prevents
//! the model picker and the JSON-RPC request from developing separate fallback
//! rules.
//!
//! The app-server protocol is versioned but intentionally forward compatible.
//! Known fields are strongly typed and unknown response fields are retained in
//! `extra` maps so a newer local Codex does not require a Wisp update merely to
//! be inspected.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MUTATING_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_MODEL_PAGES: usize = 100;
static NEXT_ACTOR_ID: AtomicU64 = AtomicU64::new(1);
// This is the spelling emitted by both the 0.142 and 0.144 generated app-server
// schemas. Keep the slash-separated spelling only as a forward-compatible
// fallback for experimental builds that briefly exposed it.
pub const METHOD_PROVIDER_CAPABILITIES: &str = "modelProvider/capabilities/read";
const ALTERNATE_METHOD_PROVIDER_CAPABILITIES: &str = "model/provider/capabilities/read";

fn should_retry_provider_capabilities(error: &RpcErrorObject) -> bool {
    error.code == -32601
        || (error.code == -32600
            && error
                .message
                .to_ascii_lowercase()
                .contains("unknown variant"))
}

// ---------------------------------------------------------------------------
// Runtime discovery
// ---------------------------------------------------------------------------

/// How Codex is launched.  `Wsl` is an explicit, extensible entry point rather
/// than a special string convention, keeping Windows and WSL homes separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEntrypoint {
    Native {
        program: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Wsl {
        #[serde(default = "default_wsl_program")]
        launcher: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        distribution: Option<String>,
        #[serde(default = "default_codex_program")]
        program: String,
        #[serde(default)]
        args: Vec<String>,
        /// Environment variables set *inside* WSL.  In particular, a WSL
        /// `CODEX_HOME` must never be taken from the Windows process env.
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
}

fn default_wsl_program() -> String {
    "wsl.exe".into()
}

fn default_codex_program() -> String {
    "codex".into()
}

impl RuntimeEntrypoint {
    pub fn native(program: impl Into<String>) -> Self {
        Self::Native {
            program: program.into(),
            args: Vec::new(),
        }
    }

    fn codex_program(&self) -> &str {
        match self {
            Self::Native { program, .. } | Self::Wsl { program, .. } => program,
        }
    }

    fn launcher_program(&self) -> &str {
        match self {
            Self::Native { program, .. } => program,
            Self::Wsl { launcher, .. } => launcher,
        }
    }

    fn append_to_command(&self, command: &mut Command, tail: &[&str]) {
        match self {
            Self::Native { args, .. } => {
                command.args(args);
                command.args(tail);
            }
            Self::Wsl {
                distribution,
                program,
                args,
                environment,
                ..
            } => {
                if let Some(distribution) = distribution {
                    command.arg("--distribution").arg(distribution);
                }
                command.arg("--exec");
                if environment.is_empty() {
                    command.arg(program);
                } else {
                    command.arg("env");
                    for (key, value) in environment {
                        command.arg(format!("{key}={value}"));
                    }
                    command.arg(program);
                }
                command.args(args);
                command.args(tail);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeSource {
    Explicit,
    CodexDesktop,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedCodexCommand {
    pub source: RuntimeSource,
    pub entrypoint: RuntimeEntrypoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
}

impl ResolvedCodexCommand {
    pub fn executable(&self) -> &str {
        self.entrypoint.codex_program()
    }

    pub fn launcher(&self) -> &str {
        self.entrypoint.launcher_program()
    }

    fn process_for(&self, tail: &[&str]) -> Command {
        let mut command = Command::new(self.entrypoint.launcher_program());
        self.entrypoint.append_to_command(&mut command, tail);
        command.envs(&self.environment);
        if let Some(home) = &self.codex_home {
            // A WSL home is set by RuntimeEntrypoint::Wsl::environment above.
            if matches!(self.entrypoint, RuntimeEntrypoint::Native { .. }) {
                command.env("CODEX_HOME", home);
            }
        }
        command
    }
}

#[derive(Debug, Clone, Default)]
pub struct RuntimeResolveOptions {
    /// Highest-priority user/profile selection.  It may itself describe WSL.
    pub explicit: Option<RuntimeEntrypoint>,
    pub codex_home: Option<String>,
    pub environment: BTreeMap<String, String>,
    /// Overrides the normal Codex Desktop roots; primarily useful for tests and
    /// portable installations.
    pub desktop_search_roots: Vec<PathBuf>,
    /// Optional PATH value for deterministic resolution.
    pub path_override: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeResolveError {
    NotFound,
}

impl fmt::Display for RuntimeResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str(
                "Codex was not found (checked the explicit command, Codex Desktop, then PATH)",
            ),
        }
    }
}

impl std::error::Error for RuntimeResolveError {}

/// Resolve in the required order: explicit profile command, newest Codex
/// Desktop binary, and finally PATH.
pub fn resolve_codex_command(
    options: &RuntimeResolveOptions,
) -> Result<ResolvedCodexCommand, RuntimeResolveError> {
    if let Some(entrypoint) = &options.explicit {
        return Ok(ResolvedCodexCommand {
            source: RuntimeSource::Explicit,
            entrypoint: entrypoint.clone(),
            codex_home: options.codex_home.clone(),
            environment: options.environment.clone(),
        });
    }

    let roots = if options.desktop_search_roots.is_empty() {
        default_desktop_roots()
    } else {
        options.desktop_search_roots.clone()
    };
    if let Some(path) = newest_desktop_binary(&roots) {
        return Ok(ResolvedCodexCommand {
            source: RuntimeSource::CodexDesktop,
            entrypoint: RuntimeEntrypoint::native(path.to_string_lossy()),
            codex_home: options.codex_home.clone(),
            environment: options.environment.clone(),
        });
    }

    let found = match options.path_override.as_deref() {
        Some(path) => find_on_path(std::ffi::OsStr::new(path)),
        None => env::var_os("PATH").and_then(|value| find_on_path(&value)),
    };
    if let Some(path) = found {
        return Ok(ResolvedCodexCommand {
            source: RuntimeSource::Path,
            entrypoint: RuntimeEntrypoint::native(path.to_string_lossy()),
            codex_home: options.codex_home.clone(),
            environment: options.environment.clone(),
        });
    }

    Err(RuntimeResolveError::NotFound)
}

fn default_desktop_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(path) = env::var_os("CODEX_DESKTOP_BIN") {
        let path = PathBuf::from(path);
        roots.push(path.parent().unwrap_or(&path).to_path_buf());
    }
    if let Some(local) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        roots.extend([
            // Codex Desktop's unpacked CLI lives here in current releases.
            // Prefer it over stale per-user installs and PATH shims.
            local.join("OpenAI").join("Codex"),
            local.join("Programs").join("Codex"),
            local.join("Codex"),
        ]);
        // MSIX package trees are not executable from a non-package Wisp
        // process. The usable Desktop CLI is mirrored under OpenAI\Codex.
    }
    // Do not enumerate Program Files\WindowsApps. Files inside an MSIX package
    // can be stat'ed by a non-package process but cannot be executed (ERROR_ACCESS_DENIED).
    // Codex Desktop mirrors its usable CLI under LOCALAPPDATA\OpenAI\Codex.
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/Applications/Codex.app/Contents/Resources"));
        if let Some(home) = dirs::home_dir() {
            roots.push(
                home.join("Applications")
                    .join("Codex.app")
                    .join("Contents")
                    .join("Resources"),
            );
        }
    } else if !cfg!(windows) {
        roots.push(PathBuf::from("/opt/Codex"));
        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".local").join("share").join("codex"));
        }
    }
    roots
}

fn newest_desktop_binary(roots: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for root in roots {
        if root.is_file() && is_codex_binary(root) {
            candidates.push(binary_candidate(root.clone()));
        } else {
            collect_codex_binaries(root, 0, 12, &mut candidates);
        }
    }
    candidates.sort_by(|left, right| {
        compare_version_keys(&left.version, &right.version)
            .then_with(|| left.modified.cmp(&right.modified))
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates.pop().map(|candidate| candidate.path)
}

#[derive(Debug)]
struct BinaryCandidate {
    path: PathBuf,
    version: Vec<u64>,
    modified: SystemTime,
}

fn binary_candidate(path: PathBuf) -> BinaryCandidate {
    let modified = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    let version = version_key(&path.to_string_lossy());
    BinaryCandidate {
        path,
        version,
        modified,
    }
}

fn collect_codex_binaries(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<BinaryCandidate>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_file() && is_codex_binary(&path) {
            out.push(binary_candidate(path));
        } else if file_type.is_dir() {
            collect_codex_binaries(&path, depth + 1, max_depth, out);
        }
    }
}

fn is_codex_binary(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    if cfg!(windows) {
        name == "codex.exe" || name == "codex.cmd" || name == "codex.bat"
    } else {
        name == "codex"
    }
}

fn version_key(value: &str) -> Vec<u64> {
    let mut best = Vec::new();
    let mut run = String::new();
    for ch in value.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_digit() || ch == '.' {
            run.push(ch);
        } else {
            let parts = run
                .trim_matches('.')
                .split('.')
                .filter_map(|part| part.parse::<u64>().ok())
                .collect::<Vec<_>>();
            if parts.len() >= 2 && compare_version_keys(&parts, &best).is_gt() {
                best = parts;
            }
            run.clear();
        }
    }
    best
}

fn compare_version_keys(left: &[u64], right: &[u64]) -> std::cmp::Ordering {
    let len = left.len().max(right.len());
    for index in 0..len {
        let ordering = left
            .get(index)
            .copied()
            .unwrap_or_default()
            .cmp(&right.get(index).copied().unwrap_or_default());
        if !ordering.is_eq() {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

fn find_on_path(path: &std::ffi::OsStr) -> Option<PathBuf> {
    #[cfg(windows)]
    const NAMES: &[&str] = &["codex.exe", "codex.cmd", "codex.bat", "codex"];
    #[cfg(not(windows))]
    const NAMES: &[&str] = &["codex"];

    for directory in env::split_paths(path) {
        for name in NAMES {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Ask the selected runtime itself for its version; never probe a different
/// PATH installation after selection.
pub async fn probe_runtime_version(
    command: &ResolvedCodexCommand,
) -> Result<String, AppServerClientError> {
    let mut process = command.process_for(&["--version"]);
    process
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), process.output())
        .await
        .map_err(|_| AppServerClientError::Timeout {
            method: "runtime --version".into(),
            timeout_ms: 15_000,
        })?
        .map_err(|error| AppServerClientError::Io(error.to_string()))?;
    if !output.status.success() {
        return Err(AppServerClientError::Process(format!(
            "{} --version exited with {}: {}",
            command.executable(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Serializable app-server data
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResponse {
    pub user_agent: String,
    pub codex_home: String,
    pub platform_family: String,
    pub platform_os: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortOption {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelServiceTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexModel {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    #[serde(default)]
    pub default_reasoning_effort: String,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub supports_personality: bool,
    #[serde(default)]
    pub service_tiers: Vec<ModelServiceTier>,
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
    #[serde(default)]
    pub default_service_tier: Option<String>,
    #[serde(default)]
    pub is_default: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// UI/integration-friendly name for a model catalog entry.
pub type ModelDescriptor = CodexModel;

impl CodexModel {
    pub fn wire_name(&self) -> &str {
        if self.model.trim().is_empty() {
            &self.id
        } else {
            &self.model
        }
    }

    pub fn supports_effort(&self, effort: &str) -> bool {
        self.supported_reasoning_efforts
            .iter()
            .any(|option| option.reasoning_effort == effort)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelListResponse {
    #[serde(default)]
    pub data: Vec<CodexModel>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EffectiveCodexConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_provider: Option<String>,
    #[serde(default)]
    pub model_reasoning_effort: Option<String>,
    #[serde(default)]
    pub model_reasoning_summary: Option<String>,
    #[serde(default)]
    pub model_verbosity: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub web_search: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<String>,
    #[serde(default)]
    pub sandbox_workspace_write: Option<WorkspaceWriteConfig>,
    /// Not currently emitted by every Codex release, but retained when it is.
    #[serde(default)]
    pub personality: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct WorkspaceWriteConfig {
    #[serde(default)]
    pub writable_roots: Vec<String>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReadResponse {
    #[serde(default)]
    pub config: EffectiveCodexConfig,
    #[serde(default)]
    pub origins: BTreeMap<String, Value>,
    #[serde(default)]
    pub layers: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CollaborationModeMask {
    pub name: String,
    #[serde(default)]
    pub mode: Option<TurnMode>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CollaborationModeListResponse {
    #[serde(default)]
    pub data: Vec<CollaborationModeMask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCapabilities {
    #[serde(default)]
    pub namespace_tools: bool,
    #[serde(default)]
    pub image_generation: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeCapabilities {
    pub app_server: bool,
    pub native_plan: bool,
    pub images: bool,
    pub personality: bool,
    pub service_tier: bool,
    pub reasoning_summary: bool,
    /// True only when the selected protocol can accept a per-turn verbosity
    /// override.  The field is still represented in config snapshots on older
    /// runtimes, where it remains an inherited local value.
    pub verbosity: bool,
    pub web_search: bool,
    pub sandbox: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeIdentity {
    pub executable: String,
    pub launcher: String,
    pub source: RuntimeSource,
    pub entrypoint: RuntimeEntrypoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub codex_home: String,
    pub platform_family: String,
    pub platform_os: String,
    pub user_agent: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub config_version: u64,
    pub runtime: RuntimeIdentity,
    pub capabilities: RuntimeCapabilities,
    pub models: Vec<CodexModel>,
    pub config: ConfigReadResponse,
    #[serde(default)]
    pub collaboration_modes: Vec<CollaborationModeMask>,
    #[serde(default)]
    pub provider_capabilities: ProviderCapabilities,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub refreshed_at_ms: u64,
}

impl RuntimeSnapshot {
    pub fn default_model(&self) -> Option<&CodexModel> {
        self.models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.models.first())
    }

    pub fn find_model(&self, name: &str) -> Option<&CodexModel> {
        self.models
            .iter()
            .find(|model| model.id == name || model.model == name)
    }

    pub fn plan_preset(&self) -> Option<&CollaborationModeMask> {
        self.collaboration_modes.iter().find(|preset| {
            preset.mode == Some(TurnMode::Plan) || preset.name.eq_ignore_ascii_case("plan")
        })
    }
}

fn infer_runtime_capabilities(
    models: &[CodexModel],
    collaboration_modes: &[CollaborationModeMask],
    provider: &ProviderCapabilities,
) -> RuntimeCapabilities {
    RuntimeCapabilities {
        app_server: true,
        native_plan: collaboration_modes.iter().any(|preset| {
            preset.mode == Some(TurnMode::Plan) || preset.name.eq_ignore_ascii_case("plan")
        }),
        images: models.iter().any(|model| {
            model
                .input_modalities
                .iter()
                .any(|modality| modality.eq_ignore_ascii_case("image"))
        }),
        personality: models.iter().any(|model| model.supports_personality),
        service_tier: models.iter().any(|model| {
            !model.service_tiers.is_empty() || !model.additional_speed_tiers.is_empty()
        }),
        reasoning_summary: true,
        // Codex 0.142 exposes model_verbosity in config/read but not yet in the
        // stable/experimental turn/start schema.  Never advertise an override
        // until the snapshot probe can prove it is accepted.
        verbosity: false,
        web_search: provider.web_search,
        sandbox: true,
    }
}

// ---------------------------------------------------------------------------
// Single-source turn configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TurnMode {
    Plan,
    #[default]
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigValueSource {
    SessionOverride,
    ProfileOverride,
    LocalCodex,
    PlanPreset,
    ModelCatalog,
    ForcedPlanPolicy,
    ServerReroute,
    Unset,
}

impl Default for ConfigValueSource {
    fn default() -> Self {
        Self::Unset
    }
}

/// Wire-compatible sandbox policy accepted by app-server turn/start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly {
        #[serde(rename = "networkAccess")]
        network_access: bool,
    },
    ExternalSandbox {
        #[serde(rename = "networkAccess")]
        network_access: String,
    },
    WorkspaceWrite {
        #[serde(rename = "writableRoots", default)]
        writable_roots: Vec<String>,
        #[serde(rename = "networkAccess")]
        network_access: bool,
        #[serde(rename = "excludeTmpdirEnvVar", default)]
        exclude_tmpdir_env_var: bool,
        #[serde(rename = "excludeSlashTmp", default)]
        exclude_slash_tmp: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModeTurnOverrides {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tier: Option<String>,
    #[serde(default)]
    pub personality: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub verbosity: Option<String>,
    #[serde(default)]
    pub web_search: Option<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CodexOverrideSet {
    #[serde(default)]
    pub normal: ModeTurnOverrides,
    #[serde(default)]
    pub plan: ModeTurnOverrides,
}

impl CodexOverrideSet {
    pub fn for_mode(&self, mode: TurnMode) -> &ModeTurnOverrides {
        match mode {
            TurnMode::Default => &self.normal,
            TurnMode::Plan => &self.plan,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnConfigResolutionInput {
    pub snapshot: RuntimeSnapshot,
    pub mode: TurnMode,
    #[serde(default)]
    pub profile: CodexOverrideSet,
    #[serde(default)]
    pub session: CodexOverrideSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedTurnConfig {
    pub config_version: u64,
    pub runtime_path: String,
    pub runtime_version: Option<String>,
    pub codex_home: String,
    pub mode: TurnMode,
    pub requested_model: Option<String>,
    pub effective_model: Option<String>,
    pub requested_effort: Option<String>,
    pub effective_effort: Option<String>,
    pub service_tier: Option<String>,
    pub personality: Option<String>,
    pub summary: Option<String>,
    pub verbosity: Option<String>,
    pub web_search: Option<String>,
    pub sandbox: Option<SandboxPolicy>,
    #[serde(default)]
    pub sources: BTreeMap<String, ConfigValueSource>,
    #[serde(default)]
    pub effective_sources: BTreeMap<String, ConfigValueSource>,
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Blocking validation failures.  They are intentionally separate from
    /// warnings so send paths cannot accidentally ignore an invalid explicit
    /// model/effort combination.
    #[serde(default)]
    pub validation_errors: Vec<String>,
}

impl ResolvedTurnConfig {
    /// Apply an app-server model/rerouted notification without overwriting the
    /// requested value persisted for audit/UI purposes.
    pub fn apply_model_reroute(&mut self, to_model: impl Into<String>, reason: &str) {
        self.apply_model_reroute_with_snapshot(to_model, reason, None);
    }

    pub fn apply_model_reroute_with_snapshot(
        &mut self,
        to_model: impl Into<String>,
        reason: &str,
        snapshot: Option<&RuntimeSnapshot>,
    ) {
        let to_model = to_model.into();
        let from_model = self
            .effective_model
            .clone()
            .unwrap_or_else(|| "<inherited>".into());
        self.effective_model = Some(to_model.clone());
        self.effective_sources
            .insert("model".into(), ConfigValueSource::ServerReroute);
        self.warnings.push(format!(
            "Codex rerouted model {from_model} -> {to_model}: {reason}"
        ));
        let Some(model) = snapshot.and_then(|snapshot| snapshot.find_model(&to_model)) else {
            if snapshot.is_some() {
                self.warnings.push(
                    "The rerouted model is not in model/list; effort, service tier and personality remain unverified."
                        .into(),
                );
            }
            return;
        };
        if self.effective_effort.as_ref().is_some_and(|effort| {
            !model.supported_reasoning_efforts.is_empty() && !model.supports_effort(effort)
        }) {
            self.warnings.push(format!(
                "The server did not verify that reasoning effort '{}' survived reroute to '{}'; actual effort is unknown.",
                self.effective_effort.as_deref().unwrap_or_default(),
                model.wire_name()
            ));
            self.effective_effort = None;
            self.effective_sources
                .insert("reasoning_effort".into(), ConfigValueSource::Unset);
        }
        if self.personality.is_some() && !model.supports_personality {
            self.warnings.push(format!(
                "The server did not verify personality after reroute to '{}'; actual personality is unknown.",
                model.wire_name()
            ));
            self.personality = None;
            self.effective_sources
                .insert("personality".into(), ConfigValueSource::Unset);
        }
        if let Some(tier) = self.service_tier.as_deref() {
            let advertised = model.service_tiers.iter().any(|value| value.id == tier)
                || model
                    .additional_speed_tiers
                    .iter()
                    .any(|value| value == tier);
            if !advertised
                && (!model.service_tiers.is_empty() || !model.additional_speed_tiers.is_empty())
            {
                self.warnings.push(format!(
                    "The server did not verify service tier '{tier}' after reroute to '{}'; actual tier is unknown.",
                    model.wire_name()
                ));
                self.service_tier = None;
                self.effective_sources
                    .insert("service_tier".into(), ConfigValueSource::Unset);
            }
        }
    }

    pub fn assert_snapshot_version(&self, current: &RuntimeSnapshot) -> Result<(), StaleConfig> {
        if self.config_version == current.config_version {
            Ok(())
        } else {
            Err(StaleConfig {
                expected: self.config_version,
                actual: current.config_version,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StaleConfig {
    pub expected: u64,
    pub actual: u64,
}

impl fmt::Display for StaleConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Codex configuration changed (expected version {}, current version {}); refresh and confirm before sending",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for StaleConfig {}

fn clean_string(value: Option<&String>) -> Option<String> {
    value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn choose_string(
    candidates: impl IntoIterator<Item = (Option<String>, ConfigValueSource)>,
) -> (Option<String>, ConfigValueSource) {
    candidates
        .into_iter()
        .find_map(|(value, source)| {
            value
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(|value| (Some(value), source))
        })
        .unwrap_or((None, ConfigValueSource::Unset))
}

fn choose_value<T: Clone>(
    candidates: impl IntoIterator<Item = (Option<T>, ConfigValueSource)>,
) -> (Option<T>, ConfigValueSource) {
    candidates
        .into_iter()
        .find_map(|(value, source)| value.map(|value| (Some(value), source)))
        .unwrap_or((None, ConfigValueSource::Unset))
}

fn sandbox_from_config(config: &EffectiveCodexConfig) -> Option<SandboxPolicy> {
    match config
        .sandbox_mode
        .as_ref()
        .map(|mode| mode.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("read-only") | Some("readonly") | Some("read_only") => Some(SandboxPolicy::ReadOnly {
            network_access: false,
        }),
        Some("workspace-write") | Some("workspacewrite") | Some("workspace_write") => {
            let details = config.sandbox_workspace_write.clone().unwrap_or_default();
            Some(SandboxPolicy::WorkspaceWrite {
                writable_roots: details.writable_roots,
                network_access: details.network_access,
                exclude_tmpdir_env_var: details.exclude_tmpdir_env_var,
                exclude_slash_tmp: details.exclude_slash_tmp,
            })
        }
        Some("danger-full-access") | Some("dangerfullaccess") | Some("danger_full_access") => {
            Some(SandboxPolicy::DangerFullAccess)
        }
        _ => None,
    }
}

/// Resolve all layers with one deterministic priority function.
///
/// Priority is session -> profile -> local Codex/Plan preset -> catalog.  Plan
/// inherits the local model but takes reasoning effort from the native Plan
/// preset before the local default.  A known model rejects unsupported efforts
/// by continuing to the next inherited candidate; a custom model/effort pair is
/// passed through so app-server can accept or report it without silent changes.
pub fn resolve_turn_config(input: &TurnConfigResolutionInput) -> ResolvedTurnConfig {
    let snapshot = &input.snapshot;
    let profile = input.profile.for_mode(input.mode);
    let session = input.session.for_mode(input.mode);
    let local = &snapshot.config.config;
    let plan_preset = (input.mode == TurnMode::Plan)
        .then(|| snapshot.plan_preset())
        .flatten();
    let catalog_default = snapshot.default_model();

    let (model, model_source) = choose_string([
        (
            clean_string(session.model.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.model.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.model.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
        (
            plan_preset.and_then(|preset| clean_string(preset.model.as_ref())),
            ConfigValueSource::PlanPreset,
        ),
        (
            catalog_default.map(|model| model.wire_name().to_string()),
            ConfigValueSource::ModelCatalog,
        ),
    ]);
    let known_model = model
        .as_deref()
        .and_then(|model| snapshot.find_model(model));

    let effort_candidates = [
        (
            clean_string(session.reasoning_effort.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.reasoning_effort.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            plan_preset.and_then(|preset| clean_string(preset.reasoning_effort.as_ref())),
            ConfigValueSource::PlanPreset,
        ),
        (
            clean_string(local.model_reasoning_effort.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
        (
            known_model
                .map(|model| model.default_reasoning_effort.trim().to_string())
                .filter(|effort| !effort.is_empty()),
            ConfigValueSource::ModelCatalog,
        ),
    ];
    let mut warnings = snapshot.warnings.clone();
    let mut validation_errors = Vec::new();
    let (requested_effort, effort_source) = effort_candidates
        .iter()
        .find_map(|(candidate, source)| candidate.clone().map(|value| (Some(value), *source)))
        .unwrap_or((None, ConfigValueSource::Unset));
    let mut effective_effort = None;
    let mut effective_effort_source = ConfigValueSource::Unset;
    for (candidate, source) in effort_candidates {
        let Some(candidate) = candidate else {
            continue;
        };
        if let Some(known_model) = known_model {
            if !known_model.supported_reasoning_efforts.is_empty()
                && !known_model.supports_effort(&candidate)
            {
                let message = format!(
                    "Reasoning effort '{candidate}' is not supported by known model '{}'",
                    known_model.wire_name()
                );
                if matches!(
                    source,
                    ConfigValueSource::SessionOverride | ConfigValueSource::ProfileOverride
                ) {
                    // Preserve exactly what UI/profile requested and make send
                    // fail.  Only an explicit UI reset to inherit may continue
                    // to lower-priority values.
                    effective_effort = Some(candidate);
                    effective_effort_source = source;
                    validation_errors.push(format!(
                        "{message}; reset reasoning effort to inherit before sending"
                    ));
                    break;
                } else {
                    warnings.push(format!("{message}; ignored the inherited value"));
                    continue;
                }
            }
        }
        effective_effort = Some(candidate);
        effective_effort_source = source;
        break;
    }

    // Never apply the catalog default's effort/tier to an explicitly selected
    // unknown provider model. Unknown values are intentionally passed through
    // (or left inherited) for Codex to validate.
    let selected_model = known_model;
    let (mut service_tier, service_tier_source) = choose_string([
        (
            clean_string(session.service_tier.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.service_tier.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.service_tier.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
        (
            selected_model.and_then(|model| clean_string(model.default_service_tier.as_ref())),
            ConfigValueSource::ModelCatalog,
        ),
    ]);
    let (mut personality, personality_source) = choose_string([
        (
            clean_string(session.personality.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.personality.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.personality.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
    ]);
    let (summary, summary_source) = choose_string([
        (
            clean_string(session.summary.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.summary.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.model_reasoning_summary.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
    ]);
    let (verbosity, verbosity_source) = choose_string([
        (
            clean_string(session.verbosity.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.verbosity.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.model_verbosity.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
    ]);
    let (web_search, web_search_source) = choose_string([
        (
            clean_string(session.web_search.as_ref()),
            ConfigValueSource::SessionOverride,
        ),
        (
            clean_string(profile.web_search.as_ref()),
            ConfigValueSource::ProfileOverride,
        ),
        (
            clean_string(local.web_search.as_ref()),
            ConfigValueSource::LocalCodex,
        ),
    ]);
    let (requested_sandbox, sandbox_source) = choose_value([
        (session.sandbox.clone(), ConfigValueSource::SessionOverride),
        (profile.sandbox.clone(), ConfigValueSource::ProfileOverride),
        (sandbox_from_config(local), ConfigValueSource::LocalCodex),
    ]);

    let (sandbox, effective_sandbox_source) = if input.mode == TurnMode::Plan {
        (
            Some(SandboxPolicy::ReadOnly {
                network_access: false,
            }),
            ConfigValueSource::ForcedPlanPolicy,
        )
    } else {
        (requested_sandbox, sandbox_source)
    };

    let mut effective_service_tier_source = service_tier_source;
    let mut effective_personality_source = personality_source;
    if let (Some(model), Some(_personality)) = (known_model, personality.as_ref()) {
        if !model.supports_personality {
            let message = format!(
                "Model '{}' does not advertise personality support",
                model.wire_name()
            );
            if matches!(
                personality_source,
                ConfigValueSource::ProfileOverride | ConfigValueSource::SessionOverride
            ) {
                validation_errors.push(format!(
                    "{message}; reset personality to inherit before sending"
                ));
            } else {
                warnings.push(format!("{message}; ignored the inherited value"));
                personality = None;
                effective_personality_source = ConfigValueSource::Unset;
            }
        }
    }
    if let (Some(model), Some(tier)) = (known_model, service_tier.as_ref()) {
        let advertised = model
            .service_tiers
            .iter()
            .any(|candidate| candidate.id == *tier)
            || model
                .additional_speed_tiers
                .iter()
                .any(|candidate| candidate == tier);
        if !advertised
            && (!model.service_tiers.is_empty() || !model.additional_speed_tiers.is_empty())
        {
            let message = format!(
                "Service tier '{tier}' is not advertised by model '{}'",
                model.wire_name()
            );
            if matches!(
                service_tier_source,
                ConfigValueSource::ProfileOverride | ConfigValueSource::SessionOverride
            ) {
                validation_errors.push(format!(
                    "{message}; reset service tier to inherit before sending"
                ));
            } else {
                warnings.push(format!("{message}; ignored the inherited value"));
                service_tier = None;
                effective_service_tier_source = ConfigValueSource::Unset;
            }
        }
    }

    let mut sources = BTreeMap::new();
    sources.insert("model".into(), model_source);
    sources.insert("reasoning_effort".into(), effort_source);
    sources.insert("service_tier".into(), service_tier_source);
    sources.insert("personality".into(), personality_source);
    sources.insert("summary".into(), summary_source);
    sources.insert("verbosity".into(), verbosity_source);
    sources.insert("web_search".into(), web_search_source);
    sources.insert("sandbox".into(), sandbox_source);
    let mut effective_sources = sources.clone();
    effective_sources.insert("reasoning_effort".into(), effective_effort_source);
    effective_sources.insert("sandbox".into(), effective_sandbox_source);
    effective_sources.insert("service_tier".into(), effective_service_tier_source);
    effective_sources.insert("personality".into(), effective_personality_source);

    if verbosity.is_some()
        && !snapshot.capabilities.verbosity
        && matches!(
            verbosity_source,
            ConfigValueSource::ProfileOverride | ConfigValueSource::SessionOverride
        )
    {
        validation_errors.push(
            "This Codex app-server does not advertise a per-turn verbosity override; reset verbosity to inherit before sending"
                .into(),
        );
    }
    ResolvedTurnConfig {
        config_version: snapshot.config_version,
        runtime_path: snapshot.runtime.executable.clone(),
        runtime_version: snapshot.runtime.version.clone(),
        codex_home: snapshot.runtime.codex_home.clone(),
        mode: input.mode,
        requested_model: model.clone(),
        effective_model: model,
        requested_effort,
        effective_effort,
        service_tier,
        personality,
        summary,
        verbosity,
        web_search,
        sandbox,
        sources,
        effective_sources,
        warnings,
        validation_errors,
    }
}

// ---------------------------------------------------------------------------
// turn/start wire construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum CodexUserInput {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default)]
        text_elements: Vec<Value>,
    },
    #[serde(rename = "image")]
    Image {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "localImage")]
    LocalImage {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    #[serde(rename = "skill")]
    Skill { name: String, path: String },
    #[serde(rename = "mention")]
    Mention { name: String, path: String },
}

impl CodexUserInput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationModeSettings {
    pub model: String,
    pub reasoning_effort: Option<String>,
    /// `None` asks Codex to use its built-in instructions for the selected mode.
    pub developer_instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollaborationModeSelection {
    pub mode: TurnMode,
    pub settings: CollaborationModeSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_user_message_id: Option<String>,
    pub input: Vec<CodexUserInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_policy: Option<SandboxPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collaboration_mode: Option<CollaborationModeSelection>,
    /// Forward-compatible app-server extension.  Older runtimes which expose
    /// verbosity only through config/read must reject an explicit override; the
    /// caller surfaces that RPC error rather than silently changing the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    /// Forward-compatible per-turn search selection.  It is omitted when the
    /// effective value is inherited from the selected CODEX_HOME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStartBuildError {
    MissingThreadId,
    EmptyInput,
    MissingPlanModel,
    InvalidConfiguration(Vec<String>),
}

impl fmt::Display for TurnStartBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingThreadId => f.write_str("turn/start requires a thread id"),
            Self::EmptyInput => f.write_str("turn/start requires at least one input item"),
            Self::MissingPlanModel => f.write_str(
                "native Plan mode requires a resolved model; refresh the runtime catalog/config",
            ),
            Self::InvalidConfiguration(errors) => {
                write!(
                    f,
                    "invalid resolved Codex configuration: {}",
                    errors.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for TurnStartBuildError {}

/// Construct `turn/start` only from the already-resolved config shown by UI.
/// No fallback or inference is performed in this serialization step.
pub fn build_turn_start_params(
    thread_id: impl Into<String>,
    input: Vec<CodexUserInput>,
    config: &ResolvedTurnConfig,
) -> Result<TurnStartParams, TurnStartBuildError> {
    let thread_id = thread_id.into();
    if thread_id.trim().is_empty() {
        return Err(TurnStartBuildError::MissingThreadId);
    }
    if input.is_empty() {
        return Err(TurnStartBuildError::EmptyInput);
    }
    if !config.validation_errors.is_empty() {
        return Err(TurnStartBuildError::InvalidConfiguration(
            config.validation_errors.clone(),
        ));
    }
    if config.mode == TurnMode::Plan && config.effective_model.is_none() {
        return Err(TurnStartBuildError::MissingPlanModel);
    }

    let collaboration_mode =
        config
            .effective_model
            .clone()
            .map(|model| CollaborationModeSelection {
                mode: config.mode,
                settings: CollaborationModeSettings {
                    model,
                    reasoning_effort: config.effective_effort.clone(),
                    developer_instructions: None,
                },
            });

    Ok(TurnStartParams {
        thread_id,
        client_user_message_id: None,
        input,
        cwd: None,
        model: config.effective_model.clone(),
        service_tier: config.service_tier.clone(),
        effort: config.effective_effort.clone(),
        summary: config.summary.clone(),
        personality: config.personality.clone(),
        sandbox_policy: config.sandbox.clone(),
        collaboration_mode,
        // Codex 0.142/0.144 expose these in config/read but not turn/start.
        // Profile values are applied to the actor with `-c`; session values
        // are rejected by the resolver. Never send fields the protocol would
        // silently ignore.
        verbosity: None,
        web_search: None,
    })
}

// ---------------------------------------------------------------------------
// JSON-lines RPC actor/client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Number(i64),
    String(String),
}

impl fmt::Display for RpcId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => value.fmt(f),
            Self::String(value) => value.fmt(f),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedJsonRpcLine {
    Response {
        id: RpcId,
        result: Result<Value, RpcErrorObject>,
    },
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonLineParseError(pub String);

impl fmt::Display for JsonLineParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonLineParseError {}

/// Parse one app-server stdout JSON line without depending on a running Codex.
pub fn parse_json_rpc_line(line: &str) -> Result<ParsedJsonRpcLine, JsonLineParseError> {
    let value: Value = serde_json::from_str(line)
        .map_err(|error| JsonLineParseError(format!("invalid JSON-RPC line: {error}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| JsonLineParseError("JSON-RPC line must be an object".into()))?;
    let id = object
        .get("id")
        .cloned()
        .map(serde_json::from_value::<RpcId>)
        .transpose()
        .map_err(|error| JsonLineParseError(format!("invalid JSON-RPC id: {error}")))?;
    let method = object.get("method").and_then(Value::as_str);

    match (id, method) {
        (Some(id), Some(method)) => Ok(ParsedJsonRpcLine::Request {
            id,
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(method)) => Ok(ParsedJsonRpcLine::Notification {
            method: method.to_string(),
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(id), None) => {
            if let Some(error) = object.get("error") {
                let error =
                    serde_json::from_value::<RpcErrorObject>(error.clone()).map_err(|source| {
                        JsonLineParseError(format!("invalid JSON-RPC error object: {source}"))
                    })?;
                Ok(ParsedJsonRpcLine::Response {
                    id,
                    result: Err(error),
                })
            } else if let Some(result) = object.get("result") {
                Ok(ParsedJsonRpcLine::Response {
                    id,
                    result: Ok(result.clone()),
                })
            } else {
                Err(JsonLineParseError(
                    "JSON-RPC response must contain result or error".into(),
                ))
            }
        }
        (None, None) => Err(JsonLineParseError(
            "JSON-RPC object must contain method or id".into(),
        )),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AppServerClientError {
    Io(String),
    Process(String),
    Protocol(String),
    Rpc(RpcErrorObject),
    Timeout { method: String, timeout_ms: u64 },
    ActorClosed,
    Serialization(String),
}

impl fmt::Display for AppServerClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "app-server I/O error: {message}"),
            Self::Process(message) => write!(f, "app-server process error: {message}"),
            Self::Protocol(message) => write!(f, "app-server protocol error: {message}"),
            Self::Rpc(error) => write!(f, "app-server RPC error {}: {}", error.code, error.message),
            Self::Timeout { method, timeout_ms } => {
                write!(
                    f,
                    "app-server request '{method}' timed out after {timeout_ms}ms"
                )
            }
            Self::ActorClosed => f.write_str("app-server actor is closed"),
            Self::Serialization(message) => {
                write!(f, "app-server serialization error: {message}")
            }
        }
    }
}

impl std::error::Error for AppServerClientError {}

#[derive(Debug, Clone)]
pub struct AppServerSpawnOptions {
    pub cwd: Option<PathBuf>,
    pub client_name: String,
    pub client_title: String,
    pub client_version: String,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub probe_version: bool,
}

impl Default for AppServerSpawnOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            client_name: "wisp-science".into(),
            client_title: "Wisp Science".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            probe_version: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppServerTransportEvent {
    Notification {
        method: String,
        params: Value,
        event: TurnEvent,
    },
    ServerRequest {
        id: RpcId,
        method: String,
        params: Value,
        event: TurnEvent,
    },
    Stderr {
        line: String,
    },
    ProtocolError {
        line: String,
        error: String,
    },
    Exited {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
}

enum ActorCommand {
    Request {
        id: RpcId,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value, AppServerClientError>>,
    },
    Notify {
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<(), AppServerClientError>>,
    },
    Respond {
        id: RpcId,
        result: Result<Value, RpcErrorObject>,
        reply: oneshot::Sender<Result<(), AppServerClientError>>,
    },
    CancelPending {
        id: RpcId,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), AppServerClientError>>,
    },
}

struct ClientInner {
    actor_id: u64,
    commands: mpsc::Sender<ActorCommand>,
    events: broadcast::Sender<AppServerTransportEvent>,
    next_id: AtomicU64,
    command: ResolvedCodexCommand,
    runtime_version: Option<String>,
    version_probe_error: Option<String>,
    initialize: InitializeResponse,
    request_timeout: Duration,
}

/// Cloneable handle to the single process-owning JSON-RPC actor.
#[derive(Clone)]
pub struct CodexAppServerClient {
    inner: Arc<ClientInner>,
}

impl fmt::Debug for CodexAppServerClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexAppServerClient")
            .field("command", &self.inner.command)
            .field("runtime_version", &self.inner.runtime_version)
            .field("initialize", &self.inner.initialize)
            .finish_non_exhaustive()
    }
}

impl CodexAppServerClient {
    /// Spawn the selected command as `app-server --stdio`, start stdout/stderr
    /// readers, then negotiate experimentalApi before returning the client.
    pub async fn spawn(
        command: ResolvedCodexCommand,
        options: AppServerSpawnOptions,
    ) -> Result<Self, AppServerClientError> {
        let (runtime_version, version_probe_error) = if options.probe_version {
            match probe_runtime_version(&command).await {
                Ok(version) => (Some(version), None),
                Err(error) => (None, Some(error.to_string())),
            }
        } else {
            (None, None)
        };
        let mut process = command.process_for(&["app-server", "--stdio"]);
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = &options.cwd {
            process.current_dir(cwd);
        }
        let mut child = process
            .spawn()
            .map_err(|error| AppServerClientError::Io(error.to_string()))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            AppServerClientError::Process("failed to open app-server stdin".into())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            AppServerClientError::Process("failed to open app-server stdout".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            AppServerClientError::Process("failed to open app-server stderr".into())
        })?;

        let (command_tx, command_rx) = mpsc::channel(128);
        let (event_tx, _) = broadcast::channel(512);
        tokio::spawn(run_app_server_actor(
            child,
            stdin,
            BufReader::new(stdout),
            BufReader::new(stderr),
            command_rx,
            event_tx.clone(),
            options.shutdown_timeout,
        ));

        // Install a temporary initialize value so the ordinary concurrent
        // request path itself is exercised during negotiation.
        let provisional = InitializeResponse {
            user_agent: String::new(),
            codex_home: command.codex_home.clone().unwrap_or_default(),
            platform_family: String::new(),
            platform_os: String::new(),
        };
        let mut client = Self {
            inner: Arc::new(ClientInner {
                actor_id: NEXT_ACTOR_ID.fetch_add(1, Ordering::Relaxed),
                commands: command_tx,
                events: event_tx,
                next_id: AtomicU64::new(1),
                command: command.clone(),
                runtime_version,
                version_probe_error,
                initialize: provisional,
                request_timeout: options.request_timeout,
            }),
        };
        let initialize: InitializeResponse = client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": options.client_name,
                        "title": options.client_title,
                        "version": options.client_version,
                    },
                    "capabilities": {
                        "experimentalApi": true,
                    }
                }),
            )
            .await
            .map_err(|error| {
                // The actor owns/cleans the child even when initialize fails.
                error
            })?;
        Arc::get_mut(&mut client.inner)
            .expect("new app-server client has no clones")
            .initialize = initialize;
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    pub fn runtime_command(&self) -> &ResolvedCodexCommand {
        &self.inner.command
    }

    pub fn actor_id(&self) -> u64 {
        self.inner.actor_id
    }

    pub fn runtime_version(&self) -> Option<&str> {
        self.inner.runtime_version.as_deref()
    }

    pub fn initialize_response(&self) -> &InitializeResponse {
        &self.inner.initialize
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppServerTransportEvent> {
        self.inner.events.subscribe()
    }

    pub async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Result<T, AppServerClientError> {
        let value = serde_json::to_value(params)
            .map_err(|error| AppServerClientError::Serialization(error.to_string()))?;
        let value = self.request_value(method.into(), value).await?;
        serde_json::from_value(value)
            .map_err(|error| AppServerClientError::Serialization(error.to_string()))
    }

    pub async fn request_value(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value, AppServerClientError> {
        let method = method.into();
        let timeout = if matches!(
            method.as_str(),
            "turn/start" | "thread/start" | "thread/resume" | "turn/interrupt"
        ) {
            self.inner.request_timeout.max(MUTATING_REQUEST_TIMEOUT)
        } else {
            self.inner.request_timeout
        };
        let next = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let id = RpcId::Number((next.min(i64::MAX as u64)) as i64);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(ActorCommand::Request {
                id: id.clone(),
                method: method.clone(),
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?;
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AppServerClientError::ActorClosed),
            Err(_) => {
                let _ = self
                    .inner
                    .commands
                    .send(ActorCommand::CancelPending { id })
                    .await;
                // A late response to a mutating request is an unknown-outcome
                // operation. Retire/kill the owning actor so an orphan turn
                // cannot continue modifying the project after Wisp reports a
                // timeout and the user retries.
                if matches!(
                    method.as_str(),
                    "turn/start" | "thread/start" | "thread/resume" | "turn/interrupt"
                ) {
                    let _ = self.shutdown().await;
                }
                Err(AppServerClientError::Timeout {
                    method,
                    timeout_ms: timeout.as_millis() as u64,
                })
            }
        }
    }

    pub async fn notify(
        &self,
        method: impl Into<String>,
        params: impl Serialize,
    ) -> Result<(), AppServerClientError> {
        let params = serde_json::to_value(params)
            .map_err(|error| AppServerClientError::Serialization(error.to_string()))?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(ActorCommand::Notify {
                method: method.into(),
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?;
        reply_rx
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?
    }

    pub async fn respond_result(
        &self,
        id: RpcId,
        result: impl Serialize,
    ) -> Result<(), AppServerClientError> {
        let result = serde_json::to_value(result)
            .map_err(|error| AppServerClientError::Serialization(error.to_string()))?;
        self.respond(id, Ok(result)).await
    }

    pub async fn respond_error(
        &self,
        id: RpcId,
        error: RpcErrorObject,
    ) -> Result<(), AppServerClientError> {
        self.respond(id, Err(error)).await
    }

    async fn respond(
        &self,
        id: RpcId,
        result: Result<Value, RpcErrorObject>,
    ) -> Result<(), AppServerClientError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(ActorCommand::Respond {
                id,
                result,
                reply: reply_tx,
            })
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?;
        reply_rx
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?
    }

    /// Close stdin, give Codex time to exit, then kill only if necessary.
    pub async fn shutdown(&self) -> Result<(), AppServerClientError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.inner
            .commands
            .send(ActorCommand::Shutdown { reply: reply_tx })
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?;
        reply_rx
            .await
            .map_err(|_| AppServerClientError::ActorClosed)?
    }
}

async fn write_json_line<W: tokio::io::AsyncWrite + Unpin>(
    stdin: &mut W,
    value: &Value,
) -> Result<(), AppServerClientError> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|error| AppServerClientError::Serialization(error.to_string()))?;
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| AppServerClientError::Io(error.to_string()))?;
    stdin
        .flush()
        .await
        .map_err(|error| AppServerClientError::Io(error.to_string()))
}

async fn run_app_server_actor<R, E>(
    mut child: Child,
    mut stdin: ChildStdin,
    stdout: BufReader<R>,
    stderr: BufReader<E>,
    mut commands: mpsc::Receiver<ActorCommand>,
    events: broadcast::Sender<AppServerTransportEvent>,
    shutdown_timeout: Duration,
) where
    R: tokio::io::AsyncRead + Unpin,
    E: tokio::io::AsyncRead + Unpin,
{
    // `Lines::next_line` is cancellation safe inside select!, unlike
    // `AsyncBufReadExt::read_line`; concurrent outgoing requests therefore
    // cannot discard a partially received JSON frame.
    let mut stdout = stdout.lines();
    let mut stderr = stderr.lines();
    let mut stderr_closed = false;
    let mut pending: HashMap<RpcId, oneshot::Sender<Result<Value, AppServerClientError>>> =
        HashMap::new();

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ActorCommand::Request { id, method, params, reply }) => {
                        let message = json!({"id": id, "method": method, "params": params});
                        match write_json_line(&mut stdin, &message).await {
                            Ok(()) => { pending.insert(id, reply); }
                            Err(error) => { let _ = reply.send(Err(error)); }
                        }
                    }
                    Some(ActorCommand::Notify { method, params, reply }) => {
                        let message = json!({"method": method, "params": params});
                        let _ = reply.send(write_json_line(&mut stdin, &message).await);
                    }
                    Some(ActorCommand::Respond { id, result, reply }) => {
                        let message = match result {
                            Ok(result) => json!({"id": id, "result": result}),
                            Err(error) => json!({"id": id, "error": error}),
                        };
                        let _ = reply.send(write_json_line(&mut stdin, &message).await);
                    }
                    Some(ActorCommand::CancelPending { id }) => {
                        pending.remove(&id);
                    }
                    Some(ActorCommand::Shutdown { reply }) => {
                        let result = graceful_shutdown(&mut child, &mut stdin, shutdown_timeout).await;
                        fail_pending(&mut pending, AppServerClientError::ActorClosed);
                        let _ = reply.send(result);
                        break;
                    }
                    None => {
                        let _ = graceful_shutdown(&mut child, &mut stdin, shutdown_timeout).await;
                        fail_pending(&mut pending, AppServerClientError::ActorClosed);
                        break;
                    }
                }
            }
            read = stdout.next_line() => {
                match read {
                    Ok(None) => {
                        fail_pending(&mut pending, AppServerClientError::Process("app-server stdout closed".into()));
                        break;
                    }
                    Ok(Some(line)) => {
                        match parse_json_rpc_line(&line) {
                            Ok(ParsedJsonRpcLine::Response { id, result }) => {
                                if let Some(reply) = pending.remove(&id) {
                                    let _ = reply.send(result.map_err(AppServerClientError::Rpc));
                                } else {
                                    let _ = events.send(AppServerTransportEvent::ProtocolError {
                                        line: line.clone(),
                                        error: format!("response for unknown or timed-out request id {id}"),
                                    });
                                }
                            }
                            Ok(ParsedJsonRpcLine::Request { id, method, params }) => {
                                let event = parse_turn_event(&method, &params, Some(id.clone()));
                                let _ = events.send(AppServerTransportEvent::ServerRequest {
                                    id, method, params, event,
                                });
                            }
                            Ok(ParsedJsonRpcLine::Notification { method, params }) => {
                                let event = parse_turn_event(&method, &params, None);
                                let _ = events.send(AppServerTransportEvent::Notification {
                                    method, params, event,
                                });
                            }
                            Err(error) => {
                                let _ = events.send(AppServerTransportEvent::ProtocolError {
                                    line,
                                    error: error.to_string(),
                                });
                            }
                        }
                    }
                    Err(error) => {
                        fail_pending(&mut pending, AppServerClientError::Io(error.to_string()));
                        break;
                    }
                }
            }
            read = stderr.next_line(), if !stderr_closed => {
                match read {
                    Ok(None) => { stderr_closed = true; /* stdout remains authoritative. */ }
                    Ok(Some(line)) => {
                        let _ = events.send(AppServerTransportEvent::Stderr {
                            line,
                        });
                    }
                    Err(error) => {
                        let _ = events.send(AppServerTransportEvent::ProtocolError {
                            line: String::new(),
                            error: format!("failed reading app-server stderr: {error}"),
                        });
                    }
                }
            }
            status = child.wait() => {
                let (code, signal) = match status {
                    Ok(status) => (status.code(), status.code().is_none().then(|| status.to_string())),
                    Err(error) => {
                        let _ = events.send(AppServerTransportEvent::ProtocolError {
                            line: String::new(),
                            error: format!("failed waiting for app-server: {error}"),
                        });
                        (None, None)
                    }
                };
                let _ = events.send(AppServerTransportEvent::Exited { code, signal });
                fail_pending(&mut pending, AppServerClientError::Process("app-server exited".into()));
                break;
            }
        }
    }
}

fn fail_pending(
    pending: &mut HashMap<RpcId, oneshot::Sender<Result<Value, AppServerClientError>>>,
    error: AppServerClientError,
) {
    for (_, reply) in pending.drain() {
        let _ = reply.send(Err(error.clone()));
    }
}

async fn graceful_shutdown(
    child: &mut Child,
    stdin: &mut ChildStdin,
    timeout: Duration,
) -> Result<(), AppServerClientError> {
    let _ = stdin.shutdown().await;
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(AppServerClientError::Io(error.to_string())),
        Err(_) => {
            child
                .kill()
                .await
                .map_err(|error| AppServerClientError::Io(error.to_string()))?;
            child
                .wait()
                .await
                .map_err(|error| AppServerClientError::Io(error.to_string()))?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Turn/Plan event decoding
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestUserInputOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestUserInputQuestion {
    pub id: String,
    #[serde(default)]
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub is_other: bool,
    #[serde(default)]
    pub is_secret: bool,
    #[serde(default)]
    pub options: Option<Vec<RequestUserInputOption>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    #[serde(default)]
    pub total: TokenUsageBreakdown,
    #[serde(default)]
    pub last: TokenUsageBreakdown,
    #[serde(default)]
    pub model_context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnEvent {
    PlanDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
    },
    FinalPlan {
        thread_id: String,
        turn_id: String,
        item_id: String,
        text: String,
    },
    PlanUpdated {
        thread_id: String,
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explanation: Option<String>,
        plan: Vec<PlanStep>,
    },
    RequestUserInput {
        request_id: RpcId,
        thread_id: String,
        turn_id: String,
        item_id: String,
        questions: Vec<RequestUserInputQuestion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auto_resolution_ms: Option<u64>,
    },
    ModelRerouted {
        thread_id: String,
        turn_id: String,
        from_model: String,
        to_model: String,
        reason: String,
    },
    Usage {
        thread_id: String,
        turn_id: String,
        token_usage: ThreadTokenUsage,
    },
    Error {
        thread_id: String,
        turn_id: String,
        message: String,
        will_retry: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        codex_error_info: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        additional_details: Option<String>,
    },
    /// Announces an agent-message item's semantic phase before its deltas.
    /// Current Codex runtimes use `commentary` for progress/reasoning and
    /// `final_answer` for the user-visible answer.  Keeping this item event is
    /// necessary because delta notifications do not always repeat `phase`.
    AgentMessageStarted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    AgentMessageDelta {
        thread_id: String,
        turn_id: String,
        item_id: String,
        delta: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    AgentMessageCompleted {
        thread_id: String,
        turn_id: String,
        item_id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
    },
    ToolCall {
        thread_id: String,
        turn_id: String,
        item_id: String,
        tool_type: String,
        name: String,
        arguments: Value,
        item: Value,
    },
    ToolResult {
        thread_id: String,
        turn_id: String,
        item_id: String,
        tool_type: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        output: Value,
        item: Value,
    },
    Diff {
        thread_id: String,
        turn_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        phase: String,
        diff: Value,
    },
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<Value>,
    },
    Unknown {
        method: String,
        params: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<RpcId>,
    },
}

fn required_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key)?.as_str().map(str::to_owned)
}

fn unknown_event(method: &str, params: &Value, request_id: Option<RpcId>) -> TurnEvent {
    TurnEvent::Unknown {
        method: method.to_string(),
        params: params.clone(),
        request_id,
    }
}

fn tool_descriptor(item: &Map<String, Value>) -> Option<(String, String, Value)> {
    let tool_type = item.get("type")?.as_str()?.to_string();
    match tool_type.as_str() {
        "commandExecution" => {
            let command = item.get("command")?.as_str()?.to_string();
            Some((
                tool_type,
                command.clone(),
                json!({
                    "command": command,
                    "cwd": item.get("cwd").cloned().unwrap_or(Value::Null),
                    "source": item.get("source").cloned().unwrap_or(Value::Null),
                }),
            ))
        }
        "mcpToolCall" => {
            let server = item.get("server")?.as_str()?.to_string();
            let tool = item.get("tool")?.as_str()?.to_string();
            Some((
                tool_type,
                format!("{server}/{tool}"),
                item.get("arguments").cloned().unwrap_or(Value::Null),
            ))
        }
        "dynamicToolCall" => {
            let tool = item.get("tool")?.as_str()?.to_string();
            let name = item
                .get("namespace")
                .and_then(Value::as_str)
                .map(|namespace| format!("{namespace}/{tool}"))
                .unwrap_or(tool);
            Some((
                tool_type,
                name,
                item.get("arguments").cloned().unwrap_or(Value::Null),
            ))
        }
        _ => None,
    }
}

fn item_lifecycle_event(
    method: &str,
    params: &Value,
    request_id: Option<RpcId>,
    completed: bool,
) -> TurnEvent {
    let Some(object) = params.as_object() else {
        return unknown_event(method, params, request_id);
    };
    let Some(item) = object.get("item").and_then(Value::as_object) else {
        return unknown_event(method, params, request_id);
    };
    let Some(thread_id) = required_string(object, "threadId") else {
        return unknown_event(method, params, request_id);
    };
    let Some(turn_id) = required_string(object, "turnId") else {
        return unknown_event(method, params, request_id);
    };
    let Some(item_id) = required_string(item, "id") else {
        return unknown_event(method, params, request_id);
    };
    if item.get("type").and_then(Value::as_str) == Some("fileChange") {
        return TurnEvent::Diff {
            thread_id,
            turn_id,
            item_id: Some(item_id),
            phase: if completed { "completed" } else { "started" }.into(),
            diff: item
                .get("changes")
                .cloned()
                .unwrap_or_else(|| Value::Object(item.clone())),
        };
    }
    let Some((tool_type, name, arguments)) = tool_descriptor(item) else {
        return unknown_event(method, params, request_id);
    };
    if completed {
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let success = item.get("success").and_then(Value::as_bool).or_else(|| {
            status.as_deref().and_then(|status| match status {
                "completed" | "success" => Some(true),
                "failed" | "declined" | "cancelled" => Some(false),
                _ => None,
            })
        });
        let output = item
            .get("result")
            .or_else(|| item.get("aggregatedOutput"))
            .or_else(|| item.get("contentItems"))
            .or_else(|| item.get("error"))
            .cloned()
            .unwrap_or(Value::Null);
        TurnEvent::ToolResult {
            thread_id,
            turn_id,
            item_id,
            tool_type,
            name,
            status,
            success,
            output,
            item: Value::Object(item.clone()),
        }
    } else {
        TurnEvent::ToolCall {
            thread_id,
            turn_id,
            item_id,
            tool_type,
            name,
            arguments,
            item: Value::Object(item.clone()),
        }
    }
}

/// Decode both server notifications and server-initiated requests.  Unknown or
/// newly-added events are preserved losslessly instead of being discarded.
pub fn parse_turn_event(method: &str, params: &Value, request_id: Option<RpcId>) -> TurnEvent {
    let Some(object) = params.as_object() else {
        return unknown_event(method, params, request_id);
    };
    match method {
        "item/plan/delta" => {
            let Some((thread_id, turn_id, item_id, delta)) = required_string(object, "threadId")
                .zip(required_string(object, "turnId"))
                .zip(required_string(object, "itemId"))
                .zip(required_string(object, "delta"))
                .map(|(((thread_id, turn_id), item_id), delta)| {
                    (thread_id, turn_id, item_id, delta)
                })
            else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::PlanDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
            }
        }
        "turn/plan/updated" => {
            let Some(thread_id) = required_string(object, "threadId") else {
                return unknown_event(method, params, request_id);
            };
            let Some(turn_id) = required_string(object, "turnId") else {
                return unknown_event(method, params, request_id);
            };
            let plan = object
                .get("plan")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<PlanStep>>(value).ok());
            let Some(plan) = plan else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::PlanUpdated {
                thread_id,
                turn_id,
                explanation: object
                    .get("explanation")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                plan,
            }
        }
        "item/started" => {
            let Some(item) = object.get("item").and_then(Value::as_object) else {
                return unknown_event(method, params, request_id);
            };
            if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                let Some((thread_id, turn_id, item_id)) = required_string(object, "threadId")
                    .zip(required_string(object, "turnId"))
                    .zip(required_string(item, "id"))
                    .map(|((thread_id, turn_id), item_id)| (thread_id, turn_id, item_id))
                else {
                    return unknown_event(method, params, request_id);
                };
                TurnEvent::AgentMessageStarted {
                    thread_id,
                    turn_id,
                    item_id,
                    phase: item.get("phase").and_then(Value::as_str).map(str::to_owned),
                }
            } else {
                item_lifecycle_event(method, params, request_id, false)
            }
        }
        "item/completed" => {
            let Some(item) = object.get("item").and_then(Value::as_object) else {
                return unknown_event(method, params, request_id);
            };
            if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                let Some((thread_id, turn_id, item_id, text)) = required_string(object, "threadId")
                    .zip(required_string(object, "turnId"))
                    .zip(required_string(item, "id"))
                    .zip(required_string(item, "text"))
                    .map(|(((thread_id, turn_id), item_id), text)| {
                        (thread_id, turn_id, item_id, text)
                    })
                else {
                    return unknown_event(method, params, request_id);
                };
                return TurnEvent::AgentMessageCompleted {
                    thread_id,
                    turn_id,
                    item_id,
                    text,
                    phase: item.get("phase").and_then(Value::as_str).map(str::to_owned),
                };
            }
            if item.get("type").and_then(Value::as_str) != Some("plan") {
                return item_lifecycle_event(method, params, request_id, true);
            }
            let Some((thread_id, turn_id, item_id, text)) = required_string(object, "threadId")
                .zip(required_string(object, "turnId"))
                .zip(required_string(item, "id"))
                .zip(required_string(item, "text"))
                .map(|(((thread_id, turn_id), item_id), text)| (thread_id, turn_id, item_id, text))
            else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::FinalPlan {
                thread_id,
                turn_id,
                item_id,
                text,
            }
        }
        "turn/diff/updated" => {
            let Some(thread_id) = required_string(object, "threadId") else {
                return unknown_event(method, params, request_id);
            };
            let Some(turn_id) = required_string(object, "turnId") else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::Diff {
                thread_id,
                turn_id,
                item_id: None,
                phase: "updated".into(),
                diff: object.get("diff").cloned().unwrap_or(Value::Null),
            }
        }
        "item/tool/requestUserInput" => {
            let Some(request_id) = request_id else {
                return unknown_event(method, params, None);
            };
            let questions = object.get("questions").cloned().and_then(|value| {
                serde_json::from_value::<Vec<RequestUserInputQuestion>>(value).ok()
            });
            let Some((thread_id, turn_id, item_id, questions)) =
                required_string(object, "threadId")
                    .zip(required_string(object, "turnId"))
                    .zip(required_string(object, "itemId"))
                    .zip(questions)
                    .map(|(((thread_id, turn_id), item_id), questions)| {
                        (thread_id, turn_id, item_id, questions)
                    })
            else {
                return unknown_event(method, params, Some(request_id));
            };
            TurnEvent::RequestUserInput {
                request_id,
                thread_id,
                turn_id,
                item_id,
                questions,
                auto_resolution_ms: object.get("autoResolutionMs").and_then(Value::as_u64),
            }
        }
        "model/rerouted" => {
            let Some((thread_id, turn_id, from_model, to_model)) =
                required_string(object, "threadId")
                    .zip(required_string(object, "turnId"))
                    .zip(required_string(object, "fromModel"))
                    .zip(required_string(object, "toModel"))
                    .map(|(((thread_id, turn_id), from_model), to_model)| {
                        (thread_id, turn_id, from_model, to_model)
                    })
            else {
                return unknown_event(method, params, request_id);
            };
            let reason = object
                .get("reason")
                .map(|reason| {
                    reason
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| reason.to_string())
                })
                .unwrap_or_else(|| "unknown".into());
            TurnEvent::ModelRerouted {
                thread_id,
                turn_id,
                from_model,
                to_model,
                reason,
            }
        }
        "thread/tokenUsage/updated" => {
            let token_usage = object
                .get("tokenUsage")
                .cloned()
                .and_then(|value| serde_json::from_value::<ThreadTokenUsage>(value).ok());
            let Some((thread_id, turn_id, token_usage)) = required_string(object, "threadId")
                .zip(required_string(object, "turnId"))
                .zip(token_usage)
                .map(|((thread_id, turn_id), usage)| (thread_id, turn_id, usage))
            else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::Usage {
                thread_id,
                turn_id,
                token_usage,
            }
        }
        "error" => {
            let Some(error) = object.get("error").and_then(Value::as_object) else {
                return unknown_event(method, params, request_id);
            };
            let Some((thread_id, turn_id, message)) = required_string(object, "threadId")
                .zip(required_string(object, "turnId"))
                .zip(required_string(error, "message"))
                .map(|((thread_id, turn_id), message)| (thread_id, turn_id, message))
            else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::Error {
                thread_id,
                turn_id,
                message,
                will_retry: object
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                codex_error_info: error
                    .get("codexErrorInfo")
                    .cloned()
                    .filter(|v| !v.is_null()),
                additional_details: error
                    .get("additionalDetails")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        }
        "item/agentMessage/delta" => {
            let Some((thread_id, turn_id, item_id, delta)) = required_string(object, "threadId")
                .zip(required_string(object, "turnId"))
                .zip(required_string(object, "itemId"))
                .zip(required_string(object, "delta"))
                .map(|(((thread_id, turn_id), item_id), delta)| {
                    (thread_id, turn_id, item_id, delta)
                })
            else {
                return unknown_event(method, params, request_id);
            };
            TurnEvent::AgentMessageDelta {
                thread_id,
                turn_id,
                item_id,
                delta,
                // Some protocol revisions include phase on the delta while
                // others only put it on item/started. Preserve it when sent;
                // providers correlate by item id for the latter form.
                phase: object
                    .get("phase")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        }
        "turn/completed" => {
            let Some(turn) = object.get("turn").and_then(Value::as_object) else {
                return unknown_event(method, params, request_id);
            };
            let Some(thread_id) = required_string(object, "threadId") else {
                return unknown_event(method, params, request_id);
            };
            let Some(turn_id) = required_string(turn, "id") else {
                return unknown_event(method, params, request_id);
            };
            let status = turn
                .get("status")
                .map(|status| {
                    status
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| status.to_string())
                })
                .unwrap_or_else(|| "unknown".into());
            TurnEvent::TurnCompleted {
                thread_id,
                turn_id,
                status,
                error: turn.get("error").cloned().filter(|value| !value.is_null()),
            }
        }
        _ => unknown_event(method, params, request_id),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestUserInputAnswer {
    #[serde(default)]
    pub answers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RequestUserInputResponse {
    #[serde(default)]
    pub answers: BTreeMap<String, RequestUserInputAnswer>,
}

impl RequestUserInputResponse {
    pub fn from_answers(answers: BTreeMap<String, Vec<String>>) -> Self {
        Self {
            answers: answers
                .into_iter()
                .map(|(id, answers)| (id, RequestUserInputAnswer { answers }))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartResponse {
    pub turn: Value,
}

impl CodexAppServerClient {
    pub async fn config_read(
        &self,
        cwd: Option<&Path>,
    ) -> Result<ConfigReadResponse, AppServerClientError> {
        self.request(
            "config/read",
            json!({
                "cwd": cwd.map(|path| path.to_string_lossy().to_string()),
                "includeLayers": true,
            }),
        )
        .await
    }

    pub async fn model_list_page(
        &self,
        cursor: Option<&str>,
        include_hidden: bool,
    ) -> Result<ModelListResponse, AppServerClientError> {
        self.request(
            "model/list",
            json!({
                "cursor": cursor,
                "includeHidden": include_hidden,
            }),
        )
        .await
    }

    pub async fn model_list(
        &self,
        include_hidden: bool,
    ) -> Result<Vec<CodexModel>, AppServerClientError> {
        let mut models = Vec::new();
        let mut cursor = None::<String>;
        let mut seen_cursors = HashSet::new();
        for _ in 0..MAX_MODEL_PAGES {
            let page = self
                .model_list_page(cursor.as_deref(), include_hidden)
                .await?;
            models.extend(page.data);
            let Some(next) = page.next_cursor else {
                return Ok(models);
            };
            if !seen_cursors.insert(next.clone()) {
                return Err(AppServerClientError::Protocol(format!(
                    "model/list repeated pagination cursor '{next}'"
                )));
            }
            cursor = Some(next);
        }
        Err(AppServerClientError::Protocol(format!(
            "model/list exceeded {MAX_MODEL_PAGES} pages"
        )))
    }

    pub async fn collaboration_mode_list(
        &self,
    ) -> Result<CollaborationModeListResponse, AppServerClientError> {
        self.request("collaborationMode/list", json!({})).await
    }

    pub async fn provider_capabilities(
        &self,
    ) -> Result<ProviderCapabilities, AppServerClientError> {
        match self.request(METHOD_PROVIDER_CAPABILITIES, json!({})).await {
            Ok(capabilities) => Ok(capabilities),
            Err(AppServerClientError::Rpc(error)) if should_retry_provider_capabilities(&error) => {
                // The selected runtime still decides; no capability is
                // fabricated when both schema spellings reject the request.
                self.request(ALTERNATE_METHOD_PROVIDER_CAPABILITIES, json!({}))
                    .await
            }
            Err(error) => Err(error),
        }
    }

    pub async fn start_turn(
        &self,
        params: &TurnStartParams,
    ) -> Result<TurnStartResponse, AppServerClientError> {
        self.request("turn/start", params).await
    }

    pub async fn start_resolved_turn(
        &self,
        thread_id: impl Into<String>,
        input: Vec<CodexUserInput>,
        config: &ResolvedTurnConfig,
    ) -> Result<TurnStartResponse, AppServerClientError> {
        let params = build_turn_start_params(thread_id, input, config)
            .map_err(|error| AppServerClientError::Protocol(error.to_string()))?;
        self.start_turn(&params).await
    }

    pub async fn answer_request_user_input(
        &self,
        request_id: RpcId,
        answers: BTreeMap<String, Vec<String>>,
    ) -> Result<(), AppServerClientError> {
        self.respond_result(request_id, RequestUserInputResponse::from_answers(answers))
            .await
    }

    /// Read all runtime-backed values from this exact actor/executable.  Plan
    /// and provider probes are optional capabilities; config and model catalog
    /// failures are fatal because resolving a truthful picker would be unsafe.
    pub async fn runtime_snapshot(
        &self,
        cwd: Option<&Path>,
    ) -> Result<RuntimeSnapshot, AppServerClientError> {
        let (models, config, modes, provider) = tokio::join!(
            self.model_list(true),
            self.config_read(cwd),
            self.collaboration_mode_list(),
            self.provider_capabilities(),
        );
        let models = models?;
        let config = config?;
        let mut warnings = Vec::new();
        if let Some(error) = &self.inner.version_probe_error {
            warnings.push(format!("Codex version probe failed: {error}"));
        }
        let collaboration_modes = match modes {
            Ok(response) => response.data,
            Err(error) => {
                warnings.push(format!(
                    "Native collaborationMode/list is unavailable: {error}"
                ));
                Vec::new()
            }
        };
        let provider_capabilities = match provider {
            Ok(capabilities) => capabilities,
            Err(error) => {
                warnings.push(format!(
                    "Model provider capability probe is unavailable: {error}"
                ));
                ProviderCapabilities::default()
            }
        };
        let capabilities =
            infer_runtime_capabilities(&models, &collaboration_modes, &provider_capabilities);
        let initialize = self.initialize_response();
        let runtime = RuntimeIdentity {
            executable: self.runtime_command().executable().to_string(),
            launcher: self.runtime_command().launcher().to_string(),
            source: self.runtime_command().source,
            entrypoint: self.runtime_command().entrypoint.clone(),
            version: self.runtime_version().map(str::to_owned),
            codex_home: initialize.codex_home.clone(),
            platform_family: initialize.platform_family.clone(),
            platform_os: initialize.platform_os.clone(),
            user_agent: initialize.user_agent.clone(),
        };
        let version_payload = serde_json::to_vec(&json!({
            "runtime": runtime,
            "models": models,
            "config": config,
            "collaborationModes": collaboration_modes,
            "providerCapabilities": provider_capabilities,
        }))
        .map_err(|error| AppServerClientError::Serialization(error.to_string()))?;
        let config_version = fnv1a64(&version_payload);
        Ok(RuntimeSnapshot {
            config_version,
            runtime,
            capabilities,
            models,
            config,
            collaboration_modes,
            provider_capabilities,
            warnings,
            refreshed_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        })
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Integration-facing snapshot helper.  `generation` is owned by the caller's
/// watcher/store, so an external config/runtime change can invalidate a preview
/// even when its JSON happens to hash to the same value.
pub async fn load_runtime_snapshot(
    client: &CodexAppServerClient,
    cwd: &Path,
    generation: u64,
) -> Result<RuntimeSnapshot, AppServerClientError> {
    let mut snapshot = client.runtime_snapshot(Some(cwd)).await?;
    snapshot.config_version = generation;
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "wisp-codex-app-server-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn codex_file_name() -> &'static str {
        if cfg!(windows) {
            "codex.exe"
        } else {
            "codex"
        }
    }

    fn sample_model() -> CodexModel {
        CodexModel {
            id: "gpt-5.6".into(),
            model: "gpt-5.6".into(),
            display_name: "GPT-5.6".into(),
            description: "test".into(),
            hidden: false,
            supported_reasoning_efforts: ["low", "high", "max", "ultra"]
                .into_iter()
                .map(|effort| ReasoningEffortOption {
                    reasoning_effort: effort.into(),
                    description: String::new(),
                })
                .collect(),
            default_reasoning_effort: "high".into(),
            input_modalities: vec!["text".into(), "image".into()],
            supports_personality: true,
            service_tiers: vec![ModelServiceTier {
                id: "fast".into(),
                name: "Fast".into(),
                description: String::new(),
            }],
            default_service_tier: Some("fast".into()),
            is_default: true,
            ..CodexModel::default()
        }
    }

    fn sample_snapshot() -> RuntimeSnapshot {
        let model = sample_model();
        RuntimeSnapshot {
            config_version: 41,
            runtime: RuntimeIdentity {
                executable: r"C:\Codex\codex.exe".into(),
                launcher: r"C:\Codex\codex.exe".into(),
                source: RuntimeSource::CodexDesktop,
                entrypoint: RuntimeEntrypoint::native(r"C:\Codex\codex.exe"),
                version: Some("codex-cli 0.142.5".into()),
                codex_home: r"C:\project\.wisp\codex-home\profile".into(),
                platform_family: "windows".into(),
                platform_os: "windows".into(),
                user_agent: "codex_cli_rs/0.142.5".into(),
            },
            capabilities: RuntimeCapabilities {
                app_server: true,
                native_plan: true,
                images: true,
                personality: true,
                service_tier: true,
                reasoning_summary: true,
                verbosity: true,
                web_search: true,
                sandbox: true,
            },
            models: vec![model],
            config: ConfigReadResponse {
                config: EffectiveCodexConfig {
                    model: Some("gpt-5.6".into()),
                    model_reasoning_effort: Some("low".into()),
                    model_reasoning_summary: Some("concise".into()),
                    model_verbosity: Some("medium".into()),
                    service_tier: Some("fast".into()),
                    web_search: Some("live".into()),
                    sandbox_mode: Some("workspace-write".into()),
                    personality: Some("friendly".into()),
                    ..EffectiveCodexConfig::default()
                },
                ..ConfigReadResponse::default()
            },
            collaboration_modes: vec![CollaborationModeMask {
                name: "Plan".into(),
                mode: Some(TurnMode::Plan),
                // Plan's model is deliberately different: Wisp must inherit
                // the effective local model unless the user overrides it.
                model: Some("preset-model-must-not-win".into()),
                reasoning_effort: Some("max".into()),
            }],
            provider_capabilities: ProviderCapabilities {
                web_search: true,
                ..ProviderCapabilities::default()
            },
            warnings: Vec::new(),
            refreshed_at_ms: 1,
        }
    }

    #[test]
    fn runtime_resolution_is_explicit_then_newest_desktop_then_path() {
        let root = temp_dir("resolution");
        let old_dir = root.join("desktop").join("app-1.9.0").join("bin");
        let new_dir = root.join("desktop").join("app-2.1.0").join("bin");
        let path_dir = root.join("path");
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        fs::write(old_dir.join(codex_file_name()), b"old").unwrap();
        fs::write(new_dir.join(codex_file_name()), b"new").unwrap();
        fs::write(path_dir.join(codex_file_name()), b"path").unwrap();

        let explicit = RuntimeResolveOptions {
            explicit: Some(RuntimeEntrypoint::native("chosen-codex")),
            desktop_search_roots: vec![root.join("desktop")],
            path_override: Some(path_dir.to_string_lossy().to_string()),
            ..RuntimeResolveOptions::default()
        };
        let resolved = resolve_codex_command(&explicit).unwrap();
        assert_eq!(resolved.source, RuntimeSource::Explicit);
        assert_eq!(resolved.executable(), "chosen-codex");

        let desktop = RuntimeResolveOptions {
            desktop_search_roots: vec![root.join("desktop")],
            path_override: Some(path_dir.to_string_lossy().to_string()),
            ..RuntimeResolveOptions::default()
        };
        let resolved = resolve_codex_command(&desktop).unwrap();
        assert_eq!(resolved.source, RuntimeSource::CodexDesktop);
        assert!(resolved.executable().contains("app-2.1.0"));

        let path_only = RuntimeResolveOptions {
            desktop_search_roots: vec![root.join("missing")],
            path_override: Some(path_dir.to_string_lossy().to_string()),
            ..RuntimeResolveOptions::default()
        };
        let resolved = resolve_codex_command(&path_only).unwrap();
        assert_eq!(resolved.source, RuntimeSource::Path);
        assert_eq!(
            Path::new(resolved.executable()),
            path_dir.join(codex_file_name())
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_wsl_entrypoint_keeps_its_own_home() {
        let mut environment = BTreeMap::new();
        environment.insert("CODEX_HOME".into(), "/home/me/.wisp/codex-home/p".into());
        let entrypoint = RuntimeEntrypoint::Wsl {
            launcher: "wsl.exe".into(),
            distribution: Some("Ubuntu-24.04".into()),
            program: "/usr/local/bin/codex".into(),
            args: vec![],
            environment: environment.clone(),
        };
        let resolved = resolve_codex_command(&RuntimeResolveOptions {
            explicit: Some(entrypoint.clone()),
            // This is metadata only for WSL; it is not injected into the
            // Windows environment by process_for.
            codex_home: Some("/home/me/.wisp/codex-home/p".into()),
            ..RuntimeResolveOptions::default()
        })
        .unwrap();
        assert_eq!(resolved.entrypoint, entrypoint);
        assert_eq!(resolved.source, RuntimeSource::Explicit);
        assert_eq!(
            resolved.codex_home.as_deref(),
            Some("/home/me/.wisp/codex-home/p")
        );
    }

    #[test]
    fn parses_fake_json_rpc_lines_without_codex() {
        assert_eq!(
            parse_json_rpc_line(r#"{"id":7,"result":{"ok":true}}"#).unwrap(),
            ParsedJsonRpcLine::Response {
                id: RpcId::Number(7),
                result: Ok(json!({"ok": true})),
            }
        );
        let error =
            parse_json_rpc_line(r#"{"id":"x","error":{"code":-32601,"message":"not found"}}"#)
                .unwrap();
        assert!(matches!(
            error,
            ParsedJsonRpcLine::Response {
                id: RpcId::String(id),
                result: Err(RpcErrorObject { code: -32601, .. })
            } if id == "x"
        ));
        assert!(matches!(
            parse_json_rpc_line(r#"{"method":"turn/started","params":{"x":1}}"#)
                .unwrap(),
            ParsedJsonRpcLine::Notification { method, .. } if method == "turn/started"
        ));
        assert!(matches!(
            parse_json_rpc_line(
                r#"{"id":9,"method":"item/tool/requestUserInput","params":{}}"#
            )
            .unwrap(),
            ParsedJsonRpcLine::Request { id: RpcId::Number(9), method, .. }
                if method == "item/tool/requestUserInput"
        ));
    }

    #[tokio::test]
    async fn fake_stdio_writer_uses_newline_delimited_codex_rpc() {
        use tokio::io::AsyncReadExt;

        let (mut writer, mut reader) = tokio::io::duplex(512);
        write_json_line(
            &mut writer,
            &json!({"id": 1, "method": "model/list", "params": {}}),
        )
        .await
        .unwrap();
        drop(writer);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await.unwrap();
        let line = String::from_utf8(bytes).unwrap();
        assert!(line.ends_with('\n'));
        assert!(!line.contains("jsonrpc"));
        assert!(matches!(
            parse_json_rpc_line(line.trim()).unwrap(),
            ParsedJsonRpcLine::Request {
                id: RpcId::Number(1),
                method,
                ..
            } if method == "model/list"
        ));
    }

    #[test]
    fn plan_inherits_local_model_and_native_plan_effort() {
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Plan,
            profile: CodexOverrideSet::default(),
            session: CodexOverrideSet::default(),
        });
        assert_eq!(resolved.effective_model.as_deref(), Some("gpt-5.6"));
        assert_eq!(resolved.effective_effort.as_deref(), Some("max"));
        assert_eq!(
            resolved.sources.get("model"),
            Some(&ConfigValueSource::LocalCodex)
        );
        assert_eq!(
            resolved.sources.get("reasoning_effort"),
            Some(&ConfigValueSource::PlanPreset)
        );
        assert!(matches!(
            resolved.sandbox,
            Some(SandboxPolicy::ReadOnly { .. })
        ));
        assert_eq!(
            resolved.effective_sources.get("sandbox"),
            Some(&ConfigValueSource::ForcedPlanPolicy)
        );
    }

    #[test]
    fn session_then_profile_priority_accepts_future_effort_strings() {
        let mut profile = CodexOverrideSet::default();
        profile.normal.model = Some("profile-model".into());
        profile.normal.reasoning_effort = Some("max".into());
        let mut session = CodexOverrideSet::default();
        session.normal.model = Some("gpt-5.6".into());
        session.normal.reasoning_effort = Some("ultra".into());
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Default,
            profile,
            session,
        });
        assert_eq!(resolved.requested_model.as_deref(), Some("gpt-5.6"));
        assert_eq!(resolved.requested_effort.as_deref(), Some("ultra"));
        assert_eq!(
            resolved.sources.get("reasoning_effort"),
            Some(&ConfigValueSource::SessionOverride)
        );
        assert!(resolved.validation_errors.is_empty());
    }

    #[test]
    fn known_invalid_explicit_effort_is_never_silently_replaced() {
        let mut session = CodexOverrideSet::default();
        session.normal.reasoning_effort = Some("impossible".into());
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Default,
            profile: CodexOverrideSet::default(),
            session,
        });
        assert_eq!(resolved.requested_effort.as_deref(), Some("impossible"));
        assert_eq!(resolved.effective_effort.as_deref(), Some("impossible"));
        assert!(!resolved.validation_errors.is_empty());
        assert!(matches!(
            build_turn_start_params("thread", vec![CodexUserInput::text("hi")], &resolved),
            Err(TurnStartBuildError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn custom_model_and_custom_effort_are_passed_to_codex() {
        let mut session = CodexOverrideSet::default();
        session.normal.model = Some("my-provider/custom-model".into());
        session.normal.reasoning_effort = Some("provider-special".into());
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Default,
            profile: CodexOverrideSet::default(),
            session,
        });
        assert_eq!(
            resolved.effective_model.as_deref(),
            Some("my-provider/custom-model")
        );
        assert_eq!(
            resolved.effective_effort.as_deref(),
            Some("provider-special")
        );
        assert!(resolved.validation_errors.is_empty());
    }

    #[test]
    fn turn_start_serializes_exact_resolved_plan_values() {
        let mut profile = CodexOverrideSet::default();
        profile.plan.reasoning_effort = Some("ultra".into());
        profile.plan.personality = Some("pragmatic".into());
        profile.plan.summary = Some("detailed".into());
        profile.plan.verbosity = Some("high".into());
        profile.plan.web_search = Some("live".into());
        profile.plan.sandbox = Some(SandboxPolicy::DangerFullAccess);
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Plan,
            profile,
            session: CodexOverrideSet::default(),
        });
        assert!(resolved.validation_errors.is_empty());
        let params = build_turn_start_params(
            "thread-1",
            vec![
                CodexUserInput::text("make a plan"),
                CodexUserInput::LocalImage {
                    path: r"C:\figure.png".into(),
                    detail: Some("high".into()),
                },
            ],
            &resolved,
        )
        .unwrap();
        let value = serde_json::to_value(params).unwrap();
        assert_eq!(value["threadId"], "thread-1");
        assert_eq!(value["model"], "gpt-5.6");
        assert_eq!(value["effort"], "ultra");
        assert_eq!(value["serviceTier"], "fast");
        assert_eq!(value["personality"], "pragmatic");
        assert_eq!(value["summary"], "detailed");
        // Codex 0.144 does not accept these as turn/start fields.  They are
        // applied at actor startup (`-c`) and must not be represented as sent
        // per-turn values.
        assert!(value.get("verbosity").is_none());
        assert!(value.get("webSearch").is_none());
        assert_eq!(value["collaborationMode"]["mode"], "plan");
        assert_eq!(
            value["collaborationMode"]["settings"]["reasoning_effort"],
            "ultra"
        );
        assert_eq!(value["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(value["input"][1]["type"], "localImage");
    }

    #[test]
    fn parses_plan_input_reroute_usage_and_error_events() {
        assert!(matches!(
            parse_turn_event(
                "item/plan/delta",
                &json!({"threadId":"t","turnId":"u","itemId":"i","delta":"abc"}),
                None,
            ),
            TurnEvent::PlanDelta { delta, .. } if delta == "abc"
        ));
        assert!(matches!(
            parse_turn_event(
                "item/completed",
                &json!({"threadId":"t","turnId":"u","item":{"type":"plan","id":"i","text":"# Plan"}}),
                None,
            ),
            TurnEvent::FinalPlan { text, .. } if text == "# Plan"
        ));
        assert!(matches!(
            parse_turn_event(
                "item/tool/requestUserInput",
                &json!({
                    "threadId":"t","turnId":"u","itemId":"i","autoResolutionMs":60000,
                    "questions":[{"id":"q","header":"Mode","question":"Which?","isOther":false,"isSecret":false,"options":[{"label":"A","description":"a"}]}]
                }),
                Some(RpcId::Number(77)),
            ),
            TurnEvent::RequestUserInput { request_id: RpcId::Number(77), questions, .. }
                if questions[0].id == "q"
        ));
        assert!(matches!(
            parse_turn_event(
                "model/rerouted",
                &json!({"threadId":"t","turnId":"u","fromModel":"a","toModel":"b","reason":"highRiskCyberActivity"}),
                None,
            ),
            TurnEvent::ModelRerouted { to_model, .. } if to_model == "b"
        ));
        assert!(matches!(
            parse_turn_event(
                "thread/tokenUsage/updated",
                &json!({
                    "threadId":"t","turnId":"u",
                    "tokenUsage":{"total":{"totalTokens":10,"inputTokens":6,"cachedInputTokens":2,"outputTokens":4,"reasoningOutputTokens":1},"last":{},"modelContextWindow":100}
                }),
                None,
            ),
            TurnEvent::Usage { token_usage, .. } if token_usage.total.total_tokens == 10
        ));
        assert!(matches!(
            parse_turn_event(
                "error",
                &json!({"threadId":"t","turnId":"u","willRetry":false,"error":{"message":"bad","codexErrorInfo":"badRequest","additionalDetails":"details"}}),
                None,
            ),
            TurnEvent::Error { message, will_retry: false, .. } if message == "bad"
        ));
    }

    #[test]
    fn preserves_agent_message_phase_across_item_lifecycle() {
        assert!(matches!(
            parse_turn_event(
                "item/started",
                &json!({
                    "threadId":"t","turnId":"u",
                    "item":{"type":"agentMessage","id":"commentary-1","phase":"commentary","text":""}
                }),
                None,
            ),
            TurnEvent::AgentMessageStarted { item_id, phase: Some(phase), .. }
                if item_id == "commentary-1" && phase == "commentary"
        ));
        assert!(matches!(
            parse_turn_event(
                "item/agentMessage/delta",
                &json!({
                    "threadId":"t","turnId":"u","itemId":"final-1",
                    "phase":"final_answer","delta":"answer"
                }),
                None,
            ),
            TurnEvent::AgentMessageDelta { delta, phase: Some(phase), .. }
                if delta == "answer" && phase == "final_answer"
        ));
        assert!(matches!(
            parse_turn_event(
                "item/completed",
                &json!({
                    "threadId":"t","turnId":"u",
                    "item":{"type":"agentMessage","id":"commentary-1","phase":"commentary","text":"working"}
                }),
                None,
            ),
            TurnEvent::AgentMessageCompleted { text, phase: Some(phase), .. }
                if text == "working" && phase == "commentary"
        ));
    }

    #[test]
    fn parses_tool_call_result_and_diff_events() {
        let started = json!({
            "threadId":"t","turnId":"u",
            "item":{"type":"mcpToolCall","id":"i","server":"wisp","tool":"search","arguments":{"q":"x"},"status":"inProgress"}
        });
        assert!(matches!(
            parse_turn_event("item/started", &started, None),
            TurnEvent::ToolCall { name, arguments, .. }
                if name == "wisp/search" && arguments["q"] == "x"
        ));
        let completed = json!({
            "threadId":"t","turnId":"u",
            "item":{"type":"dynamicToolCall","id":"i","namespace":"demo","tool":"run","arguments":{},"status":"completed","success":true,"contentItems":[{"type":"inputText","text":"ok"}]}
        });
        assert!(matches!(
            parse_turn_event("item/completed", &completed, None),
            TurnEvent::ToolResult { name, success: Some(true), .. } if name == "demo/run"
        ));
        let diff = json!({
            "threadId":"t","turnId":"u",
            "item":{"type":"fileChange","id":"i","changes":[{"path":"a","kind":"update"}],"status":"completed"}
        });
        assert!(matches!(
            parse_turn_event("item/completed", &diff, None),
            TurnEvent::Diff { phase, .. } if phase == "completed"
        ));
    }

    #[test]
    fn reroute_preserves_requested_model_and_marks_effective_source() {
        let mut resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Default,
            profile: CodexOverrideSet::default(),
            session: CodexOverrideSet::default(),
        });
        let requested = resolved.requested_model.clone();
        resolved.apply_model_reroute("safe-model", "policy");
        assert_eq!(resolved.requested_model, requested);
        assert_eq!(resolved.effective_model.as_deref(), Some("safe-model"));
        assert_eq!(
            resolved.effective_sources.get("model"),
            Some(&ConfigValueSource::ServerReroute)
        );
    }

    #[test]
    fn stale_snapshot_is_rejected_and_provider_method_is_canonical() {
        let resolved = resolve_turn_config(&TurnConfigResolutionInput {
            snapshot: sample_snapshot(),
            mode: TurnMode::Default,
            profile: CodexOverrideSet::default(),
            session: CodexOverrideSet::default(),
        });
        let mut changed = sample_snapshot();
        changed.config_version += 1;
        assert_eq!(
            resolved.assert_snapshot_version(&changed),
            Err(StaleConfig {
                expected: 41,
                actual: 42,
            })
        );
        assert_eq!(
            METHOD_PROVIDER_CAPABILITIES,
            "modelProvider/capabilities/read"
        );
        assert!(should_retry_provider_capabilities(&RpcErrorObject {
            code: -32600,
            message: "Invalid request: unknown variant".into(),
            data: None,
        }));
    }
}
