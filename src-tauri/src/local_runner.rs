use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use wisp_llm::{Message, Role};

pub const PROVIDER_CODEX_CLI: &str = "codex_cli";
pub const PROVIDER_CLAUDE_CODE: &str = "claude_code";

pub const INHERIT_SETTING: &str = "inherit";

/// Which Codex collaboration-mode settings a caller wants to resolve.
///
/// The JSONL `codex exec` compatibility path always uses `Normal`.  The app
/// server integration can use `Plan` without having to reinterpret persisted
/// profile fields itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerModelMode {
    Normal,
    Plan,
}

#[derive(Debug, Clone)]
pub struct LocalRunnerSettings {
    pub command: String,
    pub profile: String,
    pub sandbox: String,
    /// Kept for source compatibility with the v0.7 runner.  New profiles use
    /// `web_search_mode`; `true` is interpreted as `live` only when the mode
    /// itself is inherited.
    pub web_search: bool,
    pub web_search_mode: String,
    /// Backward-compatible alias for the effective normal model.
    pub model: String,
    pub normal_model: String,
    pub normal_reasoning_effort: String,
    pub plan_model: String,
    pub plan_reasoning_effort: String,
    pub service_tier: String,
    pub personality: String,
    pub reasoning_summary: String,
    pub verbosity: String,
    pub claude_command: String,
    pub persistent: bool,
}

impl Default for LocalRunnerSettings {
    fn default() -> Self {
        Self {
            command: String::new(),
            profile: String::new(),
            sandbox: "danger-full-access".into(),
            web_search: false,
            web_search_mode: INHERIT_SETTING.into(),
            model: INHERIT_SETTING.into(),
            normal_model: INHERIT_SETTING.into(),
            normal_reasoning_effort: INHERIT_SETTING.into(),
            plan_model: INHERIT_SETTING.into(),
            plan_reasoning_effort: INHERIT_SETTING.into(),
            service_tier: INHERIT_SETTING.into(),
            personality: INHERIT_SETTING.into(),
            reasoning_summary: INHERIT_SETTING.into(),
            verbosity: INHERIT_SETTING.into(),
            claude_command: String::new(),
            persistent: false,
        }
    }
}

impl LocalRunnerSettings {
    /// Return a Wisp model override, or `None` when Codex/Claude should inherit
    /// its own configuration.  A Plan model of `inherit` intentionally stays
    /// unset rather than copying the normal override; app-server then keeps the
    /// effective thread model.
    pub fn model_override(&self, mode: RunnerModelMode) -> Option<&str> {
        match mode {
            RunnerModelMode::Normal => {
                non_inherited(&self.normal_model).or_else(|| non_inherited(&self.model))
            }
            RunnerModelMode::Plan => non_inherited(&self.plan_model),
        }
    }

    pub fn reasoning_effort_override(&self, mode: RunnerModelMode) -> Option<&str> {
        let value = match mode {
            RunnerModelMode::Normal => self.normal_reasoning_effort.as_str(),
            RunnerModelMode::Plan => self.plan_reasoning_effort.as_str(),
        };
        non_inherited(value)
    }

    pub fn web_search_override(&self) -> Option<&str> {
        let value = self.web_search_mode.trim();
        if value.is_empty() || value.eq_ignore_ascii_case(INHERIT_SETTING) {
            return self.web_search.then_some("live");
        }
        match value.to_ascii_lowercase().as_str() {
            "live" => Some("live"),
            "indexed" => Some("indexed"),
            "cached" => Some("cached"),
            "disabled" | "off" | "false" => Some("disabled"),
            _ => None,
        }
    }
}

