//! Per-project Codex CLI runtime for the `codex` tool.
//!
//! Prepares `<project>/.wisp/codex-home`, seeded from the user's `~/.codex`
//! (so auth and preferences carry over), and injects a marked `wisp_bridge`
//! MCP block into the copied `config.toml` so Codex can reach Wisp's skills,
//! bundled bio MCP, and custom MCP connections via the stdio bridge.
//!
//! Ported from the runner-as-provider work in #135 (experimental/local-runners,
//! author jarxunlai), trimmed to the codex-as-tool scope.
// ponytail: no WSL path translation here — the tool spawns codex on the host;
// port runner_env_path/to_wsl_path from experimental/local-runners if WSL
// codex setups show up.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

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

pub fn prepare_codex_runtime(
    project_root: &Path,
    bridge: &McpBridgeLaunch,
) -> Result<RunnerRuntime, String> {
    prepare_codex_runtime_for_profile(project_root, "default", Some(bridge))
}

/// Prepare the isolated Codex home used by a concrete Wisp profile.  Keeping
/// profiles separate prevents a model/runtime override in one profile from
/// leaking into another.  `bridge=None` is used by App Server clients that
/// register Wisp tools dynamically instead of editing the MCP config.
pub fn prepare_codex_runtime_for_profile(
    project_root: &Path,
    profile_id: &str,
    bridge: Option<&McpBridgeLaunch>,
) -> Result<RunnerRuntime, String> {
    prepare_codex_runtime_for_profile_from_source(project_root, profile_id, bridge, None)
}

/// Variant used by WSL runners. `source_override` must point at the selected
/// runtime's own Codex home (for example `\\wsl.localhost\Ubuntu\home\me\.codex`),
/// never the Windows user's home. The target remains the project-local
/// `.wisp/codex-home/<profile>` directory.
pub fn prepare_codex_runtime_for_profile_from_source(
    project_root: &Path,
    profile_id: &str,
    bridge: Option<&McpBridgeLaunch>,
    source_override: Option<&Path>,
) -> Result<RunnerRuntime, String> {
    let home_dir = profile_runtime_home(project_root, profile_id);
    prepare_runtime_dir_under_project(project_root, &home_dir)?;
    let source = source_override
        .map(Path::to_path_buf)
        .or_else(selected_codex_home_source);
    let mut diagnostics = Vec::new();
    let synchronized_source = match source.as_deref() {
        Some(src) => match fs::metadata(src) {
            Ok(meta) if meta.is_dir() => {
                sync_cli_home(src, &home_dir)?;
                Some(src)
            }
            Ok(_) => {
                clear_manifest_managed_assets(&home_dir)?;
                diagnostics.push(format!(
                    "Local Codex config path is not a directory: {}. Inherited assets were cleared.",
                    src.display()
                ));
                None
            }
            Err(error) => {
                clear_manifest_managed_assets(&home_dir)?;
                diagnostics.push(format!(
                    "Local Codex config directory is unavailable: {} ({error}). Inherited assets were cleared.",
                    src.display()
                ));
                None
            }
        },
        None => {
            clear_manifest_managed_assets(&home_dir)?;
            diagnostics.push(
                "Cannot locate user home directory; inherited assets were cleared and Wisp generated a minimal CODEX_HOME."
                    .into(),
            );
            None
        }
    };
    let config_path = home_dir.join("config.toml");
    if let Some(src) = synchronized_source {
        rewrite_config_file_references_for_runtime(&config_path, src, &home_dir, None, None)?;
    }
    // Never inherit arbitrary global MCP processes into the isolated actor.
    // Native Plan's filesystem sandbox cannot constrain an external MCP
    // server, so retaining these entries would make the UI's read-only promise
    // false. Wisp MCP connectors are supplied through the scoped dynamic
    // router (App Server) or the plan-safe bridge (exec fallback) below.
    if config_path.is_file() {
        strip_external_process_config(&config_path, &home_dir)?;
        diagnostics.push(
            "Global Codex MCP/plugin launchers, marketplaces, thread config endpoints, and provider auth commands are disabled inside Wisp; use Wisp-scoped connectors so Plan read-only policy can be enforced."
                .into(),
        );
    }
    if let Some(bridge) = bridge {
        prepare_managed_file_destination(&home_dir, &config_path)?;
        inject_codex_config_block(&config_path, bridge)?;
        let mut manifest = read_sync_manifest(&home_dir)?;
        manifest
            .entries
            .insert("config.toml".into(), "wisp-bridge".into());
        write_sync_manifest(&home_dir, &manifest)?;
    }
    let env_home = home_dir.display().to_string();
    Ok(RunnerRuntime {
        home_dir,
        config_path,
        env: vec![("CODEX_HOME".into(), env_home)],
        diagnostics,
    })
}

pub(crate) fn profile_runtime_home(project_root: &Path, profile_id: &str) -> PathBuf {
    project_root
        .join(".wisp")
        .join("codex-home")
        .join(safe_profile_dir(profile_id))
}

fn strip_external_process_config(config_path: &Path, target_home: &Path) -> Result<(), String> {
    let config = fs::read_to_string(config_path).map_err(|error| {
        format!(
            "Failed to read isolated Codex config '{}': {error}",
            config_path.display()
        )
    })?;
    let mut document = config.parse::<toml::Value>().map_err(|error| {
        format!(
            "Failed to parse isolated Codex config '{}' while enforcing external-process isolation: {error}",
            config_path.display()
        )
    })?;
    let mut changed = false;
    if let Some(table) = document.as_table_mut() {
        for key in [
            "mcp_servers",
            "plugins",
            "marketplaces",
            "experimental_thread_config_endpoint",
            "notify",
            "hooks",
        ] {
            changed |= table.remove(key).is_some();
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
                            changed |= auth.remove(key).is_some();
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
    if changed {
        let serialized = toml::to_string_pretty(&document).map_err(|error| error.to_string())?;
        atomic_write_managed(target_home, config_path, serialized.as_bytes())?;
    }
    Ok(())
}

fn safe_profile_dir(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || raw == "default" {
        return "default".into();
    }
    let stem: String = trimmed
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(63)
        .collect();
    let stem = if stem.is_empty() { "profile" } else { &stem };
    format!("{stem}-{:016x}", stable_id_hash(raw.as_bytes()))
}

fn stable_id_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn prepare_runtime_dir(home_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(home_dir).map_err(|e| {
        format!(
            "Failed to create codex runtime '{}': {e}",
            home_dir.display()
        )
    })?;
    let meta = fs::symlink_metadata(home_dir).map_err(|e| {
        format!(
            "Failed to inspect codex runtime '{}': {e}",
            home_dir.display()
        )
    })?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "Refusing non-directory or linked codex runtime '{}'",
            home_dir.display()
        ));
    }
    Ok(())
}

fn prepare_runtime_dir_under_project(project_root: &Path, home_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(project_root).map_err(|error| {
        format!(
            "Failed to create project root '{}': {error}",
            project_root.display()
        )
    })?;
    let canonical_project = fs::canonicalize(project_root).map_err(|error| {
        format!(
            "Failed to resolve project root '{}': {error}",
            project_root.display()
        )
    })?;
    let expected = [
        project_root.join(".wisp"),
        project_root.join(".wisp").join("codex-home"),
        home_dir.to_path_buf(),
    ];
    for path in expected {
        if let Err(error) = fs::create_dir(&path) {
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "Failed to create isolated Codex directory '{}': {error}",
                    path.display()
                ));
            }
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Failed to inspect '{}': {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "Refusing linked/reparse or non-directory Codex runtime ancestor '{}'",
                path.display()
            ));
        }
        let canonical = fs::canonicalize(&path).map_err(|error| error.to_string())?;
        if !canonical.starts_with(&canonical_project) {
            return Err(format!(
                "Refusing Codex runtime ancestor '{}' outside canonical project '{}'",
                canonical.display(),
                canonical_project.display()
            ));
        }
    }
    prepare_runtime_dir(home_dir)
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

