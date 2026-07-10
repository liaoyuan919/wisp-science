//! Data model for the UI: the serde DTOs exchanged with the Tauri backend plus
//! the in-memory view/form types.
//!
//! This module holds *data only* — struct/enum shapes and trivial inherent
//! impls (defaults, conversions, small classifiers). It must not depend on
//! Leptos reactivity, the JS bindings, or view code, so the shapes stay easy to
//! reason about and reuse. Fields are `pub(crate)` so the rest of the crate can
//! read/build them; behaviour that needs i18n, signals, or FFI lives elsewhere.

use crate::i18n::Locale;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
#[serde(tag = "kind")]
pub(crate) enum AgentEvent {
    User { frame_id: String, text: String },
    Text { frame_id: String, delta: String },
    Reasoning { frame_id: String, delta: String },
    ToolCall { frame_id: String, name: String, preview: String },
    ToolResult {
        frame_id: String,
        name: String,
        ok: bool,
        content: String,
        #[serde(default)]
        duration_ms: u64,
    },
    Usage { frame_id: String, round: u64, input: u64, output: u64, ctx_tokens: usize, max_context: usize },
    Compaction { frame_id: String, before: usize, after: usize, strategy: String },
    Diff { frame_id: String, path: String },
    Stdout { frame_id: String, chunk: String },
    Done { frame_id: String },
    Error { frame_id: String, message: String },
    ReviewStarted { frame_id: String },
    Review { frame_id: String, report: ReviewReport },
    CorrectionStarted { frame_id: String, model: String },
    /// Native Codex plan-mode events.  These variants deliberately accept the
    /// app-server field names rather than translating them in JavaScript so the
    /// transcript and composer use the same payload the backend persisted.
    #[serde(rename = "plan_delta", alias = "PlanDelta")]
    PlanDelta {
        frame_id: String,
        #[serde(default, alias = "text")]
        delta: String,
        #[serde(default = "default_true")]
        native: bool,
    },
    #[serde(rename = "final_plan", alias = "FinalPlan")]
    FinalPlan {
        frame_id: String,
        #[serde(default, alias = "text", alias = "proposed_plan")]
        plan: String,
        #[serde(default = "default_true")]
        native: bool,
        #[serde(default)]
        plan_id: String,
        #[serde(default)]
        revision: i64,
    },
    #[serde(rename = "plan_updated", alias = "PlanUpdated")]
    PlanUpdated {
        frame_id: String,
        #[serde(default, alias = "text", alias = "proposed_plan")]
        plan: String,
        #[serde(default = "default_true")]
        native: bool,
    },
    #[serde(rename = "request_user_input", alias = "RequestUserInput")]
    RequestUserInput {
        frame_id: String,
        #[serde(default, alias = "id")]
        question_id: String,
        #[serde(default, alias = "prompt")]
        question: String,
        #[serde(default)]
        options: Vec<PlanQuestionOption>,
        #[serde(default)]
        is_other: bool,
        #[serde(default)]
        is_secret: bool,
    },
    #[serde(rename = "request_user_input_resolved", alias = "RequestUserInputResolved")]
    RequestUserInputResolved {
        frame_id: String,
        question_id: String,
    },
    #[serde(rename = "model_rerouted", alias = "ModelRerouted")]
    ModelRerouted {
        frame_id: String,
        #[serde(default, alias = "requested")]
        requested_model: String,
        #[serde(default, alias = "effective")]
        effective_model: String,
    },
}

fn default_true() -> bool { true }

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// IPC versions are decimal strings. Accept a JSON number as a read-only
/// compatibility path for snapshots produced by pre-v0.8 backends, but always
/// serialize the UI value as a string so JavaScript cannot round a u64.
fn deserialize_version_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(value) => value,
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Null => String::new(),
        other => return Err(serde::de::Error::custom(format!("invalid config version: {other}"))),
    })
}

#[derive(Deserialize, Clone, Hash, PartialEq, Eq)]
pub(crate) struct ReviewFinding {
    #[serde(default)]
    pub(crate) message_index: usize,
    #[serde(default)]
    pub(crate) claim: String,
    #[serde(default)]
    pub(crate) evidence: String,
    #[serde(default)]
    pub(crate) fix: String,
    #[serde(default)]
    pub(crate) verdict: String,
    #[serde(default)]
    pub(crate) severity: String,
    #[serde(default)]
    pub(crate) status: String,
}