fn non_inherited(value: &str) -> Option<&str> {
    let value = value.trim();
    if value.is_empty()
        || [
            "inherit",
            "default",
            "codex-default",
            "claude-default",
            "inherit_local_codex_default",
        ]
        .iter()
        .any(|sentinel| value.eq_ignore_ascii_case(sentinel))
    {
        None
    } else {
        Some(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalRunnerCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub prompt_cwd: String,
    pub image_args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpBridgeLaunch {
    pub command: String,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerRuntime {
    pub home_dir: PathBuf,
    pub config_path: PathBuf,
    pub env: Vec<(String, String)>,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerEvent {
    Text(String),
    Reasoning(String),
    ToolCall {
        name: String,
        preview: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        content: String,
    },
    Diff {
        path: String,
    },
    Usage {
        input: u64,
        output: u64,
    },
    Error(String),
}

pub fn is_codex_cli(provider: &str) -> bool {
    provider.trim() == PROVIDER_CODEX_CLI
}

pub fn is_claude_code(provider: &str) -> bool {
    provider.trim() == PROVIDER_CLAUDE_CODE
}

pub fn is_local_runner(provider: &str) -> bool {
    is_codex_cli(provider) || is_claude_code(provider)
}

pub fn default_runner_sandbox(raw: &str) -> String {
    match raw.trim() {
        "read-only" | "workspace-write" | "danger-full-access" => raw.trim().to_string(),
        _ => "danger-full-access".into(),
    }
}

fn append_runner_sandbox(args: &mut Vec<String>, raw: &str) {
    if matches!(
        raw.trim(),
        "read-only" | "workspace-write" | "danger-full-access"
    ) {
        args.extend(["--sandbox".into(), raw.trim().into()]);
    }
}

pub fn build_codex_command(
    settings: &LocalRunnerSettings,
    project_root: &Path,
    attachments: &[String],
    session_id: Option<&str>,
) -> LocalRunnerCommand {
    build_codex_command_for_mode(
        settings,
        RunnerModelMode::Normal,
        project_root,
        attachments,
        session_id,
    )
}

/// Build the legacy JSONL `codex exec` command.  Native Plan mode uses the
/// app-server protocol, but accepting a mode here keeps the fallback precise
/// and makes it possible to run a Plan prompt with its selected model when an
/// older Codex build lacks collaboration modes.
pub fn build_codex_command_for_mode(
    settings: &LocalRunnerSettings,
    mode: RunnerModelMode,
    project_root: &Path,
    attachments: &[String],
    session_id: Option<&str>,
) -> LocalRunnerCommand {
    let session_id = session_id.map(str::trim).filter(|s| !s.is_empty());
    let image_args = attachments
        .iter()
        .filter(|p| is_image_path(p))
        .cloned()
        .collect::<Vec<_>>();
    let use_wsl = should_use_wsl(project_root);
    let prompt_cwd = if use_wsl {
        to_wsl_path(project_root).unwrap_or_else(|| project_root.display().to_string())
    } else {
        project_root.display().to_string()
    };
    let cwd = if use_wsl {
        PathBuf::from(r"C:\Windows\System32")
    } else {
        project_root.to_path_buf()
    };
    let (program, mut args) = resolve_runner_program(
        settings,
        use_wsl,
        wsl_distribution_for(project_root).as_deref(),
    );
    if !settings.profile.trim().is_empty() {
        args.extend(["--profile".into(), settings.profile.trim().into()]);
    }
    append_codex_overrides(&mut args, settings, mode);
    if let Some(session_id) = session_id {
        args.extend(["--cd".into(), prompt_cwd.clone()]);
        append_runner_sandbox(&mut args, &settings.sandbox);
        args.extend(["-c".into(), "approval_policy=\"never\"".into()]);
        args.extend([
            "exec".into(),
            "resume".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
        ]);
        for image in &image_args {
            let image = if use_wsl {
                to_wsl_path(Path::new(image)).unwrap_or_else(|| image.clone())
            } else {
                image.clone()
            };
            args.extend(["--image".into(), image]);
        }
        args.push(session_id.into());
        args.push("-".into());
        return LocalRunnerCommand {
            program,
            args,
            cwd,
            prompt_cwd,
            image_args,
            env: vec![],
        };
    }
    args.extend([
        "exec".into(),
        "--json".into(),
        "--cd".into(),
        prompt_cwd.clone(),
        "--skip-git-repo-check".into(),
    ]);
    append_runner_sandbox(&mut args, &settings.sandbox);
    args.extend(["-c".into(), "approval_policy=\"never\"".into()]);
    args.push("-".into());
    for image in &image_args {
        let image = if use_wsl {
            to_wsl_path(Path::new(image)).unwrap_or_else(|| image.clone())
        } else {
            image.clone()
        };
        args.extend(["--image".into(), image]);
    }
    LocalRunnerCommand {
        program,
        args,
        cwd,
        prompt_cwd,
        image_args,
        env: vec![],
    }
}

fn append_codex_overrides(
    args: &mut Vec<String>,
    settings: &LocalRunnerSettings,
    mode: RunnerModelMode,
) {
    if let Some(search) = settings.web_search_override() {
        if search.eq_ignore_ascii_case("live") {
            // Preserve the v0.7 CLI spelling.  It works on older Codex builds
            // that predate the string-valued `web_search` config option.
            args.push("--search".into());
        } else {
            push_config_override(args, "web_search", search);
        }
    }
    if let Some(model) = settings.model_override(mode) {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(effort) = settings.reasoning_effort_override(mode) {
        push_config_override(args, "model_reasoning_effort", effort);
    }
    for (key, value) in [
        ("service_tier", settings.service_tier.as_str()),
        ("personality", settings.personality.as_str()),
        (
            "model_reasoning_summary",
            settings.reasoning_summary.as_str(),
        ),
        ("model_verbosity", settings.verbosity.as_str()),
    ] {
        if let Some(value) = non_inherited(value) {
            push_config_override(args, key, value);
        }
    }
}

fn push_config_override(args: &mut Vec<String>, key: &str, value: &str) {
    args.extend(["-c".into(), format!("{key}={}", toml_string(value))]);
}

/// Replace all inherited/project external-process integrations for
/// compatibility Plan and re-add only Wisp's plan-safe scoped bridge. Global
/// options are inserted before `exec` so Codex parses them consistently across
/// CLI versions.
pub(crate) fn enforce_plan_mcp_isolation(
    command: &mut LocalRunnerCommand,
    bridge: &McpBridgeLaunch,
) {
    let mut overrides = vec![
        "-c".into(),
        "project_root_markers=[\".wisp\"]".into(),
        "-c".into(),
        "mcp_servers={}".into(),
        "-c".into(),
        "features.plugins=false".into(),
        "-c".into(),
        "features.remote_plugin=false".into(),
        "-c".into(),
        "features.apps=false".into(),
        "-c".into(),
        "features.computer_use=false".into(),
        "-c".into(),
        "features.browser_use=false".into(),
        "-c".into(),
        "features.browser_use_external=false".into(),
        "-c".into(),
        "features.browser_use_full_cdp_access=false".into(),
        "-c".into(),
        "features.in_app_browser=false".into(),
        "-c".into(),
        "features.image_generation=false".into(),
        "-c".into(),
        "features.code_mode=false".into(),
        "-c".into(),
        "features.code_mode_only=false".into(),
        "-c".into(),
        "features.code_mode_host=false".into(),
        "-c".into(),
        "features.enable_mcp_apps=false".into(),
        "-c".into(),
        "features.auth_elicitation=false".into(),
        "-c".into(),
        "features.tool_call_mcp_elicitation=false".into(),
        "-c".into(),
        "features.hooks=false".into(),
        "-c".into(),
        "features.codex_hooks=false".into(),
        "-c".into(),
        "features.shell_snapshot=false".into(),
        "-c".into(),
        "features.skill_mcp_dependency_install=false".into(),
        "-c".into(),
        "features.multi_agent=false".into(),
        "-c".into(),
        "features.multi_agent_v2=false".into(),
        "-c".into(),
        "features.enable_fanout=false".into(),
        "-c".into(),
        "notify=[]".into(),
    ];
    for (key, value) in [
        ("command", toml_string(&bridge.command)),
        ("args", toml_string_array(&bridge.args)),
        ("startup_timeout_sec", "120".into()),
        ("tool_timeout_sec", "3600".into()),
    ] {
        overrides.push("-c".into());
        overrides.push(format!("mcp_servers.wisp_bridge.{key}={value}"));
    }
    let index = command
        .args
        .iter()
        .position(|value| value == "exec")
        .unwrap_or(command.args.len());
    command.args.splice(index..index, overrides);
    if let Some(exec) = command.args.iter().position(|value| value == "exec") {
        // `--ignore-rules` belongs to the exec subcommand (it is not a Codex
        // top-level global). Put it before a possible `resume` subcommand.
        // Explicit allow rules otherwise bypass even a read-only sandbox.
        command.args.insert(exec + 1, "--ignore-rules".into());
    }
}

pub fn build_claude_code_command(
    settings: &LocalRunnerSettings,
    project_root: &Path,
    session_id: Option<&str>,
) -> LocalRunnerCommand {
    let use_wsl = should_use_wsl(project_root);
    let prompt_cwd = if use_wsl {
        to_wsl_path(project_root).unwrap_or_else(|| project_root.display().to_string())
    } else {
        project_root.display().to_string()
    };
    let cwd = if use_wsl {
        PathBuf::from(r"C:\Windows\System32")
    } else {
        project_root.to_path_buf()
    };
    let (program, mut args) = resolve_claude_program(
        settings,
        use_wsl,
        wsl_distribution_for(project_root).as_deref(),
    );
    args.push("-p".into());
    args.extend(["--output-format".into(), "stream-json".into()]);
    args.push("--verbose".into());
    args.extend(["--permission-mode".into(), "bypassPermissions".into()]);
    if let Some(model) = settings.model_override(RunnerModelMode::Normal) {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(session_id) = session_id.map(str::trim).filter(|s| !s.is_empty()) {
        args.extend(["--session-id".into(), session_id.into()]);
    }
    LocalRunnerCommand {
        program,
        args,
        cwd,
        prompt_cwd,
        image_args: vec![],
        env: vec![],
    }
}

pub fn apply_runtime_env(cmd: &mut LocalRunnerCommand, runtime: &RunnerRuntime) {
    cmd.env.extend(runtime.env.clone());
}

pub fn prepare_codex_runtime(
    project_root: &Path,
    bridge: &McpBridgeLaunch,
) -> Result<RunnerRuntime, String> {
    let home_dir = project_root.join(".wisp").join("codex-home");
    prepare_runtime_dir(&home_dir)?;
    let source = user_home_dir().map(|h| h.join(".codex"));
    let mut diagnostics = Vec::new();
    match source.as_deref() {
        Some(src) if src.is_dir() => sync_cli_home(src, &home_dir, CODEX_STATIC_CONFIG_DIRS)?,
        Some(src) => diagnostics.push(format!(
            "Local Codex config directory not found: {}. Wisp generated a minimal CODEX_HOME.",
            src.display()
        )),
        None => diagnostics
            .push("Cannot locate user home directory; Wisp generated a minimal CODEX_HOME.".into()),
    }
    remove_retired_codex_assets(&home_dir)?;
    let config_path = home_dir.join("config.toml");
    strip_codex_external_process_config(&config_path)?;
    inject_codex_config_block(&config_path, bridge)?;
    diagnostics.push(
        "Compatibility Codex disables inherited MCP/plugins/rules, marketplaces, thread config endpoints, and provider auth commands; only the plan-safe Wisp bridge is added."
            .into(),
    );
    let env_home = runner_env_path(project_root, &home_dir);
    Ok(RunnerRuntime {
        home_dir,
        config_path,
        env: vec![("CODEX_HOME".into(), env_home)],
        diagnostics,
    })
}

pub fn prepare_claude_runtime(
    project_root: &Path,
    bridge: &McpBridgeLaunch,
) -> Result<RunnerRuntime, String> {
    let home_dir = project_root.join(".wisp").join("claude-home");
    prepare_runtime_dir(&home_dir)?;
    let source = user_home_dir().map(|h| h.join(".claude"));
    let mut diagnostics = Vec::new();
    match source.as_deref() {
        Some(src) if src.is_dir() => sync_cli_home(src, &home_dir, CLAUDE_STATIC_CONFIG_DIRS)?,
        Some(src) => diagnostics.push(format!(
            "Local Claude config directory not found: {}. Wisp generated a minimal CLAUDE_CONFIG_DIR.",
            src.display()
        )),
        None => diagnostics.push(
            "Cannot locate user home directory; Wisp generated a minimal CLAUDE_CONFIG_DIR.".into(),
        ),
    }
    let config_path = home_dir.join("mcp.json");
    write_claude_mcp_config(&config_path, bridge)?;
    let env_home = runner_env_path(project_root, &home_dir);
    Ok(RunnerRuntime {
        home_dir,
        config_path,
        env: vec![("CLAUDE_CONFIG_DIR".into(), env_home)],
        diagnostics,
    })
}

pub fn add_claude_mcp_config(
    cmd: &mut LocalRunnerCommand,
    config_path: &Path,
    project_root: &Path,
) {
    let path = if should_use_wsl(project_root) {
        to_wsl_path(config_path).unwrap_or_else(|| config_path.display().to_string())
    } else {
        config_path.display().to_string()
    };
    cmd.args.extend(["--mcp-config".into(), path]);
}

fn resolve_runner_program(
    settings: &LocalRunnerSettings,
    use_wsl: bool,
    distribution: Option<&str>,
) -> (String, Vec<String>) {
    let command = settings.command.trim();
    if !command.is_empty() {
        let mut parts = split_command(command);
        if !parts.is_empty() {
            let program = parts.remove(0);
            if use_wsl
                && !program
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or(&program)
                    .eq_ignore_ascii_case("wsl.exe")
                && !program.eq_ignore_ascii_case("wsl")
            {
                let mut args = Vec::new();
                if let Some(distribution) = distribution {
                    args.extend(["--distribution".into(), distribution.into()]);
                }
                args.extend(["--exec".into(), program]);
                args.extend(parts);
                return ("wsl.exe".into(), args);
            }
            return (program, parts);
        }
    }
    if use_wsl {
        let mut args = Vec::new();
        if let Some(distribution) = distribution {
            args.extend(["--distribution".into(), distribution.into()]);
        }
        args.extend(["--exec".into(), "codex".into()]);
        ("wsl.exe".into(), args)
    } else {
        (default_windows_codex_program(), vec![])
    }
}

fn resolve_claude_program(
    settings: &LocalRunnerSettings,
    use_wsl: bool,
    distribution: Option<&str>,
) -> (String, Vec<String>) {
    let command = settings.claude_command.trim();
    if !command.is_empty() {
        let mut parts = split_command(command);
        if !parts.is_empty() {
            let program = parts.remove(0);
            return (program, parts);
        }
    }
    if use_wsl {
        let mut args = Vec::new();
        if let Some(distribution) = distribution {
            args.extend(["--distribution".into(), distribution.into()]);
        }
        args.extend(["--exec".into(), "claude".into()]);
        ("wsl.exe".into(), args)
    } else {
        ("claude".into(), vec![])
    }
}

#[cfg(windows)]
fn default_windows_codex_program() -> String {
    if let Some(path) = find_openai_codex_exe() {
        return path.display().to_string();
    }
    "codex".into()
}

#[cfg(not(windows))]
fn default_windows_codex_program() -> String {
    "codex".into()
}

#[cfg(windows)]
fn find_openai_codex_exe() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("OpenAI")
        .join("Codex")
        .join("bin");
    let entries = std::fs::read_dir(base).ok()?;
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("codex.exe"))
        .filter(|path| path.is_file())
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, path)| path)
}

pub(crate) fn split_command(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // A backslash is a path separator on Windows, not a general shell
            // escape. Only consume it when it explicitly escapes the active
            // quote (or unquoted whitespace/quote); otherwise preserve it.
            let escapes_next = chars.peek().is_some_and(|next| match quote {
                Some(active) => *next == active,
                None => next.is_whitespace() || matches!(*next, '"' | '\''),
            });
            if escapes_next {
                cur.push(chars.next().expect("peeked command character"));
            } else {
                cur.push(ch);
            }
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                cur.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
        } else if ch.is_whitespace() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn should_use_wsl(project_root: &Path) -> bool {
    let s = project_root.display().to_string().replace('\\', "/");
    let lower = s.to_ascii_lowercase();
    lower.starts_with("//wsl.localhost/")
        || lower.starts_with("//wsl$/")
        || s.starts_with("/home/")
        || s.starts_with("/mnt/")
}

pub(crate) fn wsl_distribution_for(project_root: &Path) -> Option<String> {
    let value = project_root.display().to_string().replace('\\', "/");
    ["//wsl.localhost/", "//wsl$/"]
        .into_iter()
        .find_map(|prefix| {
            value
                .get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
                .then(|| &value[prefix.len()..])
        })
        .and_then(|rest| rest.split('/').next())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

pub fn runner_uses_wsl(project_root: &Path) -> bool {
    should_use_wsl(project_root)
}

fn local_metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    false
}

fn verify_local_path_components(path: &Path) -> Result<(), String> {
    let mut components = path.ancestors().collect::<Vec<_>>();
    components.reverse();
    for component in components {
        let metadata = match fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "Cannot inspect compatibility Plan config path '{}': {error}",
                    component.display()
                ));
            }
        };
        if local_metadata_is_link_or_reparse(&metadata) {
            return Err(format!(
                "Compatibility Plan config path '{}' is a symlink or reparse point.",
                component.display()
            ));
        }
    }
    Ok(())
}