/// Native source home selected by the actual Codex process environment.
/// Wisp never writes this directory; it is only the safe synchronization base
/// for the project-local isolated runtime.
pub(crate) fn selected_codex_home_source() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home_dir().map(|home| home.join(".codex")))
}

pub(crate) fn remove_profile_runtime(project_root: &Path, profile_id: &str) -> Result<(), String> {
    let target = project_root
        .join(".wisp")
        .join("codex-home")
        .join(safe_profile_dir(profile_id));
    let parent = project_root.join(".wisp").join("codex-home");
    for ancestor in [project_root.join(".wisp"), parent.clone()] {
        let metadata = match fs::symlink_metadata(&ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "Refusing to remove Codex runtime through linked ancestor '{}'",
                ancestor.display()
            ));
        }
    }
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() {
        fs::remove_file(&target)
            .or_else(|_| fs::remove_dir(&target))
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "Refusing non-directory Codex profile runtime '{}'",
            target.display()
        ));
    }
    remove_tree_without_following_links(&target)
}

/// One-time, thread-scoped lineage migration. This deliberately copies only
/// the rollout whose filename contains the exact stored thread UUID; it never
/// mirrors the sessions tree, DB/WAL, logs or history.
pub(crate) fn import_single_session_rollout(
    source_home: &Path,
    target_home: &Path,
    thread_id: &str,
) -> Result<bool, String> {
    if thread_id.is_empty()
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Refusing unsafe Codex thread id for rollout migration".into());
    }
    let source_sessions = source_home.join("sessions");
    if !source_sessions.is_dir() {
        return Ok(false);
    }
    let mut stack = vec![source_sessions.clone()];
    let mut matches = Vec::<PathBuf>::new();
    let mut budget = 100_000usize;
    while let Some(directory) = stack.pop() {
        if budget == 0 {
            return Err("Codex sessions scan exceeded the safe migration limit".into());
        }
        budget -= 1;
        for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() && entry.file_name().to_string_lossy().contains(thread_id)
            {
                matches.push(entry.path());
            }
        }
    }
    matches.sort();
    matches.dedup();
    let source = match matches.as_slice() {
        [] => return Ok(false),
        [source] => source,
        _ => {
            return Err(format!(
                "Multiple rollout files matched Codex thread '{thread_id}'; Wisp refused an ambiguous migration"
            ))
        }
    };
    let relative = source
        .strip_prefix(&source_sessions)
        .map_err(|_| "Rollout escaped the selected sessions root".to_string())?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("Refusing unsafe rollout migration path".into());
    }
    let destination = target_home.join("sessions").join(relative);
    let parent = destination
        .parent()
        .ok_or_else(|| "Invalid rollout destination".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    // Reject a linked destination path after creation; never follow a
    // repo-controlled reparse point while installing the one rollout.
    let mut cursor = target_home.to_path_buf();
    for component in Path::new("sessions")
        .join(relative)
        .parent()
        .into_iter()
        .flat_map(Path::components)
    {
        if let Component::Normal(part) = component {
            cursor.push(part);
            let metadata = fs::symlink_metadata(&cursor).map_err(|error| error.to_string())?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(format!(
                    "Refusing linked rollout destination '{}'",
                    cursor.display()
                ));
            }
        }
    }
    fs::copy(source, &destination).map_err(|error| {
        format!(
            "Failed to migrate Codex rollout '{}' to '{}': {error}",
            source.display(),
            destination.display()
        )
    })?;
    Ok(true)
}

fn remove_tree_without_following_links(path: &Path) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() || metadata.is_file() {
            fs::remove_file(&child)
                .or_else(|_| fs::remove_dir(&child))
                .map_err(|error| error.to_string())?;
        } else if metadata.is_dir() {
            remove_tree_without_following_links(&child)?;
        } else {
            fs::remove_file(&child).map_err(|error| error.to_string())?;
        }
    }
    fs::remove_dir(path).map_err(|error| error.to_string())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncManifest {
    #[serde(default)]
    entries: BTreeMap<String, String>,
}

const SYNC_MANIFEST: &str = ".wisp-sync.json";
const MANAGED_REFERENCES_DIR: &str = ".wisp-config-files";
// Keep this list intentionally small. In particular, do not mirror `plugins`:
// current Codex Desktop installations keep app-server binaries and cache trees
// there (hundreds of MB), and Wisp supplies its own dynamic tools instead.
const STATIC_DIRS: &[&str] = &["skills", "vendor_imports"];
// Directories copied by an older Wisp build. They remain valid manifest names
// solely so an upgrade can remove its own stale mirror; they are never seeded
// or fingerprinted again.
// `rules` is also retired: an explicit allow rule bypasses Codex's sandbox,
// including the built-in :read-only permission profile used by native Plan.
const RETIRED_STATIC_DIRS: &[&str] = &["plugins", "rules"];

fn read_sync_manifest(target: &Path) -> Result<SyncManifest, String> {
    let path = target.join(SYNC_MANIFEST);
    match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| format!("Invalid Codex sync manifest '{}': {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SyncManifest::default()),
        Err(error) => Err(format!(
            "Failed to read Codex sync manifest '{}': {error}",
            path.display()
        )),
    }
}

fn write_sync_manifest(target: &Path, manifest: &SyncManifest) -> Result<(), String> {
    let path = target.join(SYNC_MANIFEST);
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    atomic_write_managed(target, &path, &bytes)
        .map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

fn clear_manifest_managed_assets(target: &Path) -> Result<(), String> {
    let manifest = read_sync_manifest(target)?;
    for name in manifest.entries.keys() {
        if !is_safe_manifest_entry(name) {
            return Err(format!(
                "Refusing unsafe Codex sync manifest entry '{name}' in '{}'",
                target.join(SYNC_MANIFEST).display()
            ));
        }
        remove_managed_path(target, &target.join(name))?;
    }
    write_sync_manifest(target, &SyncManifest::default())
}

fn is_safe_manifest_entry(name: &str) -> bool {
    let mut components = Path::new(name).components();
    let Some(Component::Normal(_)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    allowed_root_file(name)
        || STATIC_DIRS.contains(&name)
        || RETIRED_STATIC_DIRS.contains(&name)
        || name == MANAGED_REFERENCES_DIR
}

/// Seed only known configuration/capability assets.  The previous recursive
/// deny-list copied state.sqlite, WAL files, sessions and logs from modern
/// Codex installations (often hundreds of MB) and could copy locked files.
fn sync_cli_home(source: &Path, target: &Path) -> Result<(), String> {
    let target_meta = fs::symlink_metadata(target).map_err(|e| {
        format!(
            "Failed to inspect isolated Codex home '{}': {e}",
            target.display()
        )
    })?;
    if target_meta.file_type().is_symlink() || !target_meta.is_dir() {
        return Err(format!(
            "Refusing to sync into non-directory or linked Codex home '{}'",
            target.display()
        ));
    }

    let previous = read_sync_manifest(target)?;
    let mut next = SyncManifest::default();
    if let Some(fingerprint) = previous.entries.get(MANAGED_REFERENCES_DIR) {
        if target.join(MANAGED_REFERENCES_DIR).is_dir() {
            next.entries
                .insert(MANAGED_REFERENCES_DIR.into(), fingerprint.clone());
        }
    }
    let mut source_assets = BTreeSet::new();
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
        let file_type = entry.file_type().map_err(|e| format!("{e}"))?;
        // Never follow links from the selected Codex home. Static assets are
        // copied as independent files so the isolated home cannot mutate (or
        // unexpectedly read through) the source home.
        if file_type.is_symlink() {
            continue;
        }
        let is_static_dir = file_type.is_dir() && STATIC_DIRS.contains(&name_s.as_ref());
        let is_config_file = file_type.is_file() && allowed_root_file(&name_s);
        if !is_static_dir && !is_config_file {
            continue;
        }
        let meta = entry.metadata().map_err(|e| format!("{e}"))?;
        source_assets.insert(name_s.to_string());
        let dest = target.join(&name);
        let fingerprint = if is_static_dir {
            tree_fingerprint(&path)?
        } else {
            metadata_fingerprint(&meta)
        };
        next.entries.insert(name_s.to_string(), fingerprint);
        if is_static_dir {
            // Always reconcile the directory itself. A source fingerprint can
            // stay unchanged while an interrupted prior copy or a local write
            // leaves extra files in the isolated home.
            sync_static_dir(&path, &dest, target)?;
        } else if !files_equal(&path, &dest)? {
            copy_file_replacing(&path, &dest, target)?;
        }
    }

    // Root assets are an allowlisted mirror, not an append-only cache. Remove
    // an inherited root file/static directory when it disappeared upstream,
    // while leaving every non-allowlisted runtime-state entry untouched.
    for entry in fs::read_dir(target).map_err(|e| {
        format!(
            "Failed to reconcile isolated Codex home '{}': {e}",
            target.display()
        )
    })? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let is_retired_mirror = RETIRED_STATIC_DIRS.contains(&name_s.as_ref())
            && previous.entries.contains_key(name_s.as_ref());
        let is_managed = STATIC_DIRS.contains(&name_s.as_ref())
            || allowed_root_file(&name_s)
            || is_retired_mirror;
        if is_managed && !source_assets.contains(name_s.as_ref()) {
            remove_managed_path(target, &entry.path())?;
        }
    }

    write_sync_manifest(target, &next)?;
    Ok(())
}