#[derive(Deserialize, Clone, Hash, PartialEq, Eq)]
pub(crate) struct ReviewReport {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) findings: Vec<ReviewFinding>,
    #[serde(default)]
    pub(crate) reviewer_model: String,
    #[serde(default)]
    pub(crate) reviewer_effort: String,
}

#[derive(Clone)]
pub(crate) enum ChatItem {
    User(String),
    QueuedUser(String),
    Assistant { text: String, model: Option<String> },
    Reasoning(String),
    Tool {
        name: String,
        ok: Option<bool>,
        input: String,
        output: String,
        /// Wall-clock start (ms) while the tool is running; cleared on result.
        started_at_ms: Option<u64>,
        /// Elapsed ms from tool call card to result.
        duration_ms: Option<u64>,
    },
    /// Inline tool-approval card (replaces the old centered modal).
    ApprovalPending { tool: String, preview: String, message: String },
    Review(ReviewReport),
    Plan(PlanCard),
    PlanQuestion(PlanQuestion),
}

impl ChatItem {
    /// Content hash used as the keyed-list key in the chat thread: a row is
    /// rebuilt only when this changes, so streaming updates to one message
    /// don't re-render the whole conversation.
    pub(crate) fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Self::User(s) => (0u8, s).hash(&mut h),
            Self::QueuedUser(s) => (1u8, s).hash(&mut h),
            Self::Assistant { text, model } => (2u8, text, model).hash(&mut h),
            Self::Reasoning(s) => (3u8, s).hash(&mut h),
            Self::Tool { name, ok, input, output, duration_ms, .. } => {
                (4u8, name, ok, input, output, duration_ms).hash(&mut h)
            }
            Self::ApprovalPending { tool, preview, message } => (6u8, tool, preview, message).hash(&mut h),
            Self::Review(report) => (5u8, report).hash(&mut h),
            Self::Plan(plan) => (7u8, plan).hash(&mut h),
            Self::PlanQuestion(question) => (8u8, question).hash(&mut h),
        }
        h.finish()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Hash, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum PlanQuestionOption {
    Label(String),
    Detail {
        #[serde(default, alias = "value")]
        label: String,
        #[serde(default)]
        description: String,
    },
}