fn collect_external_process_config_keys(value: &toml::Value, path: &str, out: &mut Vec<String>) {
    let Some(table) = value.as_table() else {
        return;
    };
    for (key, value) in table {
        let qualified = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        if matches!(
            key.as_str(),
            "mcp_servers"
                | "plugins"
                | "marketplaces"
                | "experimental_thread_config_endpoint"
                | "notify"
                | "hooks"
        ) {
            let nonempty = match value {
                toml::Value::Table(table) => !table.is_empty(),
                toml::Value::Array(values) => !values.is_empty(),
                toml::Value::String(value) => !value.trim().is_empty(),
                _ => true,
            };
            if nonempty {
                out.push(qualified.clone());
            }
        }
        if key == "auth"
            && value
                .as_table()
                .and_then(|auth| auth.get("command"))
                .is_some()
        {
            out.push(format!("{qualified}.command"));
        }
        collect_external_process_config_keys(value, &qualified, out);
    }
}

/// `codex exec --ignore-rules` suppresses user/project exec policy, but it
/// cannot clear deep-merged project MCP/plugin/provider launchers. Reject the
/// compatibility Plan before spawning Codex whenever the active Wisp project
/// layer contains one; do not pretend `mcp_servers={}` erased it. The process
/// is pinned to this boundary with `project_root_markers=[".wisp"]`.
pub(crate) fn audit_codex_project_external_process_config(
    project_root: &Path,
) -> Result<(), String> {
    if !project_root.is_absolute() {
        return Err(format!(
            "Cannot safely map compatibility Plan project config from '{}'.",
            project_root.display()
        ));
    }
    let config = project_root.join(".codex").join("config.toml");
    verify_local_path_components(&config)?;
    let raw = match fs::read_to_string(&config) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Cannot verify compatibility Plan config '{}': {error}",
                config.display()
            ));
        }
    };
    let document = raw.parse::<toml::Value>().map_err(|error| {
        format!(
            "Cannot parse compatibility Plan config '{}': {error}",
            config.display()
        )
    })?;
    let mut unsafe_keys = Vec::new();
    collect_external_process_config_keys(&document, "", &mut unsafe_keys);
    unsafe_keys.sort();
    unsafe_keys.dedup();
    if !unsafe_keys.is_empty() {
        return Err(format!(
            "The isolated Codex runtime is unavailable because project config '{}' contains external-process settings that Wisp cannot safely clear: {}.",
            config.display(),
            unsafe_keys.join(", ")
        ));
    }
    Ok(())
}

pub(crate) fn to_wsl_path(path: &Path) -> Option<String> {
    to_wsl_path_for(path, None).ok().flatten()
}