fn allowed_root_file(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "config.toml" | "auth.json" | "agents.md" | "instructions.md"
    ) || name.to_ascii_lowercase().ends_with(".config.toml")
}

/// Content fingerprint for the configuration/capability assets Wisp mirrors
/// from a selected Codex home, including safe absolute `*_file` references.
/// `auth.json` is intentionally synchronized on actor creation but excluded
/// here: Codex rotates credentials independently, and a token refresh must not
/// masquerade as a model/capability configuration change in the turn CAS.
pub(crate) fn source_assets_fingerprint(
    source: &Path,
    source_wire_home: Option<&str>,
) -> Result<String, String> {
    fn update(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }

    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("missing".into()),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Refusing linked or non-directory Codex source '{}'",
            source.display()
        ));
    }
    let canonical_source = fs::canonicalize(source).map_err(|error| error.to_string())?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut hash = 0xcbf29ce484222325u64;
    let mut configs = Vec::<PathBuf>::new();
    for entry in entries {
        let name = entry.file_name();
        let name_string = name.to_string_lossy();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_file()
            && allowed_root_file(&name_string)
            && !name_string.eq_ignore_ascii_case("auth.json")
        {
            update(&mut hash, name_string.as_bytes());
            let content = fs::read(entry.path()).map_err(|error| error.to_string())?;
            update(&mut hash, &content);
            if name_string.to_ascii_lowercase().ends_with("config.toml") {
                configs.push(entry.path());
            }
        } else if file_type.is_dir() && STATIC_DIRS.contains(&name_string.as_ref()) {
            update(&mut hash, name_string.as_bytes());
            update(
                &mut hash,
                content_tree_fingerprint(&entry.path())?.as_bytes(),
            );
        }
    }

    for config_path in configs {
        let config = fs::read_to_string(&config_path).map_err(|error| error.to_string())?;
        for line in config.lines() {
            let Some(equal) = line.find('=') else {
                continue;
            };
            let key = line[..equal]
                .trim()
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .trim();
            if !is_safe_file_reference_key(key) {
                continue;
            }
            let input = line[equal + 1..].trim_start();
            let Some((value, _)) = parse_toml_string(input)? else {
                continue;
            };
            let Some((path, relative)) =
                resolve_config_reference(&value, source, &canonical_source, source_wire_home)?
            else {
                continue;
            };
            update(&mut hash, relative.to_string_lossy().as_bytes());
            update(
                &mut hash,
                &fs::read(&path).map_err(|error| {
                    format!(
                        "Failed to read Codex referenced file '{}': {error}",
                        path.display()
                    )
                })?,
            );
        }
    }
    Ok(format!("{hash:016x}"))
}

fn files_equal(source: &Path, target: &Path) -> Result<bool, String> {
    let target_meta = match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Ok(false),
    };
    let source_meta = fs::metadata(source)
        .map_err(|e| format!("Failed to inspect source asset '{}': {e}", source.display()))?;
    if source_meta.len() != target_meta.len() {
        return Ok(false);
    }
    let source_bytes = fs::read(source)
        .map_err(|e| format!("Failed to read source asset '{}': {e}", source.display()))?;
    let target_bytes = match fs::read(target) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(false),
    };
    Ok(source_bytes == target_bytes)
}

fn copy_file_replacing(source: &Path, target: &Path, target_root: &Path) -> Result<(), String> {
    managed_relative_path(target_root, target)?;
    let parent = target.parent().ok_or_else(|| {
        format!(
            "Cannot determine destination directory for '{}'",
            target.display()
        )
    })?;
    let (temporary, handle) = create_managed_temp_file(parent, target_root)?;
    drop(handle);
    if let Err(error) = fs::copy(source, &temporary) {
        let _ = remove_managed_path(target_root, &temporary);
        return Err(format!(
            "Failed to copy inherited config '{}' to temporary '{}': {error}",
            source.display(),
            temporary.display()
        ));
    }
    replace_with_managed_temp(&temporary, target, target_root)
}

fn atomic_write_managed(target_root: &Path, target: &Path, bytes: &[u8]) -> Result<(), String> {
    managed_relative_path(target_root, target)?;
    let parent = target.parent().ok_or_else(|| {
        format!(
            "Cannot determine destination directory for '{}'",
            target.display()
        )
    })?;
    let (temporary, mut file) = create_managed_temp_file(parent, target_root)?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = remove_managed_path(target_root, &temporary);
        return Err(format!(
            "Failed to write temporary Codex asset '{}': {error}",
            temporary.display()
        ));
    }
    drop(file);
    replace_with_managed_temp(&temporary, target, target_root)
}