impl PlanQuestionOption {
    pub(crate) fn label(&self) -> &str {
        match self { Self::Label(label) => label, Self::Detail { label, .. } => label }
    }
    pub(crate) fn description(&self) -> &str {
        match self { Self::Label(_) => "", Self::Detail { description, .. } => description }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PlanQuestion {
    pub(crate) question_id: String,
    pub(crate) question: String,
    pub(crate) options: Vec<PlanQuestionOption>,
    pub(crate) is_other: bool,
    pub(crate) is_secret: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct PlanCard {
    pub(crate) text: String,
    pub(crate) complete: bool,
    pub(crate) native: bool,
    pub(crate) actionable: bool,
    pub(crate) plan_id: String,
    pub(crate) revision: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ProposedPlanRecord {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) revision: i64,
    #[serde(default, alias = "plan", alias = "proposed_plan", alias = "content")]
    pub(crate) text: String,
    #[serde(default = "default_true")]
    pub(crate) native: bool,
    #[serde(default)]
    pub(crate) status: String,
}

pub(crate) fn active_model_label(models: &[ModelProfile]) -> Option<String> {
    models.iter().find(|m| m.active).or_else(|| models.first()).map(|m| m.label.clone()).filter(|s| !s.is_empty())
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub(crate) struct ArtifactInfo {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) path: String,
    pub(crate) ts: i64,
    #[serde(default)] pub(crate) project_id: Option<String>,
    #[serde(default)] pub(crate) project_name: Option<String>,
    #[serde(default)] pub(crate) session_id: Option<String>,
    #[serde(default)] pub(crate) session_title: Option<String>,
    #[serde(default)] pub(crate) size_bytes: Option<i64>,
    #[serde(default)] pub(crate) origin: Option<String>,
}

#[derive(Deserialize, Clone, PartialEq)]
pub(crate) struct SessionSearchInfo {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) project_name: String,
    pub(crate) title: String,
    #[serde(default)] pub(crate) ts: i64,
    #[serde(default)] pub(crate) activity_at: i64,
    #[serde(default)] pub(crate) status: String,
}

#[derive(Serialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ComposerReferenceArg {
    Artifact { id: String },
    Session { id: String },
    Skill { name: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct SshHost {
    pub(crate) alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) notes: Option<String>,
}

#[derive(Clone)]
pub(crate) enum ComposerAttachment {
    Uploading { key: String, name: String },
    Ready { key: String, name: String, path: String },
    Error { key: String, name: String, error: String },
}

#[derive(Deserialize)]
pub(crate) struct UploadFileResult {
    pub(crate) ok: bool,
    pub(crate) info: Option<ArtifactInfo>,
    pub(crate) filename: Option<String>,
    pub(crate) error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Settings {
    pub(crate) provider: String,
    pub(crate) api_url: String,
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) label: String,
    pub(crate) has_api_key: bool,
    #[serde(default)]
    pub(crate) locale: String,
    #[serde(default)]
    pub(crate) workspace_dir: String,
    #[serde(default)]
    pub(crate) max_tokens: u64,
    #[serde(default)]
    pub(crate) reasoning_effort: String,
    #[serde(default)]
    pub(crate) supports_vision: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            provider: "openai".into(),
            api_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-pro".into(),
            label: "deepseek-v4-pro".into(),
            has_api_key: false,
            locale: Locale::En.code().into(),
            workspace_dir: String::new(),
            max_tokens: 8192,
            reasoning_effort: String::new(),
            supports_vision: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct DemoInfo {
    pub(crate) id: String,
    pub(crate) title: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct Demo {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) request: String,
    pub(crate) response: String,
    pub(crate) thinking: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SendMessageArgs {
    // Tauri v2 maps JS camelCase keys to snake_case params; the JS side must
    // send `sessionId` or the backend sees `None` and forks a new conversation.
    pub(crate) session_id: Option<String>,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) attachments: Vec<String>,
    #[serde(default)]
    pub(crate) references: Vec<ComposerReferenceArg>,
    #[serde(default)]
    pub(crate) resume: bool,
    /// Codex app-server collaboration mode (`default` or `plan`).  Kept
    /// optional so API/Claude profiles and older backends receive exactly the
    /// legacy payload they understand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) collaboration_mode: Option<String>,
    /// Optimistic-concurrency token returned by the runtime snapshot/preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codex_config_generation: Option<String>,
    /// The per-session layer that was visible in the composer for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) codex_overrides: Option<CodexModeOverrides>,
}

/// A model entry returned by Codex `model/list`.  Codex versions have used a
/// few names for the effort list; aliases let the UI remain forward/backward
/// compatible without inventing options on the frontend.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexModelInfo {
    #[serde(default, alias = "model", alias = "slug")]
    pub(crate) id: String,
    #[serde(default, alias = "name", alias = "displayName")]
    pub(crate) display_name: String,
    #[serde(
        default,
        alias = "supported_efforts",
        alias = "reasoning_efforts",
        alias = "supportedReasoningEfforts"
    )]
    pub(crate) supported_reasoning_efforts: Vec<String>,
    #[serde(default, alias = "default_effort", alias = "defaultReasoningEffort")]
    pub(crate) default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default, alias = "supportsImages")]
    pub(crate) supports_images: bool,
    #[serde(default, alias = "supportsPersonality")]
    pub(crate) supports_personality: bool,
    #[serde(default, alias = "serviceTiers")]
    pub(crate) service_tiers: Vec<String>,
    #[serde(default, alias = "defaultServiceTier", deserialize_with = "deserialize_nullable_string")]
    pub(crate) default_service_tier: String,
}