pub(crate) fn to_wsl_path_for(
    path: &Path,
    expected_distribution: Option<&str>,
) -> Result<Option<String>, String> {
    let raw = path.display().to_string();
    let s = raw.replace('\\', "/");
    for prefix in ["//wsl.localhost/", "//wsl$/"] {
        if s.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        {
            let rest = &s[prefix.len()..];
            let mut parts = rest.splitn(2, '/');
            let distro = parts.next().unwrap_or_default();
            if expected_distribution.is_some_and(|expected| !distro.eq_ignore_ascii_case(expected))
            {
                return Err(format!(
                    "Path '{}' belongs to WSL distribution '{}', not the selected '{}'.",
                    path.display(),
                    distro,
                    expected_distribution.unwrap_or_default()
                ));
            }
            let inner = parts.next().unwrap_or("");
            return Ok(Some(format!("/{}", inner.trim_start_matches('/'))));
        }
    }
    if s.starts_with("/home/") || s.starts_with("/mnt/") {
        return Ok(Some(s));
    }
    if raw.len() >= 3 && raw.as_bytes()[1] == b':' {
        let drive = raw.chars().next().unwrap_or('c').to_ascii_lowercase();
        let rest = raw[2..].replace('\\', "/");
        return Ok(Some(format!("/mnt/{drive}{}", rest)));
    }
    Ok(None)
}

fn prepare_runtime_dir(home_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(home_dir).map_err(|e| {
        format!(
            "Failed to create local runner runtime '{}': {e}",
            home_dir.display()
        )
    })
}

fn user_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(format!(
                "{}{}",
                drive.to_string_lossy(),
                path.to_string_lossy()
            )))
        })
}

fn runner_env_path(project_root: &Path, path: &Path) -> String {
    if should_use_wsl(project_root) {
        to_wsl_path(path).unwrap_or_else(|| path.display().to_string())
    } else {
        path.display().to_string()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncManifest {
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

const SYNC_MANIFEST: &str = ".wisp-sync.json";
const CODEX_STATIC_CONFIG_DIRS: &[&str] = &["skills", "vendor_imports"];
const CLAUDE_STATIC_CONFIG_DIRS: &[&str] = &["plugins", "skills", "commands", "agents", "hooks"];

fn remove_retired_codex_assets(home: &Path) -> Result<(), String> {
    for name in ["plugins", "rules"] {
        let path = home.join(name);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("Failed to inspect '{}': {error}", path.display())),
        };
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(&path)
                .or_else(|_| fs::remove_dir(&path))
                .map_err(|error| format!("Failed to remove '{}': {error}", path.display()))?;
        } else if metadata.is_dir() {
            fs::remove_dir_all(&path)
                .map_err(|error| format!("Failed to remove '{}': {error}", path.display()))?;
        }
    }
    let manifest_path = home.join(SYNC_MANIFEST);
    if let Ok(raw) = fs::read_to_string(&manifest_path) {
        let mut manifest = serde_json::from_str::<SyncManifest>(&raw)
            .map_err(|error| format!("Invalid '{}': {error}", manifest_path.display()))?;
        manifest.entries.remove("plugins");
        manifest.entries.remove("rules");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("Failed to write '{}': {error}", manifest_path.display()))?;
    }
    Ok(())
}

fn strip_codex_external_process_config(config_path: &Path) -> Result<(), String> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to read isolated Codex config '{}': {error}",
                config_path.display()
            ));
        }
    };
    let mut document = raw.parse::<toml::Value>().map_err(|error| {
        format!(
            "Failed to parse isolated Codex config '{}' while enforcing process isolation: {error}",
            config_path.display()
        )
    })?;
    if let Some(table) = document.as_table_mut() {
        for key in [
            "mcp_servers",
            "plugins",
            "marketplaces",
            "experimental_thread_config_endpoint",
            "notify",
            "hooks",
        ] {
            table.remove(key);
        }
        if let Some(providers) = table
            .get_mut("model_providers")
            .and_then(toml::Value::as_table_mut)
        {
            for (_, provider) in providers.iter_mut() {
                if let Some(provider) = provider.as_table_mut() {
                    let remove_auth = if let Some(auth) =
                        provider.get_mut("auth").and_then(toml::Value::as_table_mut)
                    {
                        for key in [
                            "command",
                            "args",
                            "timeout_ms",
                            "refresh_interval_ms",
                            "cwd",
                        ] {
                            auth.remove(key);
                        }
                        auth.is_empty()
                    } else {
                        false
                    };
                    if remove_auth {
                        provider.remove("auth");
                    }
                }
            }
        }
    }
    fs::write(
        config_path,
        toml::to_string_pretty(&document).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Failed to write '{}': {error}", config_path.display()))
}

/// Seed the isolated CLI home with configuration and capability assets only.
/// Modern Codex/Claude homes contain live SQLite/WAL databases, logs and
/// session indexes; copying those on every turn is both expensive and unsafe.
/// This function is strictly source -> `<project>/.wisp/*-home` and never
/// writes to the user's global CLI home.
fn sync_cli_home(source: &Path, target: &Path, static_dirs: &[&str]) -> Result<(), String> {
    let manifest_path = target.join(SYNC_MANIFEST);
    let previous = fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<SyncManifest>(&raw).ok())
        .unwrap_or_default();
    let mut next = SyncManifest::default();
    let entries = fs::read_dir(source).map_err(|e| {
        format!(
            "Failed to read local CLI config directory '{}': {e}",
            source.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{e}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let meta = entry.metadata().map_err(|e| format!("{e}"))?;
        let is_static_dir = meta.is_dir() && static_dirs.contains(&name_s.as_ref());
        let is_config_file = meta.is_file() && allowed_root_config_file(&name_s);
        if !is_static_dir && !is_config_file {
            continue;
        }
        let dest = target.join(&name);
        let fingerprint = if is_static_dir {
            tree_fingerprint(&path)?
        } else {
            metadata_fingerprint(&meta)
        };
        next.entries.insert(name_s.to_string(), fingerprint.clone());
        if previous.entries.get(name_s.as_ref()) == Some(&fingerprint) && dest.exists() {
            continue;
        }
        if is_static_dir {
            sync_static_dir(&path, &dest)?;
        } else {
            fs::copy(&path, &dest).map_err(|e| {
                format!(
                    "Failed to copy inherited config '{}' to '{}': {e}",
                    path.display(),
                    dest.display()
                )
            })?;
        }
    }
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Failed to write '{}': {e}", manifest_path.display()))?;
    Ok(())
}

fn allowed_root_config_file(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        // Codex
        "config.toml"
            | "auth.json"
            | "agents.md"
            | "instructions.md"
            // Claude Code
            | "settings.json"
            | "settings.local.json"
            | ".credentials.json"
            | "claude.md"
    ) || name.ends_with(".config.toml")
}

fn metadata_fingerprint(meta: &fs::Metadata) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{}:{modified}", meta.len())
}

fn tree_fingerprint(root: &Path) -> Result<String, String> {
    fn visit(path: &Path, hash: &mut u64) -> Result<(), String> {
        let mut entries = fs::read_dir(path)
            .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            for byte in entry.file_name().to_string_lossy().as_bytes() {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(0x100000001b3);
            }
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            for byte in metadata_fingerprint(&meta).as_bytes() {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(0x100000001b3);
            }
            if meta.is_dir() {
                visit(&entry.path(), hash)?;
            }
        }
        Ok(())
    }
    let mut hash = 0xcbf29ce484222325u64;
    visit(root, &mut hash)?;
    Ok(format!("{hash:016x}"))
}

fn sync_static_dir(source: &Path, target: &Path) -> Result<(), String> {
    // Do not symlink back into the user's global CLI home: a tool modifying a
    // linked skill/plugin would then escape Wisp's isolated runtime. The
    // manifest prevents this incremental copy from running when unchanged.
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.file_type().is_symlink() {
            fs::remove_dir(target)
                .or_else(|_| fs::remove_file(target))
                .map_err(|e| {
                    format!(
                        "Failed to replace inherited link '{}': {e}",
                        target.display()
                    )
                })?;
        } else if meta.is_dir() {
            fs::remove_dir_all(target).map_err(|e| {
                format!(
                    "Failed to refresh inherited config dir '{}': {e}",
                    target.display()
                )
            })?;
        }
    }
    copy_dir_recursive(source, target)
}

fn copy_dir_recursive(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|e| {
        format!(
            "Failed to create inherited config dir '{}': {e}",
            target.display()
        )
    })?;
    for entry in fs::read_dir(source).map_err(|e| format!("{e}"))? {
        let entry = entry.map_err(|e| format!("{e}"))?;
        let src = entry.path();
        let dst = target.join(entry.file_name());
        let meta = entry.metadata().map_err(|e| format!("{e}"))?;
        if meta.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else if meta.is_file() {
            let unchanged = fs::metadata(&dst).ok().is_some_and(|existing| {
                metadata_fingerprint(&existing) == metadata_fingerprint(&meta)
            });
            if unchanged {
                continue;
            }
            fs::copy(&src, &dst).map_err(|e| {
                format!(
                    "Failed to copy inherited config '{}' to '{}': {e}",
                    src.display(),
                    dst.display()
                )
            })?;
        }
    }
    Ok(())
}

