# Artifact Provenance — Design Spec

**Date:** 2026-07-05
**Status:** Approved design, pre-implementation
**Scope:** Click a produced figure/file → open a modal that shows the artifact large
plus its full stored lineage: the code that produced it, its execution log, its
input files, and the environment it ran in. Mirrors Claude Science's artifact
provenance, adapted to wisp's single-agent Rust architecture.

## 1. Goal & non-goals

**Goal.** Every file a tool writes into the workspace gets a recorded provenance:
which tool call produced it (with the exact source), that call's stdout/stderr/exit
status, which existing files it read as inputs, and a snapshot of the Python
environment at production time. The UI surfaces this in an image/artifact modal with
four panels — **Code**, **Execution Log**, **Inputs**, **Environment** — opened by
clicking the artifact.

**Non-goals (deliberately cut, YAGNI):**
- Version chains (Claude Science's `artifact_versions.parent_version_id` history). One
  provenance record per current file path; re-running overwrites it.
- A **Messages** panel (conversation lineage) — the conversation is already visible.
- A **Review / annotations** panel — separate feature, not in this spec.
- Reworking wisp's artifact-identity model. Provenance is keyed by
  `(frame_id, workspace-relative path)` — the identifier the UI already uses.

## 2. Current state (why this is new capture, not a column add)

- The DB `artifacts` table is minimal: `(id, project_id, root_frame_id, filename,
  content_type, storage_path, created_at)` — no code/log/env/lineage.
- `register_artifact` / `list_artifacts` commands exist but the UI **never calls
  them**. Produced figures are detected **client-side** by scanning assistant/tool
  markdown for file paths (`collect_markdown_artifacts` in `ui/src/main.rs`) and
  rendered by reading the path via `read_file`. The DB `artifacts` table is written
  only by `upload_file`.
- There is **no** backend record of "which tool call wrote which file." That is the
  core thing this feature adds.

Enabling facts that make capture cheap:
- Single tool-dispatch chokepoint: `crates/wisp-core/src/agent.rs:76`
  `let result = tools.run(&name, &args, &env).await;`. Wrapping here covers **every**
  producer uniformly — the Python kernel (`savefig`), `shell` (e.g. `Rscript` running
  a ggplot2 volcano), and `write`.
- `ToolEnv` (`crates/wisp-tools/src/env.rs`) already exposes `project_root()`,
  `emit(ToolEvent)`, `is_cancelled()`.
- src-tauri's `ToolEnv` impl (`EmitEnv`) holds the `AppHandle` (→ `AppState` → store)
  and already forwards `AgentEvent`s to the frontend and messages to a persist
  channel. It is the natural place to persist a new provenance event.

## 3. Architecture

Four pieces: **capture** (wisp-core), **environment snapshot** (src-tauri, lazy),
**storage** (wisp-store), **query + UI** (src-tauri command + ui modal).

### 3.1 Capture — snapshot diff at the dispatch boundary

In `agent.rs`, wrap the `tools.run` call for **producing tools only**
(`python`, `shell`; `write`/`edit` already know their target path, no snapshot
needed). Snapshotting is skipped for read-only tools (`read`, `grep`, `search`, …).

```
before = snapshot(project_root)              // path -> mtime, recursive
result = tools.run(name, args, env).await
after  = snapshot(project_root)
files_written = { p in after : p not in before OR after[p].mtime > before[p].mtime }
files_read    = { p in before : path-string of p appears literally in source }
```

- `snapshot` walks `project_root` recursively, **skipping** `.git`, `.venv`,
  `node_modules`, `.wisp`, `uploads`, and any dir over a file-count cap.
  `// ponytail: recursive mtime scan, cap+skip heavy dirs; switch to fs-notify if this
  shows up in profiles.`
- `source` = the tool's code: `args["code"]` for `python`, `args["command"]` for
  `shell`. `language` is **tool-derived** — `"python"` for the kernel, `"bash"` for
  `shell` — we do not try to detect that a `shell` call runs R/Julia inside.
- If `files_written` is empty, emit nothing — read-only or no-op calls never create
  provenance rows, keeping the log lean and figure-relevant.
- Otherwise assemble a record and emit a new event:
  `ToolEvent::Provenance { tool, language, source, stdout, stderr, exit_status,
  wall_s, files_written: Vec<String>, files_read: Vec<String> }`
  (stdout/stderr/exit_status/wall_s come from `result`; paths are workspace-relative).

`snapshot` + the diff live in a small new module `crates/wisp-core/src/provenance.rs`
so `agent.rs` only gains a thin wrapper. `ToolEvent` gains the `Provenance` variant in
`wisp-tools`.

### 3.2 Environment snapshot — lazy, per session

Environment capture stays **out of the hot wisp-core path**. When `EmitEnv` persists
the **first** provenance record of a session, it shells out once:
`uv pip list --format=json` using the session's kernel Python
(`crates/wisp-python/src/env.rs::python()`); if a conda env is active, also
`conda list --json`. The JSON is content-hashed; the hash + package list are stored in
`env_snapshots` and reused for every later record in that session. Failure to capture
is non-fatal — `env_hash` is left NULL and the Environment panel shows "unavailable".

### 3.3 Storage — wisp-store, additive migrations

Follow the existing additive-migration pattern (cf. the `workspace_dir`
`ALTER TABLE … ADD COLUMN` guard in `crates/wisp-store/src/lib.rs`).

```sql
CREATE TABLE execution_log (
  id            TEXT PRIMARY KEY,
  frame_id      TEXT NOT NULL,
  cell_index    INTEGER NOT NULL,       -- count of prior rows in the frame
  tool          TEXT NOT NULL,          -- 'python' | 'shell'
  language      TEXT NOT NULL,          -- tool-derived: 'python' (kernel) | 'bash' (shell)
  source        TEXT NOT NULL,
  stdout        TEXT,
  stderr        TEXT,
  exit_status   TEXT NOT NULL,          -- 'ok' | 'error'
  wall_s        REAL,
  files_written TEXT NOT NULL,          -- JSON array of workspace-relative paths
  files_read    TEXT NOT NULL,          -- JSON array
  env_hash      TEXT,                   -- FK-ish into env_snapshots(hash), nullable
  created_at    INTEGER NOT NULL
);
CREATE INDEX ix_execution_log_frame ON execution_log(frame_id, cell_index);

CREATE TABLE env_snapshots (
  hash          TEXT PRIMARY KEY,
  env_name      TEXT,
  packages_json TEXT NOT NULL,          -- JSON array of {name, version}
  created_at    INTEGER NOT NULL
);

-- artifacts gains a link to the row that produced it (nullable; uploads stay NULL)
ALTER TABLE artifacts ADD COLUMN producing_exec_id TEXT DEFAULT NULL;
```

On persisting a `Provenance` event, `EmitEnv`:
1. Resolves/creates the session `env_hash` (§3.2).
2. Inserts one `execution_log` row (`cell_index` = current count for the frame).
3. For each path in `files_written`, upserts an `artifacts` row (so produced figures
   finally land in the DB) with `producing_exec_id = <this row>`.

### 3.4 Query command + UI

**Command** (`src-tauri`):
```
get_artifact_provenance(session_id: Option<String>, path: String)
  -> Option<ArtifactProvenance>
```
Resolves the frame (given or active), finds the `execution_log` row whose
`files_written` contains `path`, and returns:
```
ArtifactProvenance {
  code: String, language: String,
  stdout: String, stderr: String, exit_status: String, wall_s: Option<f64>,
  inputs: Vec<{ path: String, artifact_id: Option<String> }>,  // files_read, linked when another artifact matches
  env: Option<{ name: Option<String>, packages: Vec<{ name, version }> }>,
}
```
Returns `None` for paths with no record (uploads, pre-feature figures) → the modal
shows an empty state.

**UI** (`ui/src/main.rs`, new `ArtifactModal` component):
- The rendered image becomes clickable (right-pane `.rp-view` preview and inline file
  links) → opens `ArtifactModal` for that path.
- Modal: `.overlay > .modal` with a large image + filename + download + close, then a
  four-tab strip **Code / Execution Log / Inputs / Environment**, populated from
  `get_artifact_provenance`. Reuses existing `RpCodeView` (line-numbered code) for
  Code, `<pre>` for the log, chips for Inputs (clickable when linked to another
  artifact), a package table for Environment.
- Empty state per tab when provenance is `None` or a field is empty.
- New i18n keys under `artifact.*` / `provenance.*` (En + Zh).

## 4. Data flow

```
agent turn
  └─ agent.rs: for a producing tool
       snapshot(before) → tools.run → snapshot(after) → diff
       files_written non-empty ? emit ToolEvent::Provenance{...}
            │
            ▼
  EmitEnv.emit (src-tauri)
       ensure session env_snapshot (lazy `uv pip list`) → env_hash
       INSERT execution_log
       upsert artifacts(producing_exec_id) for each written path
            │
   ... later, user clicks the figure ...
            ▼
  ui ArtifactModal → invoke get_artifact_provenance(session, path)
       → execution_log row + linked inputs + env_snapshot
       → Code / Execution Log / Inputs / Environment panels
```

## 5. Error handling

- Snapshot walk errors (permission, race): treated as empty diff for that side; never
  aborts the tool. Provenance is best-effort telemetry, never blocks the agent.
- Env capture failure: `env_hash` NULL, Environment panel shows "unavailable".
- `get_artifact_provenance` on an unknown path: returns `None`, modal shows empty state
  (image still viewable — the viewer never depends on provenance existing).
- Overwrite semantics: re-running a cell that writes the same path inserts a new
  `execution_log` row; the query returns the most recent by `created_at`.

## 6. Testing

- **wisp-core unit:** `provenance::snapshot` + diff — create temp dir, write a file
  mid-"run", assert it appears in `files_written`; assert a skipped dir (`.git`) never
  appears; assert a literal path in source is detected as `files_read`.
- **wisp-store unit:** insert an `execution_log` row + `env_snapshots` row, upsert an
  artifact with `producing_exec_id`, read back via the provenance query; assert
  `files_written` JSON round-trips.
- **ui e2e (Playwright, mocked bridge):** mock `get_artifact_provenance`; click a
  figure → modal opens with the image; Code/Inputs/Environment tabs render mocked
  data; an artifact with no provenance shows the empty state.

## 7. Ponytail scope ledger

- Snapshot only wraps `python` + `shell` (opaque producers); `write`/`edit` use their
  known target path; read-only tools are never snapshotted.
- Provenance rows only for calls that wrote ≥1 file.
- `files_read` = literal path match against the before-snapshot (no fs-notify / audit
  hooks).
- Env captured once per session, not per cell.
- No version history, no Messages/Review panels.
- Recursive mtime scan is capped and skips heavy dirs; ceiling + upgrade path noted in
  code with a `ponytail:` comment.