fn create_managed_temp_file(
    parent: &Path,
    target_root: &Path,
) -> Result<(PathBuf, fs::File), String> {
    if parent != target_root {
        managed_relative_path(target_root, parent)?;
    }
    for attempt in 0..32u32 {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = parent.join(format!(
            ".wisp-sync-tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        managed_relative_path(target_root, &path)?;
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Failed to create temporary Codex asset '{}': {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "Failed to allocate a temporary Codex asset in '{}'",
        parent.display()
    ))
}

fn replace_with_managed_temp(
    temporary: &Path,
    target: &Path,
    target_root: &Path,
) -> Result<(), String> {
    if fs::symlink_metadata(target).is_ok() {
        remove_managed_path(target_root, target)?;
    }
    if let Err(error) = fs::rename(temporary, target) {
        let _ = remove_managed_path(target_root, temporary);
        return Err(format!(
            "Failed to install isolated Codex asset '{}': {error}",
            target.display()
        ));
    }
    Ok(())
}

/// Rewrite absolute `*_file` values that point into the selected Codex home.
///
/// `source_wire_home` and `target_wire_home` let a WSL provider map config
/// values such as `/home/me/.codex/instructions.md` to local UNC files while
/// serializing the isolated destination back as a WSL-visible path. Providers
/// must call this after preparing a WSL runtime; this function never emits a
/// Windows path when `target_wire_home` is supplied.
pub(crate) fn rewrite_config_file_references_for_runtime(
    config_path: &Path,
    source_home: &Path,
    target_home: &Path,
    source_wire_home: Option<&str>,
    target_wire_home: Option<&str>,
) -> Result<(), String> {
    if source_wire_home.is_some() != target_wire_home.is_some() {
        return Err("Both source and target wire Codex homes are required".into());
    }
    let config = match fs::read_to_string(config_path) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_reference_mirror(target_home)?;
            set_reference_manifest(target_home, false)?;
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "Failed to read isolated Codex config '{}': {error}",
                config_path.display()
            ));
        }
    };
    let canonical_source = fs::canonicalize(source_home).map_err(|e| {
        format!(
            "Failed to resolve selected Codex home '{}': {e}",
            source_home.display()
        )
    })?;
    let mut references = BTreeMap::<PathBuf, PathBuf>::new();
    let mut rewritten = String::with_capacity(config.len());
    for chunk in config.split_inclusive('\n') {
        let (line, newline) = chunk
            .strip_suffix("\r\n")
            .map(|line| (line, "\r\n"))
            .or_else(|| chunk.strip_suffix('\n').map(|line| (line, "\n")))
            .unwrap_or((chunk, ""));
        let replacement = rewrite_config_assignment(
            line,
            source_home,
            &canonical_source,
            target_home,
            source_wire_home,
            target_wire_home,
            &mut references,
        )?;
        rewritten.push_str(replacement.as_deref().unwrap_or(line));
        rewritten.push_str(newline);
    }

    install_reference_mirror(target_home, &references)?;
    if rewritten != config {
        atomic_write_managed(target_home, config_path, rewritten.as_bytes())?;
    }
    set_reference_manifest(target_home, !references.is_empty())
}

#[allow(clippy::too_many_arguments)]
fn rewrite_config_assignment(
    line: &str,
    source_home: &Path,
    canonical_source: &Path,
    target_home: &Path,
    source_wire_home: Option<&str>,
    target_wire_home: Option<&str>,
    references: &mut BTreeMap<PathBuf, PathBuf>,
) -> Result<Option<String>, String> {
    let Some(equal) = line.find('=') else {
        return Ok(None);
    };
    let key = line[..equal]
        .trim()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .trim();
    if !is_safe_file_reference_key(key) {
        return Ok(None);
    }
    let value_offset = equal + 1 + line[equal + 1..].len() - line[equal + 1..].trim_start().len();
    let Some((value, consumed)) = parse_toml_string(&line[value_offset..])? else {
        return Ok(None);
    };
    let suffix = &line[value_offset + consumed..];
    let suffix_trimmed = suffix.trim_start();
    if !suffix_trimmed.is_empty() && !suffix_trimmed.starts_with('#') {
        return Ok(None);
    }

    let Some((local_source, relative)) =
        resolve_config_reference(&value, source_home, canonical_source, source_wire_home)?
    else {
        return Ok(None);
    };
    let destination_relative = PathBuf::from(MANAGED_REFERENCES_DIR).join(&relative);
    let destination_local = target_home.join(&destination_relative);
    managed_relative_path(target_home, &destination_local)?;
    references.insert(destination_relative.clone(), local_source);
    let serialized_target = match target_wire_home {
        Some(wire_home) => wire_join(wire_home, &destination_relative)?,
        None => destination_local.display().to_string(),
    };
    let mut output = String::with_capacity(line.len() + serialized_target.len());
    output.push_str(&line[..value_offset]);
    output.push_str(&toml_string(&serialized_target));
    output.push_str(suffix);
    Ok(Some(output))
}

fn is_safe_file_reference_key(key: &str) -> bool {
    !key.is_empty()
        && key.ends_with("_file")
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn parse_toml_string(input: &str) -> Result<Option<(String, usize)>, String> {
    let Some(first) = input.as_bytes().first().copied() else {
        return Ok(None);
    };
    if first == b'\'' {
        let Some(end) = input[1..].find('\'') else {
            return Err("Unterminated literal TOML file path".into());
        };
        return Ok(Some((input[1..end + 1].to_string(), end + 2)));
    }
    if first != b'"' {
        return Ok(None);
    }
    let mut output = String::new();
    let mut chars = input[1..].char_indices();
    while let Some((offset, ch)) = chars.next() {
        if ch == '"' {
            return Ok(Some((output, offset + 2)));
        }
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let Some((_, escaped)) = chars.next() else {
            return Err("Unterminated TOML escape in file path".into());
        };
        match escaped {
            'b' => output.push('\u{0008}'),
            't' => output.push('\t'),
            'n' => output.push('\n'),
            'f' => output.push('\u{000c}'),
            'r' => output.push('\r'),
            '"' => output.push('"'),
            '\\' => output.push('\\'),
            'u' | 'U' => {
                let digits = if escaped == 'u' { 4 } else { 8 };
                let mut hex = String::with_capacity(digits);
                for _ in 0..digits {
                    let Some((_, digit)) = chars.next() else {
                        return Err("Incomplete TOML Unicode escape in file path".into());
                    };
                    if !digit.is_ascii_hexdigit() {
                        return Err("Invalid TOML Unicode escape in file path".into());
                    }
                    hex.push(digit);
                }
                let scalar = u32::from_str_radix(&hex, 16).map_err(|e| e.to_string())?;
                let decoded = char::from_u32(scalar)
                    .ok_or_else(|| "Invalid TOML Unicode scalar in file path".to_string())?;
                output.push(decoded);
            }
            _ => {
                return Err(format!(
                    "Unsupported TOML escape '\\{escaped}' in file path"
                ))
            }
        }
    }
    Err("Unterminated basic TOML file path".into())
}

fn resolve_config_reference(
    value: &str,
    source_home: &Path,
    canonical_source: &Path,
    source_wire_home: Option<&str>,
) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let (candidate, expected_inside) = if let Some(wire_home) = source_wire_home {
        if let Some(relative) = wire_relative(value, wire_home)? {
            (source_home.join(relative), true)
        } else {
            let native = PathBuf::from(value);
            if native.is_absolute() {
                (native, true)
            } else {
                return Ok(None);
            }
        }
    } else {
        let native = PathBuf::from(value);
        if !native.is_absolute() {
            return Ok(None);
        }
        (native, false)
    };
    let canonical_candidate = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) => {
            return Err(format!(
                "Configured Codex file '{}' cannot be resolved: {error}",
                candidate.display()
            ));
        }
    };
    let relative = match canonical_candidate.strip_prefix(canonical_source) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) if expected_inside => {
            return Err(format!(
                "Refusing Codex file reference '{}' outside selected home '{}'",
                candidate.display(),
                source_home.display()
            ));
        }
        Err(_) => {
            // Native Codex configs commonly keep an instruction file outside
            // CODEX_HOME. Copy it into a stable managed namespace and include
            // its content in the watcher fingerprint rather than silently
            // letting the actor read an untracked mutable path.
            let mut hash = 0xcbf29ce484222325u64;
            for byte in canonical_candidate.to_string_lossy().as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            let file_name = canonical_candidate
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .unwrap_or("config-file");
            PathBuf::from("external").join(format!("{hash:016x}-{file_name}"))
        }
    };
    validate_reference_relative(&relative)?;
    let meta = fs::metadata(&canonical_candidate).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err(format!(
            "Configured Codex file '{}' is not a regular file",
            canonical_candidate.display()
        ));
    }
    Ok(Some((canonical_candidate, relative)))
}