const WISP_BLOCK_BEGIN: &str = "# BEGIN WISP BUILTINS";
const WISP_BLOCK_END: &str = "# END WISP BUILTINS";

fn inject_codex_config_block(config_path: &Path, bridge: &McpBridgeLaunch) -> Result<(), String> {
    let existing = fs::read_to_string(config_path).unwrap_or_default();
    let block = codex_config_block(bridge);
    let updated = replace_marked_block(&existing, &block);
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
    }
    let mut f = fs::File::create(config_path).map_err(|e| {
        format!(
            "Failed to write Codex runtime config '{}': {e}",
            config_path.display()
        )
    })?;
    f.write_all(updated.as_bytes()).map_err(|e| format!("{e}"))
}

fn replace_marked_block(existing: &str, block: &str) -> String {
    let Some(start) = existing.find(WISP_BLOCK_BEGIN) else {
        let mut out = existing.trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
        out.push('\n');
        return out;
    };
    let Some(rel_end) = existing[start..].find(WISP_BLOCK_END) else {
        let mut out = existing[..start].trim_end().to_string();
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(block);
        out.push('\n');
        return out;
    };
    let end = start + rel_end + WISP_BLOCK_END.len();
    let mut out = String::new();
    out.push_str(existing[..start].trim_end());
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(block);
    out.push_str(existing[end..].trim_start_matches(['\r', '\n']));
    out
}

fn codex_config_block(bridge: &McpBridgeLaunch) -> String {
    format!(
        "{WISP_BLOCK_BEGIN}\n\
[mcp_servers.wisp_bridge]\n\
transport = \"stdio\"\n\
command = {}\n\
args = {}\n\
startup_timeout_sec = 120\n\
{WISP_BLOCK_END}",
        toml_string(&bridge.command),
        toml_string_array(&bridge.args)
    )
}

fn write_claude_mcp_config(config_path: &Path, bridge: &McpBridgeLaunch) -> Result<(), String> {
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
    }
    let body = serde_json::json!({
        "mcpServers": {
            "wisp_bridge": {
                "command": bridge.command,
                "args": bridge.args
            }
        }
    });
    let data = serde_json::to_vec_pretty(&body).map_err(|e| format!("{e}"))?;
    fs::write(config_path, data).map_err(|e| {
        format!(
            "Failed to write Claude MCP config '{}': {e}",
            config_path.display()
        )
    })
}

fn toml_string_array(values: &[String]) -> String {
    let inner = values
        .iter()
        .map(|s| toml_string(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

pub fn is_image_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    matches!(
        lower.rsplit('.').next(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    )
}

pub fn build_prompt(
    project_root: &Path,
    history: &[Message],
    user_message: &str,
    attachments: &[String],
) -> String {
    let use_wsl = should_use_wsl(project_root);
    let wire_root = if use_wsl {
        to_wsl_path(project_root).unwrap_or_else(|| project_root.display().to_string())
    } else {
        project_root.display().to_string()
    };
    let mut out = String::new();
    out.push_str("# Wisp local runner\n\n");
    out.push_str("You are running as a local agent for wisp-science. Complete the user's scientific analysis task using the local workspace and your configured tools.\n\n");
    out.push_str("Rules:\n");
    out.push_str("- Do not wait for interactive approval; make reasonable progress within the configured sandbox.\n");
    out.push_str("- Treat attached files as authoritative input data.\n");
    out.push_str("- Wisp skills and Wisp MCP tools are exposed through an MCP server named `wisp_bridge`; when a task needs Wisp capabilities, call `wisp_list_skills`, `wisp_use_skill`, or the bridged MCP tools instead of guessing whether tools exist.\n");
    out.push_str("- Save generated reports, tables, figures, or code artifacts under the project workspace when useful.\n");
    out.push_str("- In the final answer, summarize what you did and mention important output file paths.\n\n");
    out.push_str(&format!("Project workspace: {}\n\n", wire_root));
    if !attachments.is_empty() {
        out.push_str("Attached files:\n");
        for path in attachments {
            let kind = if is_image_path(path) {
                "image passed via --image"
            } else {
                "file path"
            };
            let wire_path = if use_wsl {
                to_wsl_path(Path::new(path)).unwrap_or_else(|| path.clone())
            } else {
                path.clone()
            };
            out.push_str(&format!("- {wire_path} ({kind})\n"));
        }
        out.push('\n');
    }
    let turns = compact_history(history);
    if !turns.is_empty() {
        out.push_str("Recent conversation context:\n\n");
        out.push_str(&turns);
        out.push('\n');
    }
    out.push_str("Current user request:\n\n");
    out.push_str(user_message.trim());
    out.push('\n');
    out
}

fn compact_history(history: &[Message]) -> String {
    let mut lines = Vec::new();
    let keep = history.iter().rev().take(24).cloned().collect::<Vec<_>>();
    for msg in keep.into_iter().rev() {
        match msg.role {
            Role::System => {}
            Role::User => push_history(&mut lines, "User", &msg.content.as_text()),
            Role::Assistant => push_history(&mut lines, "Assistant", &msg.content.as_text()),
            Role::Tool => {
                let name = msg.tool_name.as_deref().unwrap_or("tool");
                push_history(&mut lines, &format!("Tool {name}"), &msg.content.as_text());
            }
        }
    }
    lines.join("\n\n")
}

fn push_history(lines: &mut Vec<String>, role: &str, text: &str) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    let t = truncate(t, 4_000);
    lines.push(format!("## {role}\n{t}"));
}

fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let head = limit.saturating_sub(160);
    format!(
        "{}\n...[truncated]...\n{}",
        &text[..floor_boundary(text, head)],
        &text[floor_boundary(text, text.len().saturating_sub(120))..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

pub fn parse_codex_jsonl(line: &str) -> Vec<RunnerEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let mut events = Vec::new();
    let typ = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if typ == "error" {
        if let Some(msg) = v.get("message").and_then(|v| v.as_str()) {
            events.push(RunnerEvent::Reasoning(msg.to_string()));
        }
        return events;
    }
    if typ == "turn.completed" {
        if let Some((input, output)) = usage_from(&v) {
            events.push(RunnerEvent::Usage { input, output });
        }
    }
    if typ == "turn.failed" {
        let msg = v
            .get("error")
            .or_else(|| v.get("message"))
            .map(value_preview)
            .unwrap_or_else(|| "Codex turn failed".into());
        events.push(RunnerEvent::Error(msg));
    }
    let item = v.get("item").unwrap_or(&v);
    parse_item(item, &mut events);
    events
}

pub fn codex_session_id_from_jsonl(line: &str) -> Option<String> {
    let v = serde_json::from_str::<Value>(line).ok()?;
    find_codex_session_id(&v)
}

fn find_codex_session_id(v: &Value) -> Option<String> {
    match v {
        Value::Object(map) => {
            for key in [
                "session_id",
                "sessionId",
                "session",
                "conversation_id",
                "conversationId",
                "thread_id",
                "threadId",
            ] {
                if let Some(id) = map
                    .get(key)
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    return Some(id.to_string());
                }
            }
            map.values().find_map(find_codex_session_id)
        }
        Value::Array(items) => items.iter().find_map(find_codex_session_id),
        _ => None,
    }
}

pub fn parse_claude_jsonl(line: &str) -> Vec<RunnerEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let mut events = Vec::new();
    let typ = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match typ {
        "assistant" => {
            if let Some(message) = v.get("message") {
                parse_claude_message(message, &mut events);
            }
        }
        "user" => {
            if let Some(message) = v.get("message") {
                parse_claude_tool_results(message, &mut events);
            }
        }
        "result" => {
            if let Some((input, output)) = usage_from(&v) {
                events.push(RunnerEvent::Usage { input, output });
            }
            let subtype = v.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            if subtype.starts_with("error") {
                let msg = v
                    .get("error")
                    .or_else(|| v.get("result"))
                    .map(value_preview)
                    .unwrap_or_else(|| "Claude Code runner failed".into());
                events.push(RunnerEvent::Error(msg));
            }
        }
        "error" => {
            let msg = v
                .get("message")
                .or_else(|| v.get("error"))
                .map(value_preview)
                .unwrap_or_else(|| "Claude Code runner failed".into());
            events.push(RunnerEvent::Error(msg));
        }
        _ => {}
    }
    events
}

fn parse_claude_message(message: &Value, events: &mut Vec<RunnerEvent>) {
    if let Some((input, output)) = usage_from(message) {
        events.push(RunnerEvent::Usage { input, output });
    }
    let Some(content) = message.get("content").and_then(|v| v.as_array()) else {
        return;
    };
    for part in content {
        match part.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "text" => {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        events.push(RunnerEvent::Text(text.to_string()));
                    }
                }
            }
            "thinking" => {
                if let Some(text) = part.get("thinking").and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        events.push(RunnerEvent::Reasoning(text.to_string()));
                    }
                }
            }
            "tool_use" => {
                let name = part
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let preview = part.get("input").map(value_preview).unwrap_or_default();
                events.push(RunnerEvent::ToolCall {
                    name: format!("claude.{name}"),
                    preview,
                });
            }
            _ => {}
        }
    }
}