impl CodexModelInfo {
    pub(crate) fn label(&self) -> String {
        if self.display_name.trim().is_empty() { self.id.clone() } else { self.display_name.clone() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexRuntimeInfo {
    #[serde(default, alias = "path", alias = "executable", alias = "executablePath")]
    pub(crate) executable_path: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")]
    pub(crate) version: String,
    #[serde(default, alias = "home", alias = "codexHome")]
    pub(crate) codex_home: String,
    #[serde(default, alias = "runtime_source")]
    pub(crate) source: String,
    #[serde(default, alias = "execution_context")]
    pub(crate) context: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexCapabilities {
    #[serde(default, alias = "appServer")]
    pub(crate) app_server: bool,
    #[serde(default, alias = "nativePlan", alias = "plan")]
    pub(crate) native_plan: bool,
    #[serde(default, alias = "images", alias = "imageInput")]
    pub(crate) image_input: bool,
    #[serde(default)]
    pub(crate) personality: bool,
    #[serde(default, alias = "serviceTier")]
    pub(crate) service_tier: bool,
    #[serde(default, alias = "reasoningSummary")]
    pub(crate) reasoning_summary: bool,
    #[serde(default)]
    pub(crate) verbosity: bool,
    #[serde(default, alias = "webSearch")]
    pub(crate) web_search: bool,
    #[serde(default)]
    pub(crate) sandbox: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub(crate) enum CollaborationModeInfo {
    Name(String),
    Detail {
        #[serde(default, alias = "mode")]
        id: String,
        #[serde(default, alias = "name", alias = "displayName")]
        label: String,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexTurnValues {
    #[serde(default)] pub(crate) model: String,
    #[serde(default, alias = "reasoning_effort", alias = "reasoningEffort")]
    pub(crate) effort: String,
    #[serde(default)] pub(crate) service_tier: String,
    #[serde(default)] pub(crate) personality: String,
    #[serde(default)] pub(crate) summary: String,
    #[serde(default)] pub(crate) verbosity: String,
    #[serde(default)] pub(crate) web_search: String,
    #[serde(default)] pub(crate) sandbox: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ResolvedTurnConfig {
    #[serde(default, alias = "generation", alias = "configGeneration", deserialize_with = "deserialize_version_string")]
    pub(crate) config_version: String,
    #[serde(default)] pub(crate) runtime_path: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) runtime_version: String,
    #[serde(default)] pub(crate) codex_home: String,
    #[serde(default, alias = "collaboration_mode", alias = "collaborationMode")]
    pub(crate) mode: String,
    #[serde(default)] pub(crate) requested: CodexTurnValues,
    #[serde(default)] pub(crate) effective: CodexTurnValues,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) requested_model: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) effective_model: String,
    #[serde(default, alias = "requested_reasoning_effort", deserialize_with = "deserialize_nullable_string")]
    pub(crate) requested_effort: String,
    #[serde(default, alias = "effective_reasoning_effort", deserialize_with = "deserialize_nullable_string")]
    pub(crate) effective_effort: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) service_tier: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) personality: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) summary: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) verbosity: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) web_search: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) sandbox: String,
    #[serde(default)] pub(crate) sandbox_policy: serde_json::Value,
    #[serde(default)] pub(crate) sources: HashMap<String, String>,
    #[serde(default)] pub(crate) effective_sources: HashMap<String, String>,
    #[serde(default)] pub(crate) warnings: Vec<String>,
    #[serde(default)] pub(crate) validation_errors: Vec<String>,
}

impl ResolvedTurnConfig {
    pub(crate) fn requested_model(&self) -> &str {
        if self.requested_model.is_empty() { &self.requested.model } else { &self.requested_model }
    }
    pub(crate) fn effective_model(&self) -> &str {
        if self.effective_model.is_empty() { &self.effective.model } else { &self.effective_model }
    }
    pub(crate) fn requested_effort(&self) -> &str {
        if self.requested_effort.is_empty() { &self.requested.effort } else { &self.requested_effort }
    }
    pub(crate) fn effective_effort(&self) -> &str {
        if self.effective_effort.is_empty() { &self.effective.effort } else { &self.effective_effort }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct RuntimeSnapshot {
    #[serde(default, alias = "generation", alias = "configGeneration", deserialize_with = "deserialize_version_string")]
    pub(crate) config_version: String,
    #[serde(default, alias = "profileId")]
    pub(crate) profile_id: String,
    #[serde(default, alias = "projectId")]
    pub(crate) project_id: String,
    #[serde(default)] pub(crate) runtime: CodexRuntimeInfo,
    // Compatibility with the original flat UI contract.
    #[serde(default)] pub(crate) path: String,
    #[serde(default, deserialize_with = "deserialize_nullable_string")] pub(crate) version: String,
    #[serde(default, alias = "codexHome")] pub(crate) codex_home: String,
    #[serde(default)] pub(crate) models: Vec<CodexModelInfo>,
    #[serde(default, alias = "effective_config", alias = "effectiveConfig")]
    pub(crate) config: Option<ResolvedTurnConfig>,
    #[serde(default, alias = "overrides", alias = "profileOverrides")]
    pub(crate) profile_overrides: Option<CodexModeOverrides>,
    #[serde(default, alias = "collaborationModes")]
    pub(crate) collaboration_modes: Vec<CollaborationModeInfo>,
    #[serde(default, alias = "capabilities", alias = "providerCapabilities")]
    pub(crate) provider_capabilities: CodexCapabilities,
    #[serde(default)] pub(crate) warnings: Vec<String>,
    #[serde(default, alias = "updated_at", alias = "refreshedAt")]
    pub(crate) refreshed_at: String,
}

impl RuntimeSnapshot {
    pub(crate) fn executable_path(&self) -> &str {
        if self.runtime.executable_path.is_empty() { &self.path } else { &self.runtime.executable_path }
    }
    pub(crate) fn version(&self) -> &str {
        if self.runtime.version.is_empty() { &self.version } else { &self.runtime.version }
    }
    pub(crate) fn codex_home(&self) -> &str {
        if self.runtime.codex_home.is_empty() { &self.codex_home } else { &self.runtime.codex_home }
    }
}

/// One mode's profile/session layer.  Empty values mean "inherit", never a
/// hard-coded fallback.  Custom IDs/efforts are preserved verbatim so Codex,
/// not the UI, is the authority that accepts or rejects them.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexModeOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, alias = "reasoning_effort", skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CodexModeOverrides {
    #[serde(default)] pub(crate) normal: CodexModeOverride,
    #[serde(default)] pub(crate) plan: CodexModeOverride,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) service_tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) personality: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verbosity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) web_search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox: Option<String>,
}

impl CodexModeOverrides {
    pub(crate) fn for_mode(&self, mode: &str) -> &CodexModeOverride {
        if mode.eq_ignore_ascii_case("plan") { &self.plan } else { &self.normal }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SessionCodexState {
    #[serde(default)]
    pub(crate) overrides: CodexModeOverrides,
    #[serde(default)]
    pub(crate) mode: String,
    #[serde(default, deserialize_with = "deserialize_version_string")]
    pub(crate) revision: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub(crate) struct CodexTurnConfigAudit {
    #[serde(default)]
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) mode: String,
    #[serde(default, deserialize_with = "deserialize_version_string")]
    pub(crate) config_version: String,
    #[serde(default)]
    pub(crate) requested: serde_json::Value,
    #[serde(default)]
    pub(crate) sent: serde_json::Value,
    #[serde(default)]
    pub(crate) actual: serde_json::Value,
    #[serde(default)]
    pub(crate) created_at: i64,
    #[serde(default)]
    pub(crate) updated_at: i64,
}

#[derive(Deserialize, Clone)]
pub(crate) struct SessionInfo {
    pub(crate) id: String,
    pub(crate) title: String,
    #[allow(dead_code)]
    pub(crate) ts: i64,
    #[serde(default)]
    pub(crate) folder_id: Option<String>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct FolderInfo {
    pub(crate) id: String,
    pub(crate) name: String,
}

/// A transcript row returned by `load_session`.
#[derive(Deserialize, Clone)]
pub(crate) struct LoadedItem {
    pub(crate) role: String,
    pub(crate) text: String,
    pub(crate) tool_name: Option<String>,
    pub(crate) ok: Option<bool>,
    #[serde(default)]
    pub(crate) input: String,
    #[serde(default)]
    pub(crate) model_name: Option<String>,
}

impl LoadedItem {
    pub(crate) fn into_chat(self) -> ChatItem {
        match self.role.as_str() {
            "user" => ChatItem::User(self.text),
            "reasoning" => ChatItem::Reasoning(self.text),
            "tool" => ChatItem::Tool {
                name: self.tool_name.unwrap_or_else(|| "tool".into()),
                ok: self.ok,
                input: self.input,
                output: self.text,
                started_at_ms: None,
                duration_ms: None,
            },
            _ => ChatItem::Assistant { text: self.text, model: self.model_name },
        }
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct TableData {
    pub(crate) headers: Vec<String>,
    pub(crate) rows: Vec<Vec<String>>,
}

#[derive(Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum PreviewData {
    Table(TableData),
    Text(String),
    Markdown(String),
    Latex { tex: String, display: bool },
    File { path: String, kind: String },
    Smiles(String),
    Fasta(String),
}

#[derive(Clone, PartialEq)]
pub(crate) struct Artifact {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) kind: &'static str,
    pub(crate) data: PreviewData,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub(crate) struct FileContent {
    pub(crate) path: String,
    pub(crate) mime: String,
    pub(crate) text: Option<String>,
    pub(crate) base64: Option<String>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct DirEntry {
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
}

#[derive(Deserialize, Clone)]
pub(crate) struct FileSearchHit {
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) is_dir: bool,
    pub(crate) size: u64,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct ProjectInfo {
    #[serde(default)] pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) root: String,
    pub(crate) skill_count: usize,
    pub(crate) mcp_server_count: usize,
    pub(crate) memory_file_count: usize,
    pub(crate) has_api_key: bool,
}

#[derive(Clone, Deserialize, PartialEq)]
pub(crate) struct ProjectSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)] pub(crate) description: String,
    #[allow(dead_code)] #[serde(default)] pub(crate) workspace_dir: String,
    #[serde(default)] pub(crate) session_count: i64,
    #[serde(default)] pub(crate) updated_at: i64,
    #[serde(default)] pub(crate) running_count: i64,
    #[serde(default)] pub(crate) needs_you_count: i64,
}

/// Editable project settings (Project Settings modal). `agent_context` is the
/// project's `.wisp/WISP.md`, injected into every seeded system prompt.
#[derive(Clone, Deserialize, Default)]
pub(crate) struct ProjectSettings {
    #[allow(dead_code)] #[serde(default)] pub(crate) id: String,
    #[serde(default)] pub(crate) name: String,
    #[serde(default)] pub(crate) description: String,
    #[serde(default)] pub(crate) agent_context: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionStatusKind {
    Running,
    NeedsYou,
    Complete,
}

impl SessionStatusKind {
    pub(crate) fn from_str(s: &str) -> Self {
        match s {
            "running" => Self::Running,
            "needs_you" => Self::NeedsYou,
            _ => Self::Complete,
        }
    }

    pub(crate) fn i18n_key(self) -> &'static str {
        match self {
            Self::Running => "sess_status.running",
            Self::NeedsYou => "sess_status.needs_you",
            Self::Complete => "sess_status.complete",
        }
    }

    pub(crate) fn css(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::NeedsYou => "needs-you",
            Self::Complete => "complete",
        }
    }
}

/// One configured model profile (mirrors `models::ModelProfile` in src-tauri).
#[derive(Clone, Deserialize)]
pub(crate) struct ModelProfile {
    pub(crate) id: String,
    pub(crate) label: String,
    #[serde(default)] pub(crate) provider: String,
    #[serde(default)] pub(crate) api_url: String,
    #[serde(default)] pub(crate) model: String,
    #[allow(dead_code)] #[serde(default)] pub(crate) has_api_key: bool,
    #[serde(default)] pub(crate) active: bool,
    #[serde(default)] pub(crate) max_tokens: u64,
    #[serde(default)] pub(crate) reasoning_effort: String,
    #[serde(default)] pub(crate) supports_vision: bool,
    #[serde(default)] pub(crate) use_for_vision: bool,
    #[serde(default)] pub(crate) runner_command: String,
    #[serde(default)] pub(crate) runner_profile: String,
    #[serde(default)] pub(crate) runner_sandbox: String,
    #[serde(default, alias = "runner_web_search")] pub(crate) runner_web_search_mode: String,
    #[serde(default)] pub(crate) runner_claude_command: String,
    #[serde(default)] pub(crate) runner_persistent: bool,
    #[serde(default)] pub(crate) normal_model: String,
    #[serde(default)] pub(crate) normal_reasoning_effort: String,
    #[serde(default)] pub(crate) plan_model: String,
    #[serde(default)] pub(crate) plan_reasoning_effort: String,
    #[serde(default)] pub(crate) service_tier: String,
    #[serde(default)] pub(crate) personality: String,
    #[serde(default)] pub(crate) reasoning_summary: String,
    #[serde(default)] pub(crate) verbosity: String,
}

/// A user-definable agent persona (mirrors `specialists::Specialist` in src-tauri).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct Specialist {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)] pub(crate) icon: String,
    #[serde(default)] pub(crate) color: String,
    #[serde(default)] pub(crate) description: String,
    #[serde(default)] pub(crate) instructions: String,
    #[serde(default)] pub(crate) model_id: String,
    #[serde(default)] pub(crate) skills: Option<Vec<String>>,
    #[serde(default)] pub(crate) connectors: Option<Vec<String>>,
    #[serde(default)] pub(crate) builtin: bool,
}

#[derive(Clone, Deserialize)]
pub(crate) struct RecentSession {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) title: String,
    #[allow(dead_code)] #[serde(default)] pub(crate) ts: i64,
    #[serde(default)] pub(crate) status: String,
}