fn validate_reference_relative(relative: &Path) -> Result<(), String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "Refusing unsafe Codex config file path '{}'",
            relative.display()
        ));
    }
    let first = relative
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .unwrap_or_default();
    let file_name = relative
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    if is_runtime_state_root(&first) || is_runtime_state_root(&file_name) {
        return Err(format!(
            "Refusing Codex runtime-state file reference '{}'",
            relative.display()
        ));
    }
    Ok(())
}

fn wire_relative(value: &str, wire_home: &str) -> Result<Option<PathBuf>, String> {
    let home = wire_home.trim_end_matches('/');
    if value == home {
        return Err("A Codex *_file reference cannot point at the home directory".into());
    }
    let Some(remainder) = value.strip_prefix(home) else {
        return Ok(None);
    };
    let Some(relative) = remainder.strip_prefix('/') else {
        return Ok(None);
    };
    let mut path = PathBuf::new();
    for component in relative.split('/') {
        if component.is_empty() || matches!(component, "." | "..") {
            return Err(format!("Refusing unsafe WSL Codex file path '{value}'"));
        }
        path.push(component);
    }
    validate_reference_relative(&path)?;
    Ok(Some(path))
}

fn wire_join(wire_home: &str, relative: &Path) -> Result<String, String> {
    validate_reference_relative(relative)?;
    let suffix = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_string_lossy().replace('\\', "/")),
            _ => Err("Unsafe target wire path".to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join("/");
    Ok(format!("{}/{}", wire_home.trim_end_matches('/'), suffix))
}

fn install_reference_mirror(
    target_home: &Path,
    references: &BTreeMap<PathBuf, PathBuf>,
) -> Result<(), String> {
    let destination = target_home.join(MANAGED_REFERENCES_DIR);
    if references.is_empty() {
        return remove_reference_mirror(target_home);
    }
    let staging = target_home.join(format!(
        ".wisp-config-files-stage-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    managed_relative_path(target_home, &staging)?;
    fs::create_dir(&staging).map_err(|e| {
        format!(
            "Failed to create Codex reference staging directory '{}': {e}",
            staging.display()
        )
    })?;
    let result = (|| {
        for (destination_relative, source) in references {
            let relative = destination_relative
                .strip_prefix(MANAGED_REFERENCES_DIR)
                .map_err(|_| "Invalid managed reference destination".to_string())?;
            validate_reference_relative(relative)?;
            let staged_file = staging.join(relative);
            if let Some(parent) = staged_file.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "Failed to create Codex reference directory '{}': {e}",
                        parent.display()
                    )
                })?;
            }
            fs::copy(source, &staged_file).map_err(|e| {
                format!(
                    "Failed to copy Codex config file '{}' to '{}': {e}",
                    source.display(),
                    staged_file.display()
                )
            })?;
        }
        if fs::symlink_metadata(&destination).is_ok() {
            remove_managed_path(target_home, &destination)?;
        }
        fs::rename(&staging, &destination).map_err(|e| {
            format!(
                "Failed to install Codex reference directory '{}': {e}",
                destination.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() && fs::symlink_metadata(&staging).is_ok() {
        let _ = remove_managed_path(target_home, &staging);
    }
    result
}

fn remove_reference_mirror(target_home: &Path) -> Result<(), String> {
    let path = target_home.join(MANAGED_REFERENCES_DIR);
    if fs::symlink_metadata(&path).is_ok() {
        remove_managed_path(target_home, &path)?;
    }
    Ok(())
}

fn set_reference_manifest(target: &Path, present: bool) -> Result<(), String> {
    let mut manifest = read_sync_manifest(target)?;
    if present {
        let fingerprint = content_tree_fingerprint(&target.join(MANAGED_REFERENCES_DIR))?;
        manifest
            .entries
            .insert(MANAGED_REFERENCES_DIR.into(), fingerprint);
    } else {
        manifest.entries.remove(MANAGED_REFERENCES_DIR);
    }
    write_sync_manifest(target, &manifest)
}

fn content_tree_fingerprint(root: &Path) -> Result<String, String> {
    fn update(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            *hash ^= u64::from(*byte);
            *hash = hash.wrapping_mul(0x100000001b3);
        }
    }
    fn visit(path: &Path, hash: &mut u64) -> Result<(), String> {
        let mut entries = fs::read_dir(path)
            .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            update(hash, entry.file_name().to_string_lossy().as_bytes());
            let file_type = entry.file_type().map_err(|e| e.to_string())?;
            if file_type.is_symlink() {
                // The synchronizer skips links/junctions rather than following
                // them. Fingerprinting must use the same semantics while still
                // recording the skipped name so all other assets remain
                // observable by the watcher.
                update(hash, b"skipped-link");
                continue;
            }
            if file_type.is_dir() {
                update(hash, b"dir");
                visit(&entry.path(), hash)?;
            } else if file_type.is_file() {
                update(hash, b"file");
                let bytes = fs::read(entry.path()).map_err(|e| e.to_string())?;
                update(hash, &bytes);
            }
        }
        Ok(())
    }
    let mut hash = 0xcbf29ce484222325u64;
    visit(root, &mut hash)?;
    Ok(format!("{hash:016x}"))
}

fn metadata_fingerprint(meta: &fs::Metadata) -> String {
    let modified = meta
        .modified()
        .ok()
        .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|v| v.as_nanos())
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
            let name = entry.file_name();
            for byte in name.to_string_lossy().as_bytes() {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(0x100000001b3);
            }
            let meta = fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
            for byte in metadata_fingerprint(&meta).as_bytes() {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(0x100000001b3);
            }
            if meta.is_dir() && !meta.file_type().is_symlink() {
                visit(&entry.path(), hash)?;
            }
        }
        Ok(())
    }
    let mut hash = 0xcbf29ce484222325u64;
    visit(root, &mut hash)?;
    Ok(format!("{hash:016x}"))
}

fn sync_static_dir(source: &Path, target: &Path, target_root: &Path) -> Result<(), String> {
    // Keep the isolated home physically independent.  A symlink/junction would
    // let a Codex/plugin update inside Wisp mutate the user's global home.
    mirror_dir_recursive(source, target, target_root)
}

fn mirror_dir_recursive(source: &Path, target: &Path, target_root: &Path) -> Result<(), String> {
    prepare_managed_dir_destination(target_root, target)?;
    fs::create_dir_all(target).map_err(|e| {
        format!(
            "Failed to create inherited config dir '{}': {e}",
            target.display()
        )
    })?;
    let mut source_entries = BTreeSet::<OsString>::new();
    for entry in fs::read_dir(source).map_err(|e| format!("{e}"))? {
        let entry = entry.map_err(|e| format!("{e}"))?;
        let src = entry.path();
        let dst = target.join(entry.file_name());
        let file_type = entry.file_type().map_err(|e| format!("{e}"))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            source_entries.insert(entry.file_name());
            mirror_dir_recursive(&src, &dst, target_root)?;
        } else if file_type.is_file() {
            source_entries.insert(entry.file_name());
            if !files_equal(&src, &dst)? {
                copy_file_replacing(&src, &dst, target_root)?;
            }
        }
    }

    for entry in fs::read_dir(target).map_err(|e| format!("{e}"))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !source_entries.contains(&entry.file_name()) {
            remove_managed_path(target_root, &entry.path())?;
        }
    }
    Ok(())
}

fn prepare_managed_dir_destination(target_root: &Path, path: &Path) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            remove_managed_path(target_root, path)?;
        }
    }
    Ok(())
}

fn prepare_managed_file_destination(target_root: &Path, path: &Path) -> Result<(), String> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            remove_managed_path(target_root, path)?;
        }
    }
    Ok(())
}