fn parse_claude_tool_results(message: &Value, events: &mut Vec<RunnerEvent>) {
    let Some(content) = message.get("content").and_then(|v| v.as_array()) else {
        return;
    };
    for part in content {
        if part.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let ok = part
            .get("is_error")
            .and_then(|v| v.as_bool())
            .map(|is_error| !is_error)
            .unwrap_or(true);
        let content = part
            .get("content")
            .map(value_preview)
            .unwrap_or_else(|| "tool result".into());
        events.push(RunnerEvent::ToolResult {
            name: "claude.tool".into(),
            ok,
            content,
        });
    }
}

fn parse_item(item: &Value, events: &mut Vec<RunnerEvent>) {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match item_type {
        "agent_message" | "message" => {
            if let Some(text) = item_text(item, &["text", "content"]) {
                events.push(RunnerEvent::Text(text));
            }
        }
        "reasoning" => {
            if let Some(text) = item_text(item, &["text", "summary", "content"]) {
                events.push(RunnerEvent::Reasoning(text));
            }
        }
        "command_execution" => parse_command_item(item, events),
        "mcp_tool_call" | "tool_call" => parse_tool_item(item, events),
        "file_change" | "file_changes" | "patch" => parse_file_item(item, events),
        _ => {}
    }
}

fn item_text(item: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = item.get(*key) else {
            continue;
        };
        if let Some(text) = value_text(value) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn value_text(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    let arr = v.as_array()?;
    let text = arr
        .iter()
        .filter_map(|part| {
            part.get("text")
                .or_else(|| part.get("content"))
                .and_then(|v| v.as_str())
        })
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn parse_command_item(item: &Value, events: &mut Vec<RunnerEvent>) {
    let command = item
        .get("command")
        .map(value_preview)
        .unwrap_or_else(|| "command".into());
    let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "in_progress" || status == "started" {
        events.push(RunnerEvent::ToolCall {
            name: "codex.command".into(),
            preview: command,
        });
        return;
    }
    let content = item
        .get("output")
        .or_else(|| item.get("stdout"))
        .or_else(|| item.get("stderr"))
        .map(value_preview)
        .unwrap_or_else(|| command.clone());
    events.push(RunnerEvent::ToolResult {
        name: "codex.command".into(),
        ok: status != "failed",
        content,
    });
}

fn parse_tool_item(item: &Value, events: &mut Vec<RunnerEvent>) {
    let name = item
        .get("name")
        .or_else(|| item.get("tool_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("codex.tool")
        .to_string();
    let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
    if status == "in_progress" || status == "started" {
        events.push(RunnerEvent::ToolCall {
            name,
            preview: item.get("arguments").map(value_preview).unwrap_or_default(),
        });
    } else {
        events.push(RunnerEvent::ToolResult {
            name,
            ok: status != "failed",
            content: item
                .get("output")
                .or_else(|| item.get("result"))
                .map(value_preview)
                .unwrap_or_default(),
        });
    }
}

fn parse_file_item(item: &Value, events: &mut Vec<RunnerEvent>) {
    if let Some(path) = item
        .get("path")
        .or_else(|| item.get("file"))
        .and_then(|v| v.as_str())
    {
        events.push(RunnerEvent::Diff { path: path.into() });
    }
    if let Some(paths) = item.get("paths").and_then(|v| v.as_array()) {
        for path in paths.iter().filter_map(|v| v.as_str()) {
            events.push(RunnerEvent::Diff { path: path.into() });
        }
    }
}

fn usage_from(v: &Value) -> Option<(u64, u64)> {
    let usage = v.get("usage")?;
    let input = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some((input, output))
}

fn value_preview(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    serde_json::to_string(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_distribution_helpers_are_case_insensitive_and_reject_cross_distro_paths() {
        let project = Path::new(r"\\WSL.LocalHost\Ubuntu-24.04\home\research\project");
        assert_eq!(
            wsl_distribution_for(project).as_deref(),
            Some("Ubuntu-24.04")
        );
        assert_eq!(
            to_wsl_path_for(project, Some("ubuntu-24.04")).unwrap(),
            Some("/home/research/project".into())
        );
        let error = to_wsl_path_for(project, Some("Debian")).unwrap_err();
        assert!(error.contains("Ubuntu-24.04"));
        assert!(error.contains("Debian"));

        let legacy_unc = Path::new(r"\\wSl$\uBuNtU\home\research\figure.png");
        assert_eq!(
            to_wsl_path_for(legacy_unc, Some("UBUNTU")).unwrap(),
            Some("/home/research/figure.png".into())
        );
    }

    #[test]
    fn builds_wsl_codex_command_for_unc_path() {
        let settings = LocalRunnerSettings {
            command: String::new(),
            profile: "glm".into(),
            sandbox: String::new(),
            web_search: true,
            model: "inherit".into(),
            ..Default::default()
        };
        let cmd = build_codex_command(
            &settings,
            Path::new(r"\\wsl.localhost\Ubuntu\home\ljx\proj"),
            &["/home/ljx/proj/a.png".into()],
            None,
        );
        assert_eq!(cmd.program, "wsl.exe");
        assert!(cmd.args.contains(&"--search".into()));
        assert!(cmd.args.contains(&"--profile".into()));
        assert!(!cmd.args.iter().any(|arg| arg == "--sandbox"));
        assert!(!cmd.args.contains(&"danger-full-access".into()));
        assert!(cmd.args.contains(&"--image".into()));
        assert!(
            cmd.args.iter().position(|a| a == "-").unwrap()
                < cmd.args.iter().position(|a| a == "--image").unwrap()
        );
        assert_eq!(cmd.prompt_cwd, "/home/ljx/proj");
    }

    #[test]
    fn explicit_command_is_respected() {
        let settings = LocalRunnerSettings {
            command: "wsl.exe -e codex".into(),
            sandbox: "workspace-write".into(),
            model: "gpt-5.4".into(),
            normal_model: "gpt-5.4".into(),
            ..Default::default()
        };
        let cmd = build_codex_command(&settings, Path::new("C:/repo"), &[], None);
        assert_eq!(cmd.program, "wsl.exe");
        assert_eq!(&cmd.args[..2], ["-e", "codex"]);
        assert!(cmd.args.contains(&"--model".into()));
        assert!(cmd.args.contains(&"gpt-5.4".into()));
        assert!(cmd.args.contains(&"workspace-write".into()));
    }

    #[test]
    fn quoted_windows_runner_path_keeps_backslashes_and_spaces() {
        let settings = LocalRunnerSettings {
            command: r#""C:\Program Files\OpenAI Codex\codex.exe" --channel desktop"#.into(),
            ..Default::default()
        };
        let cmd = build_codex_command(&settings, Path::new("C:/repo"), &[], None);
        assert_eq!(cmd.program, r"C:\Program Files\OpenAI Codex\codex.exe");
        assert_eq!(&cmd.args[..2], ["--channel", "desktop"]);
    }

    #[test]
    fn exec_fallback_emits_only_wisp_overrides() {
        let settings = LocalRunnerSettings {
            command: "codex".into(),
            normal_model: "gpt-5.6-sol".into(),
            normal_reasoning_effort: "ultra".into(),
            web_search_mode: "cached".into(),
            service_tier: "priority".into(),
            personality: "pragmatic".into(),
            reasoning_summary: "concise".into(),
            verbosity: "low".into(),
            ..Default::default()
        };
        let cmd = build_codex_command(&settings, Path::new("C:/repo"), &[], None);
        for expected in [
            "web_search=\"cached\"",
            "model_reasoning_effort=\"ultra\"",
            "service_tier=\"priority\"",
            "personality=\"pragmatic\"",
            "model_reasoning_summary=\"concise\"",
            "model_verbosity=\"low\"",
        ] {
            assert!(cmd.args.contains(&expected.into()), "missing {expected:?}");
        }
        assert!(cmd.args.windows(2).any(|w| w == ["--model", "gpt-5.6-sol"]));
        assert!(!cmd.args.contains(&"--search".into()));
    }

    #[test]
    fn plan_fallback_resolves_plan_model_without_overwriting_normal() {
        let settings = LocalRunnerSettings {
            command: "codex".into(),
            normal_model: "gpt-normal".into(),
            normal_reasoning_effort: "high".into(),
            plan_model: "gpt-plan".into(),
            plan_reasoning_effort: "medium".into(),
            ..Default::default()
        };
        let normal = build_codex_command(&settings, Path::new("C:/repo"), &[], None);
        let plan = build_codex_command_for_mode(
            &settings,
            RunnerModelMode::Plan,
            Path::new("C:/repo"),
            &[],
            None,
        );
        assert!(normal.args.contains(&"gpt-normal".into()));
        assert!(normal
            .args
            .contains(&"model_reasoning_effort=\"high\"".into()));
        assert!(plan.args.contains(&"gpt-plan".into()));
        assert!(plan
            .args
            .contains(&"model_reasoning_effort=\"medium\"".into()));
        assert!(!plan.args.contains(&"gpt-normal".into()));
    }

    #[test]
    fn compatibility_plan_disables_notify_hooks_before_exec() {
        let mut command = build_codex_command_for_mode(
            &LocalRunnerSettings {
                command: "codex".into(),
                ..Default::default()
            },
            RunnerModelMode::Plan,
            Path::new("C:/repo"),
            &[],
            None,
        );
        enforce_plan_mcp_isolation(
            &mut command,
            &McpBridgeLaunch {
                command: "wisp-tauri".into(),
                args: vec!["--wisp-mcp-bridge".into(), "--plan-safe".into()],
            },
        );
        let exec = command
            .args
            .iter()
            .position(|arg| arg == "exec")
            .expect("Codex compatibility command must contain exec");
        let global = &command.args[..exec];
        assert!(!global.iter().any(|arg| arg == "--ignore-rules"));
        assert_eq!(
            command.args.get(exec + 1).map(String::as_str),
            Some("--ignore-rules")
        );
        for expected in [
            "project_root_markers=[\".wisp\"]",
            "mcp_servers={}",
            "features.plugins=false",
            "features.remote_plugin=false",
            "features.apps=false",
            "features.computer_use=false",
            "features.browser_use=false",
            "features.browser_use_external=false",
            "features.browser_use_full_cdp_access=false",
            "features.in_app_browser=false",
            "features.image_generation=false",
            "features.code_mode=false",
            "features.code_mode_only=false",
            "features.code_mode_host=false",
            "features.enable_mcp_apps=false",
            "features.auth_elicitation=false",
            "features.tool_call_mcp_elicitation=false",
            "features.hooks=false",
            "features.codex_hooks=false",
            "features.shell_snapshot=false",
            "features.skill_mcp_dependency_install=false",
            "features.multi_agent=false",
            "features.multi_agent_v2=false",
            "features.enable_fanout=false",
            "notify=[]",
        ] {
            assert!(
                global.windows(2).any(|pair| pair == ["-c", expected]),
                "missing Plan process-isolation override {expected}"
            );
        }
    }

    #[test]
    fn codex_resume_uses_external_session_id() {
        let settings = LocalRunnerSettings {
            sandbox: "workspace-write".into(),
            model: "gpt-5.4".into(),
            normal_model: "gpt-5.4".into(),
            persistent: true,
            ..Default::default()
        };
        let cmd = build_codex_command(
            &settings,
            Path::new("/repo"),
            &["fig.png".into()],
            Some("sid-1"),
        );
        assert!(cmd.args.windows(2).any(|w| w == ["exec", "resume"]));
        assert!(cmd.args.contains(&"sid-1".into()));
        assert!(cmd.args.contains(&"--image".into()));
        assert!(cmd.args.contains(&"workspace-write".into()));
    }

    #[test]
    fn prompt_includes_attachments_and_history() {
        let history = vec![
            Message::user("previous question"),
            Message::assistant("previous answer"),
        ];
        let prompt = build_prompt(
            Path::new("/tmp/proj"),
            &history,
            "analyze this",
            &["a.csv".into(), "b.png".into()],
        );
        assert!(prompt.contains("previous question"));
        assert!(prompt.contains("a.csv"));
        assert!(prompt.contains("image passed via --image"));
        assert!(prompt.contains("wisp_bridge"));
        assert!(prompt.contains("analyze this"));
    }

    #[test]
    fn parses_agent_message_and_usage() {
        let events = parse_codex_jsonl(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#,
        );
        assert_eq!(events, vec![RunnerEvent::Text("done".into())]);
        let events = parse_codex_jsonl(
            r#"{"type":"turn.completed","usage":{"input_tokens":7,"output_tokens":3}}"#,
        );
        assert_eq!(
            events,
            vec![RunnerEvent::Usage {
                input: 7,
                output: 3
            }]
        );
        let events = parse_codex_jsonl(
            r#"{"type":"item.completed","item":{"type":"message","content":[{"type":"output_text","text":"hello"}]}}"#,
        );
        assert_eq!(events, vec![RunnerEvent::Text("hello".into())]);
        let events = parse_codex_jsonl(r#"{"type":"error","message":"Reconnecting..."}"#);
        assert_eq!(
            events,
            vec![RunnerEvent::Reasoning("Reconnecting...".into())]
        );
    }

    #[test]
    fn parses_command_and_diff() {
        let events = parse_codex_jsonl(
            r#"{"type":"item.started","item":{"type":"command_execution","command":"ls","status":"in_progress"}}"#,
        );
        assert_eq!(
            events,
            vec![RunnerEvent::ToolCall {
                name: "codex.command".into(),
                preview: "ls".into()
            }]
        );
        let events = parse_codex_jsonl(
            r#"{"type":"item.completed","item":{"type":"file_change","path":"out.md"}}"#,
        );
        assert_eq!(
            events,
            vec![RunnerEvent::Diff {
                path: "out.md".into()
            }]
        );
    }

    #[test]
    fn builds_and_parses_claude_code_runner() {
        let settings = LocalRunnerSettings {
            model: "claude-sonnet-5".into(),
            normal_model: "claude-sonnet-5".into(),
            claude_command: "claude.exe --dangerously-skip-permissions".into(),
            persistent: true,
            ..Default::default()
        };
        let cmd = build_claude_code_command(
            &settings,
            Path::new("C:/repo"),
            Some("123e4567-e89b-12d3-a456-426614174000"),
        );
        assert_eq!(cmd.program, "claude.exe");
        assert!(cmd.args.contains(&"-p".into()));
        assert!(cmd.args.contains(&"stream-json".into()));
        assert!(cmd.args.contains(&"--model".into()));
        assert!(cmd.args.contains(&"--session-id".into()));
        assert!(cmd
            .args
            .contains(&"123e4567-e89b-12d3-a456-426614174000".into()));
        let events = parse_claude_jsonl(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","name":"Bash","input":{"command":"pwd"}}],"usage":{"input_tokens":4,"output_tokens":2}}}"#,
        );
        assert_eq!(
            events,
            vec![
                RunnerEvent::Usage {
                    input: 4,
                    output: 2
                },
                RunnerEvent::Text("hi".into()),
                RunnerEvent::ToolCall {
                    name: "claude.Bash".into(),
                    preview: r#"{"command":"pwd"}"#.into()
                }
            ]
        );
    }

    #[test]
    fn adds_claude_mcp_config_without_dropping_session_args() {
        let settings = LocalRunnerSettings {
            claude_command: "claude".into(),
            persistent: true,
            ..Default::default()
        };
        let mut cmd = build_claude_code_command(&settings, Path::new("C:/repo"), Some("sid"));
        add_claude_mcp_config(
            &mut cmd,
            Path::new("C:/repo/.wisp/claude-home/mcp.json"),
            Path::new("C:/repo"),
        );
        assert!(cmd.args.contains(&"--session-id".into()));
        assert!(cmd.args.contains(&"sid".into()));
        assert!(cmd.args.contains(&"--mcp-config".into()));
        assert!(cmd
            .args
            .contains(&"C:/repo/.wisp/claude-home/mcp.json".into()));
    }

    #[test]
    fn extracts_codex_session_id_from_jsonl() {
        assert_eq!(
            codex_session_id_from_jsonl(r#"{"type":"session.created","session_id":"abc-123"}"#)
                .as_deref(),
            Some("abc-123")
        );
        assert_eq!(
            codex_session_id_from_jsonl(r#"{"type":"event","payload":{"threadId":"thread-7"}}"#)
                .as_deref(),
            Some("thread-7")
        );
    }

    #[test]
    fn codex_config_block_preserves_user_config_and_replaces_old_block() {
        let bridge = McpBridgeLaunch {
            command: r"C:\Wisp\wisp-tauri.exe".into(),
            args: vec![
                "--wisp-mcp-bridge".into(),
                "--project-root".into(),
                r"C:\repo".into(),
            ],
        };
        let original = r#"model = "gpt-5"

# BEGIN WISP BUILTINS
[mcp_servers.wisp_bridge]
command = "old"
# END WISP BUILTINS

[profiles.default]
model = "local"
"#;
        let updated = replace_marked_block(original, &codex_config_block(&bridge));
        assert!(updated.contains(r#"model = "gpt-5""#));
        assert!(updated.contains("[profiles.default]"));
        assert!(updated.contains("transport = \"stdio\""));
        assert!(updated.contains("startup_timeout_sec = 120"));
        assert!(!updated.contains("command = \"old\""));
        assert!(updated.contains("wisp-tauri.exe"));
    }

    #[test]
    fn sync_cli_home_skips_cache_and_copies_config_assets() {
        let base = std::env::temp_dir().join(format!(
            "wisp-runner-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("skills").join("s")).unwrap();
        std::fs::create_dir_all(src.join("plugins").join("p")).unwrap();
        std::fs::create_dir_all(src.join("rules")).unwrap();
        std::fs::create_dir_all(src.join("cache")).unwrap();
        std::fs::create_dir_all(src.join("sessions")).unwrap();
        std::fs::write(src.join("config.toml"), "model = 'x'").unwrap();
        std::fs::write(src.join("auth.json"), "{}").unwrap();
        std::fs::write(src.join("skills").join("s").join("SKILL.md"), "body").unwrap();
        std::fs::write(src.join("plugins").join("p").join("plugin.json"), "{}").unwrap();
        std::fs::write(src.join("rules").join("allow.rules"), "allow").unwrap();
        std::fs::write(src.join("cache").join("stale"), "no").unwrap();
        std::fs::write(src.join("sessions").join("thread.jsonl"), "no").unwrap();
        std::fs::write(src.join("state_5.sqlite"), "no").unwrap();
        std::fs::write(src.join("state_5.sqlite-wal"), "no").unwrap();
        std::fs::write(src.join("logs_2.sqlite"), "no").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        sync_cli_home(&src, &dst, CODEX_STATIC_CONFIG_DIRS).unwrap();
        assert!(dst.join("config.toml").is_file());
        assert!(dst.join("auth.json").is_file());
        assert!(dst.join("skills").join("s").join("SKILL.md").is_file());
        assert!(!dst.join("plugins").exists());
        assert!(!dst.join("rules").exists());
        assert!(!dst.join("cache").exists());
        assert!(!dst.join("sessions").exists());
        assert!(!dst.join("state_5.sqlite").exists());
        assert!(!dst.join("state_5.sqlite-wal").exists());
        assert!(!dst.join("logs_2.sqlite").exists());
        assert!(dst.join(SYNC_MANIFEST).is_file());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn compatibility_codex_config_strips_external_process_launchers() {
        let base = std::env::temp_dir().join(format!(
            "wisp-runner-config-isolation-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let config = base.join("config.toml");
        std::fs::write(
            &config,
            r#"
model = "gpt-custom"
experimental_thread_config_endpoint = "https://unsafe.invalid/config"
[mcp_servers.unsafe]
command = "unsafe.exe"
[plugins.unsafe]
enabled = true
[marketplaces.unsafe]
source = "C:/unsafe"
[model_providers.custom]
base_url = "https://example.invalid"
[model_providers.custom.auth]
command = "token-helper.exe"
args = ["token"]
token_env = "SAFE_TOKEN"
"#,
        )
        .unwrap();
        strip_codex_external_process_config(&config).unwrap();
        let parsed = std::fs::read_to_string(&config)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        let table = parsed.as_table().unwrap();
        assert_eq!(table["model"].as_str(), Some("gpt-custom"));
        for removed in [
            "mcp_servers",
            "plugins",
            "marketplaces",
            "experimental_thread_config_endpoint",
        ] {
            assert!(!table.contains_key(removed), "{removed} must be removed");
        }
        let auth = table["model_providers"]["custom"]["auth"]
            .as_table()
            .expect("benign auth fields remain");
        assert_eq!(auth["token_env"].as_str(), Some("SAFE_TOKEN"));
        assert!(!auth.contains_key("command"));
        assert!(!auth.contains_key("args"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn compatibility_plan_rejects_external_processes_from_active_project_config() {
        let base = std::env::temp_dir().join(format!(
            "wisp-runner-project-config-audit-{}",
            uuid::Uuid::new_v4()
        ));
        let nested = base.join("repo").join("nested");
        let dot_codex = nested.join(".codex");
        let parent_dot_codex = base.join("repo").join(".codex");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(nested.join(".wisp")).unwrap();
        std::fs::create_dir_all(&dot_codex).unwrap();
        std::fs::create_dir_all(&parent_dot_codex).unwrap();
        // The `.wisp` project-root marker prevents this unrelated parent layer
        // from being effective for the active Wisp project.
        std::fs::write(
            parent_dot_codex.join("config.toml"),
            "[mcp_servers.parent]\ncommand = 'must-not-load.exe'\n",
        )
        .unwrap();
        let config = dot_codex.join("config.toml");
        std::fs::write(&config, "model = 'safe'\nnotify = []\n").unwrap();
        audit_codex_project_external_process_config(&nested).unwrap();

        std::fs::write(&config, "[mcp_servers.unsafe]\ncommand = 'unsafe.exe'\n").unwrap();
        let error = audit_codex_project_external_process_config(&nested).unwrap_err();
        assert!(error.contains("mcp_servers"));
        assert!(error.contains("config.toml"));

        std::fs::write(
            &config,
            "[model_providers.custom.auth]\ncommand = 'token-helper.exe'\n",
        )
        .unwrap();
        let error = audit_codex_project_external_process_config(&nested).unwrap_err();
        assert!(error.contains("auth.command"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn runtime_env_is_added_to_command() {
        let mut cmd = build_codex_command(
            &LocalRunnerSettings {
                command: "codex".into(),
                ..Default::default()
            },
            Path::new("C:/repo"),
            &[],
            None,
        );
        let rt = RunnerRuntime {
            home_dir: PathBuf::from("C:/repo/.wisp/codex-home"),
            config_path: PathBuf::from("C:/repo/.wisp/codex-home/config.toml"),
            env: vec![("CODEX_HOME".into(), "C:/repo/.wisp/codex-home".into())],
            diagnostics: vec![],
        };
        apply_runtime_env(&mut cmd, &rt);
        assert_eq!(
            cmd.env,
            vec![("CODEX_HOME".into(), "C:/repo/.wisp/codex-home".into())]
        );
    }
}