#[derive(Clone, serde::Deserialize, PartialEq)]
pub(crate) struct SkillRow {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(default)] pub(crate) tags: Vec<String>,
    pub(crate) enabled: bool,
    pub(crate) builtin: bool,
    #[allow(dead_code)] pub(crate) dir: String,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct ConnRow { pub(crate) id: String, pub(crate) name: String, pub(crate) enabled: bool, pub(crate) transport: ConnTransport }
#[derive(Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub(crate) enum ConnTransport {
    Stdio { command: String, #[serde(default)] args: Vec<String>, #[allow(dead_code)] #[serde(default)] env: Vec<(String,String)>, #[allow(dead_code)] #[serde(default)] cwd: Option<String> },
    Http  { url: String, #[serde(default)] headers: Vec<(String,String)> },
}
#[derive(Clone, serde::Deserialize)]
pub(crate) struct ConnView { pub(crate) connections: Vec<ConnRow> }

// Multi-level connectors tree (bundled bio-tools domains + custom connections).
fn default_tool_mode() -> String { "allow".into() }
#[derive(Clone, serde::Deserialize)]
pub(crate) struct ConnectorTool {
    pub(crate) name: String,
    #[serde(default = "default_tool_mode")] pub(crate) mode: String,
    #[serde(default)] pub(crate) description: String,
    #[allow(dead_code)] #[serde(default, rename = "inputSchema")] pub(crate) input_schema: serde_json::Value,
}
#[derive(Clone, serde::Deserialize)]
pub(crate) struct ConnectorInfo {
    pub(crate) key: String,
    pub(crate) name: String,
    pub(crate) kind: String,
    #[allow(dead_code)] pub(crate) enabled: bool,
    pub(crate) skip_approvals: bool,
    #[allow(dead_code)] pub(crate) transport: String,
    #[allow(dead_code)] pub(crate) subtitle: String,
    pub(crate) tools: Vec<ConnectorTool>,
}
#[derive(Clone, serde::Deserialize)]
pub(crate) struct ConnectorsView {
    pub(crate) connectors: Vec<ConnectorInfo>,
    /// Global approval scope: "full" | "auto" | "ask".
    pub(crate) scope: String,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct ApprovalGrantRow {
    pub(crate) scope: String,
    #[serde(default)]
    pub(crate) session_id: Option<String>,
    #[serde(default)]
    pub(crate) project_id: Option<String>,
    pub(crate) kind: String,
    pub(crate) target: String,
    pub(crate) label: String,
}

// Simple flat form state (kind + raw text fields; args/env/headers entered as text, parsed on save).
#[derive(Clone, Default)]
pub(crate) struct ConnForm { pub(crate) id: Option<String>, pub(crate) name: String, pub(crate) kind: String, pub(crate) command: String, pub(crate) args: String, pub(crate) url: String, pub(crate) headers: String, pub(crate) enabled: bool }

#[derive(Clone, Default)]
pub(crate) struct ModelForm {
    pub(crate) id: Option<String>,
    pub(crate) label: String,
    pub(crate) provider: String,
    pub(crate) api_url: String,
    pub(crate) model: String,
    pub(crate) max_tokens: u64,
    pub(crate) reasoning_effort: String,
    pub(crate) supports_vision: bool,
    pub(crate) use_for_vision: bool,
    pub(crate) runner_command: String,
    pub(crate) runner_profile: String,
    pub(crate) runner_sandbox: String,
    pub(crate) runner_web_search_mode: String,
    pub(crate) runner_claude_command: String,
    pub(crate) runner_persistent: bool,
    pub(crate) normal_model: String,
    pub(crate) normal_reasoning_effort: String,
    pub(crate) plan_model: String,
    pub(crate) plan_reasoning_effort: String,
    pub(crate) service_tier: String,
    pub(crate) personality: String,
    pub(crate) reasoning_summary: String,
    pub(crate) verbosity: String,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct MemoryFile {
    pub(crate) name: String,
    pub(crate) preview: String,
    pub(crate) bytes: u64,
}

#[derive(Deserialize, Clone)]
pub(crate) struct MemoryView {
    pub(crate) enabled: bool,
    pub(crate) today_file: String,
    pub(crate) files: Vec<MemoryFile>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct BootstrapStatus {
    pub(crate) skills_loaded: usize,
    pub(crate) python_ok: bool,
    pub(crate) mcp_catalog: usize,
    pub(crate) uv_ok: bool,
    pub(crate) node_ok: bool,
    #[allow(dead_code)] pub(crate) npm_ok: bool,
    pub(crate) sci_ok: bool,
    pub(crate) pixi_ok: bool,
    pub(crate) app_version: String,
    pub(crate) workspace: String,
    pub(crate) errors: Vec<String>,
}

#[derive(Deserialize, Clone)]
pub(crate) struct Capabilities {
    pub(crate) mcp_servers: Vec<String>,
    pub(crate) memory_files: Vec<MemoryFile>,
    pub(crate) project: ProjectInfo,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct OnboardingState {
    pub(crate) show: bool,
    pub(crate) has_api_key: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RightTab {
    Artifacts,
    Notebook,
    File,
    Provenance,
    Hosts,
    SideChat,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct ExecutionContext {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) label: String,
    pub(crate) config_json: String,
    pub(crate) capabilities_json: String,
    pub(crate) last_probe_at: Option<i64>,
    pub(crate) last_probe_status: Option<String>,
    pub(crate) last_probe_error: Option<String>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

#[derive(Deserialize, Clone)]
#[allow(dead_code)]
pub(crate) struct RunRecord {
    pub(crate) id: String,
    pub(crate) project_id: String,
    pub(crate) frame_id: Option<String>,
    pub(crate) context_id: String,
    pub(crate) title: String,
    pub(crate) kind: String,
    pub(crate) status: String,
    pub(crate) command: Option<String>,
    pub(crate) script_path: Option<String>,
    pub(crate) input_refs_json: String,
    pub(crate) output_specs_json: String,
    pub(crate) created_at: i64,
    pub(crate) started_at: Option<i64>,
    pub(crate) ended_at: Option<i64>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) stdout_tail: Option<String>,
    pub(crate) stderr_tail: Option<String>,
    #[serde(rename = "remote_workdir", alias = "remoteWorkdir")]
    pub(crate) remote_workdir: Option<String>,
    pub(crate) remote_handle_json: Option<String>,
    pub(crate) timeout_secs: Option<i64>,
    pub(crate) last_polled_at: Option<i64>,
    #[serde(rename = "last_poll_error", alias = "lastPollError")]
    pub(crate) last_poll_error: Option<String>,
    pub(crate) env_snapshot_json: String,
}

/// Provenance for a produced file — mirrors the `get_artifact_provenance`
/// Tauri command output (src-tauri `ArtifactProvenance`). Deserialize only.
#[derive(Clone, Deserialize, Default)]
pub(crate) struct ArtifactProvenance {
    pub(crate) code: String,
    pub(crate) language: String,
    pub(crate) output: String,
    #[allow(dead_code)]
    pub(crate) exit_status: String,
    #[serde(default)]
    pub(crate) inputs: Vec<ProvInput>,
    pub(crate) env: Option<ProvEnv>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ProvInput {
    pub(crate) path: String,
    pub(crate) produced_here: bool,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ProvEnv {
    #[allow(dead_code)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) packages: Vec<ProvPkg>,
}

#[derive(Clone, Deserialize)]
pub(crate) struct ProvPkg {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) version: String,
}