fn remove_managed_path(target_root: &Path, path: &Path) -> Result<(), String> {
    let relative = managed_relative_path(target_root, path)?;
    if relative.components().count() == 1 {
        let name = relative.to_string_lossy();
        if is_runtime_state_root(&name) {
            return Err(format!(
                "Refusing to delete Codex runtime state '{}'",
                path.display()
            ));
        }
    }

    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to inspect managed Codex asset '{}': {error}",
                path.display()
            ));
        }
    };
    if meta.file_type().is_symlink() {
        // Never recurse through a link/junction. Depending on the platform, a
        // directory link is removed with remove_dir and a file link with
        // remove_file.
        if fs::remove_file(path).is_err() {
            fs::remove_dir(path).map_err(|e| {
                format!(
                    "Failed to unlink managed Codex asset '{}': {e}",
                    path.display()
                )
            })?;
        }
    } else if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| {
            format!(
                "Failed to inspect managed Codex directory '{}': {e}",
                path.display()
            )
        })? {
            let entry = entry.map_err(|e| e.to_string())?;
            remove_managed_path(target_root, &entry.path())?;
        }
        fs::remove_dir(path).map_err(|e| {
            format!(
                "Failed to remove stale Codex directory '{}': {e}",
                path.display()
            )
        })?;
    } else {
        fs::remove_file(path).map_err(|e| {
            format!(
                "Failed to remove stale Codex asset '{}': {e}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn managed_relative_path<'a>(target_root: &'a Path, path: &'a Path) -> Result<&'a Path, String> {
    let relative = path.strip_prefix(target_root).map_err(|_| {
        format!(
            "Refusing to modify '{}' outside isolated Codex home '{}'",
            path.display(),
            target_root.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "Refusing unsafe managed Codex path '{}'",
            path.display()
        ));
    }
    Ok(relative)
}

fn is_runtime_state_root(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "sessions"
            | "session"
            | "logs"
            | "log"
            | "cache"
            | ".cache"
            | "history"
            | "memories"
            | "memory"
            | "tmp"
            | "temp"
    ) || lower.contains(".sqlite")
        || lower.ends_with("-wal")
        || lower.ends_with("-shm")
        || lower.ends_with(".wal")
        || lower.ends_with(".log")
        || lower.starts_with("history.")
}

const WISP_BLOCK_BEGIN: &str = "# BEGIN WISP BUILTINS";
const WISP_BLOCK_END: &str = "# END WISP BUILTINS";

fn inject_codex_config_block(config_path: &Path, bridge: &McpBridgeLaunch) -> Result<(), String> {
    let existing = fs::read_to_string(config_path).unwrap_or_default();
    let block = codex_config_block(bridge);
    let updated = replace_marked_block(&existing, &block);
    let parent = config_path.parent().ok_or_else(|| {
        format!(
            "Cannot determine Codex runtime config directory for '{}'",
            config_path.display()
        )
    })?;
    atomic_write_managed(parent, config_path, updated.as_bytes()).map_err(|e| {
        format!(
            "Failed to write Codex runtime config '{}': {e}",
            config_path.display()
        )
    })
}

/// Replace (or append) the marked Wisp block, preserving user config around it.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_single_session_rollout_copies_only_the_matching_thread() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-rollout-import-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = base.join("source");
        let target = base.join("target");
        let thread_id = "0190abcd-1234-5678-9abc-def012345678";
        let matching = source
            .join("sessions")
            .join("2026")
            .join("07")
            .join(format!("rollout-2026-07-10T00-00-00-{thread_id}.jsonl"));
        let unrelated = source
            .join("sessions")
            .join("2026")
            .join("07")
            .join("rollout-2026-07-10T00-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
        std::fs::create_dir_all(matching.parent().unwrap()).unwrap();
        std::fs::write(&matching, "matching rollout").unwrap();
        std::fs::write(&unrelated, "unrelated rollout").unwrap();

        assert!(import_single_session_rollout(&source, &target, thread_id).unwrap());
        let imported = target
            .join("sessions")
            .join(matching.strip_prefix(source.join("sessions")).unwrap());
        let not_imported = target
            .join("sessions")
            .join(unrelated.strip_prefix(source.join("sessions")).unwrap());
        assert_eq!(
            std::fs::read_to_string(imported).unwrap(),
            "matching rollout"
        );
        assert!(!not_imported.exists());

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn import_single_session_rollout_rejects_ambiguous_matches() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-rollout-ambiguous-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = base.join("source");
        let target = base.join("target");
        let thread_id = "0190abcd-1234-5678-9abc-def012345678";
        for year in ["2025", "2026"] {
            let directory = source.join("sessions").join(year);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("rollout-{year}-{thread_id}.jsonl")),
                year,
            )
            .unwrap();
        }

        let error = import_single_session_rollout(&source, &target, thread_id).unwrap_err();
        assert!(error.contains("Multiple rollout files matched"));
        assert!(!target.join("sessions").exists());

        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(windows)]
    #[test]
    fn profile_runtime_rejects_a_linked_windows_ancestor_when_supported() {
        use std::os::windows::fs::symlink_dir;

        let base = std::env::temp_dir().join(format!(
            "wisp-codex-linked-ancestor-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let linked_wisp = project.join(".wisp");
        if let Err(error) = symlink_dir(&outside, &linked_wisp) {
            // Windows without Developer Mode or symlink privilege cannot
            // construct this fixture; do not turn that host policy into a
            // product test failure.
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) {
                let _ = std::fs::remove_dir_all(base);
                return;
            }
            panic!("failed to create linked ancestor fixture: {error}");
        }

        let error = prepare_codex_runtime_for_profile_from_source(
            &project,
            "linked-profile",
            None,
            Some(&base.join("source")),
        )
        .unwrap_err();
        assert!(error.contains("linked/reparse"), "{error}");

        let _ = std::fs::remove_dir(&linked_wisp);
        let _ = std::fs::remove_dir_all(base);
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
            "wisp-codex-rt-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("skills").join("s")).unwrap();
        std::fs::create_dir_all(src.join("plugins").join("cache")).unwrap();
        std::fs::create_dir_all(src.join("cache")).unwrap();
        std::fs::write(src.join("config.toml"), "model = 'x'").unwrap();
        std::fs::write(src.join("auth.json"), "{}").unwrap();
        std::fs::write(src.join("skills").join("s").join("SKILL.md"), "body").unwrap();
        std::fs::write(src.join("plugins").join("cache").join("large.bin"), "no").unwrap();
        std::fs::write(src.join("cache").join("stale"), "no").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        sync_cli_home(&src, &dst).unwrap();
        assert!(dst.join("config.toml").is_file());
        assert!(dst.join("auth.json").is_file());
        assert!(dst.join("skills").join("s").join("SKILL.md").is_file());
        assert!(!dst.join("plugins").exists());
        assert!(!dst.join("cache").exists());
        assert!(dst.join(SYNC_MANIFEST).is_file());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sync_cli_home_removes_retired_plugin_and_rule_mirrors() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-retired-plugin-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("plugins").join("cache")).unwrap();
        std::fs::write(src.join("plugins").join("cache").join("huge.bin"), "source").unwrap();
        std::fs::create_dir_all(src.join("rules")).unwrap();
        std::fs::write(src.join("rules").join("allow.rules"), "source").unwrap();
        std::fs::write(src.join("config.toml"), "model='x'").unwrap();
        std::fs::create_dir_all(dst.join("plugins").join("cache")).unwrap();
        std::fs::write(dst.join("plugins").join("cache").join("old.bin"), "stale").unwrap();
        std::fs::create_dir_all(dst.join("rules")).unwrap();
        std::fs::write(dst.join("rules").join("old.rules"), "stale").unwrap();
        let previous = SyncManifest {
            entries: BTreeMap::from([
                ("plugins".to_string(), "old".to_string()),
                ("rules".to_string(), "old".to_string()),
            ]),
        };
        write_sync_manifest(&dst, &previous).unwrap();

        sync_cli_home(&src, &dst).unwrap();

        assert!(!dst.join("plugins").exists());
        assert!(!dst.join("rules").exists());
        let manifest = read_sync_manifest(&dst).unwrap();
        assert!(!manifest.entries.contains_key("plugins"));
        assert!(!manifest.entries.contains_key("rules"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn source_fingerprint_ignores_plugin_cache_contents() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-plugin-fingerprint-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("plugins").join("cache")).unwrap();
        std::fs::write(base.join("config.toml"), "model='x'").unwrap();
        let plugin = base.join("plugins").join("cache").join("large.bin");
        std::fs::write(&plugin, "one").unwrap();
        let before = source_assets_fingerprint(&base, None).unwrap();
        std::fs::write(&plugin, "two").unwrap();
        let after = source_assets_fingerprint(&base, None).unwrap();
        assert_eq!(before, after);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn source_fingerprint_ignores_auth_rotation_but_tracks_configuration_and_skills() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-config-fingerprint-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(base.join("skills").join("demo")).unwrap();
        std::fs::write(base.join("config.toml"), "model='gpt-a'").unwrap();
        std::fs::write(base.join("auth.json"), r#"{"token":"old"}"#).unwrap();
        std::fs::write(base.join("skills").join("demo").join("SKILL.md"), "one").unwrap();

        let baseline = source_assets_fingerprint(&base, None).unwrap();
        std::fs::write(base.join("auth.json"), r#"{"token":"new"}"#).unwrap();
        assert_eq!(baseline, source_assets_fingerprint(&base, None).unwrap());

        std::fs::write(base.join("config.toml"), "model='gpt-b'").unwrap();
        let config_changed = source_assets_fingerprint(&base, None).unwrap();
        assert_ne!(baseline, config_changed);

        std::fs::write(base.join("skills").join("demo").join("SKILL.md"), "two").unwrap();
        assert_ne!(
            config_changed,
            source_assets_fingerprint(&base, None).unwrap()
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn isolated_config_strips_only_process_launchers_from_provider_auth() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-process-config-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).unwrap();
        let config = base.join("config.toml");
        std::fs::write(
            &config,
            r#"
notify = ["unsafe.exe"]
[mcp_servers.unsafe]
command = "unsafe.exe"
[plugins.unsafe]
enabled = true
[marketplaces.unsafe]
source = "C:/unsafe"
[model_providers.custom.auth]
command = "token-helper.exe"
args = ["token"]
token_env = "SAFE_TOKEN"
"#,
        )
        .unwrap();
        strip_external_process_config(&config, &base).unwrap();
        let parsed = std::fs::read_to_string(&config)
            .unwrap()
            .parse::<toml::Value>()
            .unwrap();
        let table = parsed.as_table().unwrap();
        for removed in ["notify", "mcp_servers", "plugins", "marketplaces"] {
            assert!(!table.contains_key(removed), "{removed} must be removed");
        }
        let auth = table["model_providers"]["custom"]["auth"]
            .as_table()
            .unwrap();
        assert_eq!(auth["token_env"].as_str(), Some("SAFE_TOKEN"));
        assert!(!auth.contains_key("command"));
        assert!(!auth.contains_key("args"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sync_cli_home_never_copies_runtime_state() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("sessions")).unwrap();
        std::fs::create_dir_all(src.join("logs")).unwrap();
        std::fs::write(src.join("state.sqlite"), "db").unwrap();
        std::fs::write(src.join("state.sqlite-wal"), "wal").unwrap();
        std::fs::write(src.join("history.jsonl"), "history").unwrap();
        std::fs::write(src.join("config.toml"), "model='x'").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        sync_cli_home(&src, &dst).unwrap();
        assert!(dst.join("config.toml").exists());
        assert!(!dst.join("state.sqlite").exists());
        assert!(!dst.join("state.sqlite-wal").exists());
        assert!(!dst.join("history.jsonl").exists());
        assert!(!dst.join("sessions").exists());
        assert!(!dst.join("logs").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sync_cli_home_removes_stale_assets_but_preserves_runtime_state() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-mirror-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(src.join("skills").join("demo")).unwrap();
        std::fs::write(src.join("config.toml"), "model='x'").unwrap();
        std::fs::write(src.join("auth.json"), "{}").unwrap();
        std::fs::write(src.join("local.config.toml"), "approval='never'").unwrap();
        std::fs::write(src.join("skills").join("demo").join("keep.md"), "keep").unwrap();
        std::fs::write(src.join("skills").join("demo").join("stale.md"), "stale").unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        sync_cli_home(&src, &dst).unwrap();

        // Runtime state may be created after the initial seed. A later mirror
        // pass must never treat it as an inherited asset or delete it.
        for directory in ["sessions", "logs", "cache", "memories"] {
            std::fs::create_dir_all(dst.join(directory)).unwrap();
            std::fs::write(dst.join(directory).join("keep"), "runtime").unwrap();
        }
        for file in [
            "state.sqlite",
            "state.sqlite-wal",
            "state.sqlite-shm",
            "history.jsonl",
            "runner.log",
        ] {
            std::fs::write(dst.join(file), "runtime").unwrap();
        }

        std::fs::remove_file(src.join("auth.json")).unwrap();
        std::fs::remove_file(src.join("local.config.toml")).unwrap();
        std::fs::remove_file(src.join("skills").join("demo").join("stale.md")).unwrap();
        sync_cli_home(&src, &dst).unwrap();

        assert!(!dst.join("auth.json").exists());
        assert!(!dst.join("local.config.toml").exists());
        assert!(!dst.join("skills").join("demo").join("stale.md").exists());
        assert!(dst.join("skills").join("demo").join("keep.md").exists());
        for directory in ["sessions", "logs", "cache", "memories"] {
            assert!(dst.join(directory).join("keep").exists());
        }
        for file in [
            "state.sqlite",
            "state.sqlite-wal",
            "state.sqlite-shm",
            "history.jsonl",
            "runner.log",
        ] {
            assert!(dst.join(file).exists());
        }

        // Removing a whole allowlisted static directory upstream removes only
        // its isolated mirror, not adjacent runtime state.
        std::fs::remove_dir_all(src.join("skills")).unwrap();
        sync_cli_home(&src, &dst).unwrap();
        assert!(!dst.join("skills").exists());
        assert!(dst.join("sessions").join("keep").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn profile_runtime_can_sync_from_an_explicit_codex_home() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-source-override-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = base.join("project");
        let selected_home = base.join("selected-codex-home");
        std::fs::create_dir_all(selected_home.join("rules")).unwrap();
        std::fs::write(selected_home.join("config.toml"), "model='selected'").unwrap();
        std::fs::write(selected_home.join("auth.json"), "{\"token\":\"selected\"}").unwrap();
        std::fs::write(selected_home.join("rules").join("selected.rules"), "allow").unwrap();

        let runtime = prepare_codex_runtime_for_profile_from_source(
            &project,
            "wsl-profile",
            None,
            Some(&selected_home),
        )
        .unwrap();

        assert_eq!(
            runtime.home_dir,
            project
                .join(".wisp")
                .join("codex-home")
                .join(safe_profile_dir("wsl-profile"))
        );
        assert_eq!(
            std::fs::read_to_string(runtime.home_dir.join("config.toml")).unwrap(),
            "model='selected'"
        );
        assert_eq!(
            std::fs::read_to_string(runtime.home_dir.join("auth.json")).unwrap(),
            "{\"token\":\"selected\"}"
        );
        assert!(!runtime.home_dir.join("rules").exists());
        assert_eq!(
            runtime.env,
            vec![(
                "CODEX_HOME".to_string(),
                runtime.home_dir.display().to_string()
            )]
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unavailable_source_clears_only_manifest_assets() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-unavailable-source-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = base.join("project");
        let source = base.join("selected-home");
        std::fs::create_dir_all(source.join("skills").join("demo")).unwrap();
        std::fs::write(source.join("config.toml"), "model='source'").unwrap();
        std::fs::write(source.join("auth.json"), "{}").unwrap();
        std::fs::write(source.join("skills").join("demo").join("SKILL.md"), "skill").unwrap();
        let runtime = prepare_codex_runtime_for_profile_from_source(
            &project,
            "missing-source",
            None,
            Some(&source),
        )
        .unwrap();
        std::fs::write(runtime.home_dir.join("manual.config.toml"), "manual=true").unwrap();
        std::fs::create_dir_all(runtime.home_dir.join("sessions")).unwrap();
        std::fs::write(runtime.home_dir.join("sessions").join("keep"), "runtime").unwrap();
        std::fs::write(runtime.home_dir.join("state.sqlite-wal"), "runtime").unwrap();

        std::fs::remove_dir_all(&source).unwrap();
        let refreshed = prepare_codex_runtime_for_profile_from_source(
            &project,
            "missing-source",
            None,
            Some(&source),
        )
        .unwrap();

        assert!(!refreshed.home_dir.join("config.toml").exists());
        assert!(!refreshed.home_dir.join("auth.json").exists());
        assert!(!refreshed.home_dir.join("skills").exists());
        assert!(refreshed.home_dir.join("manual.config.toml").exists());
        assert!(refreshed.home_dir.join("sessions").join("keep").exists());
        assert!(refreshed.home_dir.join("state.sqlite-wal").exists());
        assert!(refreshed
            .diagnostics
            .iter()
            .any(|message| message.contains("Inherited assets were cleared")));
        assert!(read_sync_manifest(&refreshed.home_dir)
            .unwrap()
            .entries
            .is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn app_server_sync_restores_source_after_bridge_injection_or_local_edit() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-config-restore-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = base.join("project");
        let source = base.join("selected-home");
        std::fs::create_dir_all(&source).unwrap();
        let source_config = "model = 'source-model'\napproval_policy = 'never'\n";
        std::fs::write(source.join("config.toml"), source_config).unwrap();
        std::fs::write(source.join("auth.json"), "{\"source\":true}").unwrap();
        let bridge = McpBridgeLaunch {
            command: "wisp-tauri".into(),
            args: vec!["--wisp-mcp-bridge".into()],
        };
        let exec_runtime = prepare_codex_runtime_for_profile_from_source(
            &project,
            "shared-profile",
            Some(&bridge),
            Some(&source),
        )
        .unwrap();
        assert!(std::fs::read_to_string(&exec_runtime.config_path)
            .unwrap()
            .contains(WISP_BLOCK_BEGIN));
        std::fs::write(
            &exec_runtime.config_path,
            format!(
                "{}local_only = true\n",
                std::fs::read_to_string(&exec_runtime.config_path).unwrap()
            ),
        )
        .unwrap();
        std::fs::write(exec_runtime.home_dir.join("auth.json"), "{\"local\":true}").unwrap();

        let app_server_runtime = prepare_codex_runtime_for_profile_from_source(
            &project,
            "shared-profile",
            None,
            Some(&source),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&app_server_runtime.config_path).unwrap(),
            source_config
        );
        assert_eq!(
            std::fs::read_to_string(app_server_runtime.home_dir.join("auth.json")).unwrap(),
            "{\"source\":true}"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn absolute_config_file_references_are_copied_and_rewritten() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-config-reference-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = base.join("project");
        let source = base.join("selected-home");
        let instructions = source.join("instructions").join("model.md");
        let extra = source.join("policy").join("extra.txt");
        std::fs::create_dir_all(instructions.parent().unwrap()).unwrap();
        std::fs::create_dir_all(extra.parent().unwrap()).unwrap();
        std::fs::write(&instructions, "isolated instructions").unwrap();
        std::fs::write(&extra, "extra policy").unwrap();
        std::fs::write(
            source.join("config.toml"),
            format!(
                "model_instructions_file = {}\npolicy_file = {}\nrelative_file = \"local.md\"\n",
                toml_string(&instructions.display().to_string()),
                toml_string(&extra.display().to_string())
            ),
        )
        .unwrap();

        let runtime = prepare_codex_runtime_for_profile_from_source(
            &project,
            "reference-profile",
            None,
            Some(&source),
        )
        .unwrap();
        let isolated_instructions = runtime
            .home_dir
            .join(MANAGED_REFERENCES_DIR)
            .join("instructions")
            .join("model.md");
        let isolated_extra = runtime
            .home_dir
            .join(MANAGED_REFERENCES_DIR)
            .join("policy")
            .join("extra.txt");
        let isolated_config = std::fs::read_to_string(&runtime.config_path).unwrap();
        assert!(
            isolated_config.contains(&toml_string(&isolated_instructions.display().to_string()))
        );
        assert!(isolated_config.contains(&toml_string(&isolated_extra.display().to_string())));
        assert!(!isolated_config.contains(&toml_string(&instructions.display().to_string())));
        assert_eq!(
            std::fs::read_to_string(&isolated_instructions).unwrap(),
            "isolated instructions"
        );
        assert_eq!(
            std::fs::read_to_string(&isolated_extra).unwrap(),
            "extra policy"
        );

        std::fs::write(
            source.join("config.toml"),
            format!(
                "model_instructions_file = {}\n",
                toml_string(&instructions.display().to_string())
            ),
        )
        .unwrap();
        prepare_codex_runtime_for_profile_from_source(
            &project,
            "reference-profile",
            None,
            Some(&source),
        )
        .unwrap();
        assert!(isolated_instructions.exists());
        assert!(!isolated_extra.exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn wsl_wire_rewrite_never_serializes_the_windows_target_path() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-wire-reference-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = base.join("wsl-home-unc");
        let target = base.join("profile-home");
        let source_file = source.join("instructions").join("plan.md");
        std::fs::create_dir_all(source_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(&source_file, "plan instructions").unwrap();
        let config_path = target.join("config.toml");
        std::fs::write(
            &config_path,
            "model_instructions_file = '/home/me/.codex/instructions/plan.md'\n",
        )
        .unwrap();
        write_sync_manifest(&target, &SyncManifest::default()).unwrap();

        rewrite_config_file_references_for_runtime(
            &config_path,
            &source,
            &target,
            Some("/home/me/.codex"),
            Some("/mnt/e/repo/.wisp/codex-home/profile"),
        )
        .unwrap();

        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(config.contains(
            "/mnt/e/repo/.wisp/codex-home/profile/.wisp-config-files/instructions/plan.md"
        ));
        assert!(!config.contains("/home/me/.codex/instructions/plan.md"));
        assert!(!config.contains(&target.display().to_string()));
        assert!(target
            .join(MANAGED_REFERENCES_DIR)
            .join("instructions")
            .join("plan.md")
            .exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn managed_cleanup_cannot_escape_the_profile_home() {
        let base = std::env::temp_dir().join(format!(
            "wisp-codex-cleanup-boundary-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = base.join("profile-home");
        let outside = base.join("outside.config.toml");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(&outside, "must remain").unwrap();

        let error = remove_managed_path(&target, &outside).unwrap_err();
        assert!(error.contains("outside isolated Codex home"));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "must remain");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn profile_runtime_directory_is_sanitized() {
        assert_eq!(safe_profile_dir(""), "default");
        assert_eq!(safe_profile_dir("default"), "default");
        assert!(safe_profile_dir(" plan/profile ").starts_with("plan_profile-"));
        assert_ne!(safe_profile_dir("a/b"), safe_profile_dir("a_b"));
        assert_eq!(safe_profile_dir("a/b"), safe_profile_dir("a/b"));
        assert!(safe_profile_dir(&"x".repeat(200)).len() <= 80);
    }
}
