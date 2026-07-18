use crate::{acp, acp_bridge_launch, build_provider_config, load_settings, models, ActiveProject};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tauri::State;
use tokio::sync::Mutex;
use wisp_acp::{
    acp::schema::v1::{ContentBlock, McpServer, McpServerStdio, SessionId, TextContent},
    AcpPermissionKind, AcpSessionEvent, AcpSessionHandle, AcpStopReason, AcpUpdateKind,
};
use wisp_core::{
    AgentArtifact, AgentBackend, AgentDelegationRequest, AgentDelegationResponse, AgentDelegator,
    AgentEvidence, AgentRole, AgentSessionPolicy, AgentUsage, DelegationExecutionObserver,
    DelegationExecutionResult, DelegationExecutionStatus, DelegationExecutor, DelegationPlan,
    DelegationStatus, PermissionSet, ValidatedAgentDelegationRequest,
};
use wisp_llm::Message;
use wisp_store::{
    AcpSessionBinding, AgentWorkflowAttempt, AgentWorkflowAttemptStatus, AgentWorkflowStatus, Store,
};

const RESULT_INSTRUCTIONS: &str = "Return one JSON object and no Markdown fence. Include summary (string), files_changed (array), diff_summary (string), artifacts (array), evidence (array), tests (array), and risks (array). Do not delegate further.";

#[tauri::command]
pub(crate) async fn run_agent_workflow(
    state: State<'_, crate::AppState>,
    window: tauri::WebviewWindow,
    workflow_id: String,
) -> Result<DelegationExecutionResult, String> {
    let project = state.active(window.label());
    let _project_activity = state.begin_project_activity(&project.id)?;
    let workflow = state
        .store
        .get_agent_workflow(&workflow_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Agent workflow does not exist".to_string())?;
    if workflow.project_id != project.id {
        return Err("Agent workflow does not belong to the active project".into());
    }
    let plan: DelegationPlan = serde_json::from_str(&workflow.plan_json)
        .map_err(|error| format!("Agent workflow plan is invalid: {error}"))?;
    if plan.id != workflow.id {
        return Err("Agent workflow plan identity does not match its persisted record".into());
    }
    let delegator = Arc::new(TauriDelegator::new(
        state.store.clone(),
        state.app_data.clone(),
        project,
    ));
    let observer = Arc::new(StoreDelegationObserver::new(state.store.clone()));
    let result = DelegationExecutor::new(delegator)
        .with_observer(observer)
        .execute(plan)
        .await;
    if result.is_err() {
        let _ = state
            .store
            .transition_agent_workflow_status(
                &workflow_id,
                AgentWorkflowStatus::Running,
                AgentWorkflowStatus::Failed,
            )
            .await;
    }
    result.map_err(|error| error.to_string())
}

pub(crate) struct TauriDelegator {
    local: LocalDelegator,
    acp: AcpDelegator,
}

impl TauriDelegator {
    pub(crate) fn new(store: Store, app_data: std::path::PathBuf, project: ActiveProject) -> Self {
        Self {
            local: LocalDelegator {
                store: store.clone(),
                project: project.clone(),
            },
            acp: AcpDelegator {
                store,
                app_data,
                project,
                active: Arc::new(Mutex::new(HashMap::new())),
            },
        }
    }
}

#[async_trait]
impl AgentDelegator for TauriDelegator {
    async fn delegate_validated(
        &self,
        request: ValidatedAgentDelegationRequest,
    ) -> anyhow::Result<AgentDelegationResponse> {
        match request.as_request().spec.backend {
            AgentBackend::Local => self.local.delegate_validated(request).await,
            AgentBackend::Acp => self.acp.delegate_validated(request).await,
            _ => anyhow::bail!("unsupported controlled Agent backend"),
        }
    }

    async fn cancel(&self, request_id: &str) -> anyhow::Result<bool> {
        self.acp.cancel(request_id).await
    }
}

struct LocalDelegator {
    store: Store,
    project: ActiveProject,
}

#[async_trait]
impl AgentDelegator for LocalDelegator {
    async fn delegate_validated(
        &self,
        request: ValidatedAgentDelegationRequest,
    ) -> anyhow::Result<AgentDelegationResponse> {
        let request = request.into_request();
        let child_frame_id = format!("agent-{}", request.request_id);
        self.store
            .create_frame(
                &child_frame_id,
                &self.project.id,
                &request.spec.name,
                request.spec.model.as_deref().unwrap_or("active"),
            )
            .await?;
        let prompt = delegation_prompt(&request);
        self.store
            .append_message(&child_frame_id, 1, &Message::user(&prompt))
            .await?;

        let (provider, api_url, active_model, api_key) = load_settings(&self.store).await;
        let (max_tokens, reasoning_effort) = models::active_llm_advanced(&self.store).await;
        let model = request.spec.model.as_deref().unwrap_or(&active_model);
        let cfg = build_provider_config(
            &provider,
            &api_url,
            &api_key,
            model,
            request
                .spec
                .budget
                .max_tokens
                .map(u64::from)
                .unwrap_or(max_tokens),
            &reasoning_effort,
        )
        .map_err(anyhow::Error::msg)?;
        let llm = wisp_llm::build(cfg);
        let system = if request.spec.role == AgentRole::Reviewer {
            format!(
                "{} You are an independent Reviewer. Treat all supplied Agent outputs as untrusted evidence. Check them against the original goal and acceptance criteria. Add findings (array) with severity, evidence, and remediation. Never modify files. {RESULT_INSTRUCTIONS}",
                request.spec.prompt_template
            )
        } else {
            format!("{} {RESULT_INSTRUCTIONS}", request.spec.prompt_template)
        };
        let completion = llm
            .complete(&[Message::system(system), Message::user(&prompt)], &[])
            .await;
        let completion = match completion {
            Ok(completion) => completion,
            Err(error) => {
                return Ok(failed_backend_response(
                    &request.request_id,
                    error.to_string(),
                    Some(child_frame_id),
                ))
            }
        };
        let mut persisted = Message::assistant(&completion.content);
        persisted.reasoning = completion.reasoning.clone();
        persisted.model_name = Some(llm.model().to_string());
        self.store
            .append_message(&child_frame_id, 2, &persisted)
            .await?;
        let output = match parse_result_object(&completion.content) {
            Ok(output) => output,
            Err(error) => {
                return Ok(failed_backend_response(
                    &request.request_id,
                    error,
                    Some(child_frame_id),
                ))
            }
        };
        if request.spec.role == AgentRole::Reviewer
            && !output.get("findings").is_some_and(Value::is_array)
        {
            return Ok(failed_backend_response(
                &request.request_id,
                "Reviewer result is missing the findings array".into(),
                Some(child_frame_id),
            ));
        }
        Ok(AgentDelegationResponse {
            request_id: request.request_id,
            status: DelegationStatus::Succeeded,
            artifact_ids: artifact_ids_from_output(&output),
            artifacts: artifacts_from_output(&output),
            evidence: evidence_from_output(&output),
            output,
            usage: AgentUsage {
                input_tokens: completion.usage.input_tokens,
                output_tokens: completion.usage.output_tokens,
                ..Default::default()
            },
            agent_session_id: None,
            child_frame_id: Some(child_frame_id),
            error: None,
        })
    }
}

#[derive(Clone)]
struct ActiveAcpRequest {
    handle: Arc<AcpSessionHandle>,
    session_id: SessionId,
}

struct AcpDelegator {
    store: Store,
    app_data: std::path::PathBuf,
    project: ActiveProject,
    active: Arc<Mutex<HashMap<String, ActiveAcpRequest>>>,
}

#[async_trait]
impl AgentDelegator for AcpDelegator {
    async fn delegate_validated(
        &self,
        request: ValidatedAgentDelegationRequest,
    ) -> anyhow::Result<AgentDelegationResponse> {
        let request = request.into_request();
        let profiles = acp::profiles(&self.store).await;
        let requested_profile_id = request.spec.model.as_deref();
        let requested_profile = requested_profile_id
            .and_then(|id| profiles.iter().find(|profile| profile.id == id))
            .cloned();
        if requested_profile_id.is_some() && requested_profile.is_none() {
            anyhow::bail!("the selected ACP Agent profile does not exist");
        }
        let profile = if request.spec.template_id == "code_execution" {
            match requested_profile {
                Some(profile) if is_codex_profile(&profile) => profile,
                Some(_) => anyhow::bail!("code execution requires a Codex ACP Agent profile"),
                None => profiles
                    .iter()
                    .find(|profile| is_codex_profile(profile))
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("no Codex ACP Agent profile is configured"))?,
            }
        } else {
            requested_profile
                .or_else(|| profiles.first().cloned())
                .ok_or_else(|| anyhow::anyhow!("no ACP Agent profile is configured"))?
        };
        let reusable_candidate = match request.spec.session_policy {
            AgentSessionPolicy::New => None,
            AgentSessionPolicy::ReuseIfAvailable | AgentSessionPolicy::RequireExisting => {
                self.store
                    .latest_agent_workflow_step_session(&request.step_id)
                    .await?
            }
        };
        let reusable = if let Some((agent_session_id, frame_id)) = reusable_candidate {
            let binding = self.store.get_acp_session(&frame_id).await?;
            let valid = binding.as_ref().is_some_and(|binding| {
                binding.agent_session_id == agent_session_id
                    && binding.agent_profile_id == profile.id
                    && binding.profile_fingerprint == acp::fingerprint(&profile)
                    && std::path::Path::new(&binding.cwd) == self.project.root
            });
            if valid {
                Some((agent_session_id, frame_id))
            } else if request.spec.session_policy == AgentSessionPolicy::RequireExisting {
                anyhow::bail!("the saved ACP session no longer matches its profile or workspace");
            } else {
                None
            }
        } else {
            None
        };
        if request.spec.session_policy == AgentSessionPolicy::RequireExisting && reusable.is_none()
        {
            anyhow::bail!("this Agent requires an existing ACP session");
        }
        let child_frame_id = reusable
            .as_ref()
            .map(|(_, frame_id)| frame_id.clone())
            .unwrap_or_else(|| format!("agent-{}", request.request_id));
        if reusable.is_none() {
            self.store
                .create_frame(
                    &child_frame_id,
                    &self.project.id,
                    &request.spec.name,
                    &profile.label,
                )
                .await?;
        }
        let prompt_text = delegation_prompt(&request);
        let next_seq = self.store.load_messages(&child_frame_id).await?.len() as i64 + 1;
        self.store
            .append_message(&child_frame_id, next_seq, &Message::user(&prompt_text))
            .await?;

        let handle = Arc::new(AcpSessionHandle::launch(acp::launch_profile(&profile)).await?);
        let (command, args) = acp_bridge_launch(&self.app_data, &self.project, &child_frame_id)
            .map_err(anyhow::Error::msg)?;
        let bridge = vec![McpServer::Stdio(
            McpServerStdio::new("wisp-science", command).args(args),
        )];
        let session_id = if let Some((agent_session_id, _)) = &reusable {
            let id = SessionId::new(agent_session_id.clone());
            match handle
                .resume_session(id.clone(), &self.project.root, bridge.clone())
                .await
            {
                Ok(_) => id,
                Err(wisp_acp::AcpError::Unsupported(_)) => {
                    handle
                        .load_session(id.clone(), &self.project.root, bridge)
                        .await?;
                    id
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            handle
                .new_session(&self.project.root, bridge)
                .await?
                .session_id
        };
        if reusable.is_none() {
            let info = handle.info();
            let now = chrono::Utc::now().timestamp();
            let implementation = info.implementation.as_ref().map(
                |value| json!({"name":value.name,"title":value.title,"version":value.version}),
            );
            self.store
                .save_acp_session(&AcpSessionBinding {
                    frame_id: child_frame_id.clone(),
                    agent_profile_id: profile.id.clone(),
                    profile_fingerprint: acp::fingerprint(&profile),
                    agent_session_id: session_id.to_string(),
                    cwd: self.project.root.to_string_lossy().into_owned(),
                    protocol_version: i64::from(info.protocol_version),
                    agent_info_json: serde_json::to_string(&implementation)?,
                    capabilities_json: info.capabilities.to_string(),
                    created_at: now,
                    updated_at: now,
                })
                .await?;
        }
        self.active.lock().await.insert(
            request.request_id.clone(),
            ActiveAcpRequest {
                handle: handle.clone(),
                session_id: session_id.clone(),
            },
        );
        let result = run_acp_request(
            &request,
            &self.store,
            &child_frame_id,
            handle.clone(),
            session_id.clone(),
            prompt_text,
            next_seq + 1,
        )
        .await
        .unwrap_or_else(|error| {
            let mut response = failed_backend_response(
                &request.request_id,
                error.to_string(),
                Some(child_frame_id.clone()),
            );
            response.agent_session_id = Some(session_id.to_string());
            response
        });
        self.active.lock().await.remove(&request.request_id);
        handle.shutdown(Duration::from_secs(2)).await;
        Ok(result)
    }

    async fn cancel(&self, request_id: &str) -> anyhow::Result<bool> {
        let Some(active) = self.active.lock().await.remove(request_id) else {
            return Ok(false);
        };
        active.handle.cancel(active.session_id)?;
        let handle = active.handle;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            handle.shutdown(Duration::from_secs(1)).await;
        });
        Ok(true)
    }
}

async fn run_acp_request(
    request: &AgentDelegationRequest,
    store: &Store,
    child_frame_id: &str,
    handle: Arc<AcpSessionHandle>,
    session_id: SessionId,
    prompt_text: String,
    mut next_seq: i64,
) -> anyhow::Result<AgentDelegationResponse> {
    let prompt = handle.prompt(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(prompt_text))],
    );
    tokio::pin!(prompt);
    let mut answer = String::new();
    let mut evidence = Vec::new();
    let mut usage = AgentUsage::default();
    let outcome = loop {
        tokio::select! {
            outcome = &mut prompt => break outcome?,
            event = handle.next_event() => match event {
                Some(AcpSessionEvent::Update { kind, payload, .. }) => {
                    if kind == AcpUpdateKind::AgentMessage {
                        if let Some(text) = acp_text(&payload) {
                            answer.push_str(text);
                        }
                    } else if matches!(kind, AcpUpdateKind::ToolCall | AcpUpdateKind::ToolCallUpdate) {
                        if kind == AcpUpdateKind::ToolCall {
                            usage.tool_calls += 1;
                        }
                        evidence.push(AgentEvidence {
                            kind: "acp_tool".into(),
                            summary: bounded_json(&payload, 2_000),
                            reference: payload.get("toolCallId").and_then(Value::as_str).map(str::to_string),
                        });
                    } else if kind == AcpUpdateKind::Usage {
                        usage.input_tokens = json_u64(&payload, &["inputTokens", "input_tokens"]);
                        usage.output_tokens = json_u64(&payload, &["outputTokens", "output_tokens"]);
                    }
                }
                Some(AcpSessionEvent::Permission(permission)) => {
                    let allowed = permission_option(&permission, &request.spec.permissions);
                    handle.respond_permission(permission.request_id, allowed)?;
                }
                Some(AcpSessionEvent::Exited { error }) => anyhow::bail!(error.unwrap_or_else(|| "ACP Agent exited".into())),
                None => anyhow::bail!("ACP Agent event stream closed"),
            }
        }
    };
    let drain_deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < drain_deadline {
        let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), handle.next_event()).await
        else {
            break;
        };
        if let AcpSessionEvent::Update { kind, payload, .. } = event {
            if kind == AcpUpdateKind::AgentMessage {
                if let Some(text) = acp_text(&payload) {
                    answer.push_str(text);
                }
            }
        }
    }
    let mut assistant = Message::assistant(&answer);
    assistant.model_name = request.spec.model.clone();
    store
        .append_message(child_frame_id, next_seq, &assistant)
        .await?;
    next_seq += 1;
    for item in &evidence {
        store
            .append_message(
                child_frame_id,
                next_seq,
                &Message::tool(
                    item.reference.as_deref().unwrap_or("acp-tool"),
                    "acp:delegation",
                    &item.summary,
                ),
            )
            .await?;
        next_seq += 1;
    }

    if outcome.stop_reason == AcpStopReason::Cancelled {
        return Ok(AgentDelegationResponse {
            request_id: request.request_id.clone(),
            status: DelegationStatus::Cancelled,
            output: json!({}),
            artifact_ids: vec![],
            artifacts: vec![],
            evidence,
            usage,
            agent_session_id: Some(session_id.to_string()),
            child_frame_id: Some(child_frame_id.into()),
            error: None,
        });
    }
    if outcome.stop_reason != AcpStopReason::EndTurn {
        return Ok(failed_backend_response(
            &request.request_id,
            format!("ACP Agent stopped with {:?}", outcome.stop_reason),
            Some(child_frame_id.into()),
        ));
    }
    let output = match parse_result_object(&answer) {
        Ok(output) => output,
        Err(error) => {
            return Ok(failed_backend_response(
                &request.request_id,
                error,
                Some(child_frame_id.into()),
            ))
        }
    };
    let artifacts = store
        .list_artifacts(child_frame_id)
        .await?
        .into_iter()
        .map(|(id, name, kind, path, _)| AgentArtifact {
            id,
            name,
            kind,
            path: Some(path),
        })
        .collect::<Vec<_>>();
    let artifact_ids = artifacts
        .iter()
        .map(|artifact| artifact.id.clone())
        .collect();
    evidence.extend(evidence_from_output(&output));
    Ok(AgentDelegationResponse {
        request_id: request.request_id.clone(),
        status: DelegationStatus::Succeeded,
        output,
        artifact_ids,
        artifacts,
        evidence,
        usage,
        agent_session_id: Some(session_id.to_string()),
        child_frame_id: Some(child_frame_id.into()),
        error: None,
    })
}

fn permission_option(
    request: &wisp_acp::AcpPermissionRequest,
    permissions: &PermissionSet,
) -> Option<String> {
    let tool = request.tool_call.to_string().to_lowercase();
    let allowed_tool = permissions.tools.iter().any(|allowed| {
        let allowed = allowed.to_lowercase();
        tool.contains(&allowed)
            || (allowed == "shell"
                && ["shell", "bash", "execute", "terminal"]
                    .iter()
                    .any(|name| tool.contains(name)))
            || (allowed == "read_file" && tool.contains("read"))
            || (allowed == "write_file" && ["write", "edit"].iter().any(|name| tool.contains(name)))
    });
    let kind = if allowed_tool && permissions.write {
        AcpPermissionKind::AllowOnce
    } else {
        AcpPermissionKind::RejectOnce
    };
    request
        .options
        .iter()
        .find(|option| option.kind == kind)
        .or_else(|| {
            request.options.iter().find(|option| {
                matches!(
                    option.kind,
                    AcpPermissionKind::RejectOnce | AcpPermissionKind::RejectAlways
                )
            })
        })
        .map(|option| option.id.clone())
}

fn is_codex_profile(profile: &acp::AcpAgentProfile) -> bool {
    profile.command.to_lowercase().contains("codex-acp")
        || profile
            .args
            .iter()
            .any(|argument| argument.to_lowercase().contains("codex-acp"))
}

pub(crate) struct StoreDelegationObserver {
    store: Store,
    attempt_ids: Mutex<HashMap<String, String>>,
}

impl StoreDelegationObserver {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            store,
            attempt_ids: Mutex::new(HashMap::new()),
        }
    }

    async fn create_started_attempt(
        &self,
        request: &AgentDelegationRequest,
    ) -> anyhow::Result<AgentWorkflowAttempt> {
        let attempt_number = self
            .store
            .next_agent_workflow_attempt_number(&request.step_id)
            .await?;
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let mut attempt = AgentWorkflowAttempt::queued(
            &attempt_id,
            &request.workflow_id,
            &request.step_id,
            attempt_number,
            &request.request_id,
            request.spec.backend.as_str(),
            serde_json::to_string(&request.input)?,
        )?;
        self.store.create_agent_workflow_attempt(&attempt).await?;
        attempt.status = AgentWorkflowAttemptStatus::Running;
        attempt.started_at = Some(chrono::Utc::now().timestamp());
        if !self
            .store
            .update_agent_workflow_attempt(&attempt, AgentWorkflowAttemptStatus::Queued)
            .await?
        {
            anyhow::bail!("Agent attempt was changed before it could start");
        }
        self.attempt_ids
            .lock()
            .await
            .insert(request.request_id.clone(), attempt_id);
        Ok(attempt)
    }
}

#[async_trait]
impl DelegationExecutionObserver for StoreDelegationObserver {
    async fn workflow_started(&self, plan: &DelegationPlan) -> anyhow::Result<()> {
        if !self
            .store
            .transition_agent_workflow_status(
                &plan.id,
                AgentWorkflowStatus::Approved,
                AgentWorkflowStatus::Running,
            )
            .await?
        {
            anyhow::bail!("Agent workflow is not approved or is already running");
        }
        Ok(())
    }

    async fn step_started(&self, request: &AgentDelegationRequest) -> anyhow::Result<()> {
        self.create_started_attempt(request).await?;
        Ok(())
    }

    async fn step_finished(
        &self,
        request: &AgentDelegationRequest,
        response: &AgentDelegationResponse,
    ) -> anyhow::Result<()> {
        let attempt_id = self
            .attempt_ids
            .lock()
            .await
            .get(&request.request_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Agent attempt is not persisted"))?;
        let mut attempt = self
            .store
            .get_agent_workflow_attempt(&attempt_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Agent attempt disappeared"))?;
        attempt.status = match response.status {
            DelegationStatus::Succeeded => AgentWorkflowAttemptStatus::Succeeded,
            DelegationStatus::Cancelled => AgentWorkflowAttemptStatus::Cancelled,
            DelegationStatus::Blocked => AgentWorkflowAttemptStatus::Failed,
            _ => AgentWorkflowAttemptStatus::Failed,
        };
        attempt.response_json = Some(serde_json::to_string(response)?);
        attempt.output_json = serde_json::to_string(&response.output)?;
        attempt.artifact_ids_json = serde_json::to_string(&response.artifact_ids)?;
        attempt.evidence_json = serde_json::to_string(&response.evidence)?;
        attempt.error = response.error.clone();
        attempt.agent_session_id = response.agent_session_id.clone();
        attempt.child_frame_id = response.child_frame_id.clone();
        attempt.input_tokens = i64::try_from(response.usage.input_tokens).unwrap_or(i64::MAX);
        attempt.output_tokens = i64::try_from(response.usage.output_tokens).unwrap_or(i64::MAX);
        attempt.tool_calls = i64::try_from(response.usage.tool_calls).unwrap_or(i64::MAX);
        attempt.cost_microunits = i64::try_from(response.usage.cost_microunits).unwrap_or(i64::MAX);
        attempt.finished_at = Some(chrono::Utc::now().timestamp());
        if !self
            .store
            .update_agent_workflow_attempt(&attempt, AgentWorkflowAttemptStatus::Running)
            .await?
        {
            anyhow::bail!("Agent attempt terminal state lost a concurrent update");
        }
        Ok(())
    }

    async fn step_blocked(
        &self,
        request: &AgentDelegationRequest,
        reason: &str,
    ) -> anyhow::Result<()> {
        let attempt_number = self
            .store
            .next_agent_workflow_attempt_number(&request.step_id)
            .await?;
        let mut attempt = AgentWorkflowAttempt::queued(
            uuid::Uuid::new_v4().to_string(),
            &request.workflow_id,
            &request.step_id,
            attempt_number,
            &request.request_id,
            request.spec.backend.as_str(),
            serde_json::to_string(&request.input)?,
        )?;
        self.store.create_agent_workflow_attempt(&attempt).await?;
        attempt.status = AgentWorkflowAttemptStatus::Blocked;
        attempt.error = Some(reason.into());
        attempt.finished_at = Some(chrono::Utc::now().timestamp());
        if !self
            .store
            .update_agent_workflow_attempt(&attempt, AgentWorkflowAttemptStatus::Queued)
            .await?
        {
            anyhow::bail!("blocked Agent attempt lost a concurrent update");
        }
        Ok(())
    }

    async fn workflow_finished(
        &self,
        plan: &DelegationPlan,
        status: DelegationExecutionStatus,
    ) -> anyhow::Result<()> {
        let status = match status {
            DelegationExecutionStatus::Succeeded => AgentWorkflowStatus::Succeeded,
            DelegationExecutionStatus::Failed => AgentWorkflowStatus::Failed,
            DelegationExecutionStatus::Cancelled => AgentWorkflowStatus::Cancelled,
        };
        if !self
            .store
            .transition_agent_workflow_status(&plan.id, AgentWorkflowStatus::Running, status)
            .await?
        {
            anyhow::bail!("Agent workflow terminal state lost a concurrent update");
        }
        Ok(())
    }
}

fn delegation_prompt(request: &AgentDelegationRequest) -> String {
    format!(
        "Controlled Agent task\nName: {}\nGoal: {}\nContext: {}\nAcceptance criteria:\n{}\nInput JSON:\n{}\n\n{}",
        request.spec.name,
        request.spec.goal,
        request.spec.context_summary,
        request
            .spec
            .acceptance_criteria
            .iter()
            .map(|criterion| format!("- {criterion}"))
            .collect::<Vec<_>>()
            .join("\n"),
        serde_json::to_string_pretty(&request.input).unwrap_or_else(|_| "{}".into()),
        RESULT_INSTRUCTIONS,
    )
}

fn parse_result_object(raw: &str) -> Result<Value, String> {
    let start = raw
        .find('{')
        .ok_or_else(|| "Agent returned no JSON object".to_string())?;
    let end = raw
        .rfind('}')
        .filter(|end| *end >= start)
        .ok_or_else(|| "Agent returned an incomplete JSON object".to_string())?;
    let value: Value = serde_json::from_str(&raw[start..=end])
        .map_err(|error| format!("Agent returned invalid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "Agent result must be an object".to_string())?;
    if !object.get("summary").is_some_and(Value::is_string) {
        return Err("Agent result is missing the summary string".into());
    }
    if !object.get("diff_summary").is_some_and(Value::is_string) {
        return Err("Agent result is missing the diff_summary string".into());
    }
    for field in ["files_changed", "artifacts", "evidence", "tests", "risks"] {
        if !object.get(field).is_some_and(Value::is_array) {
            return Err(format!("Agent result is missing the {field} array"));
        }
    }
    Ok(value)
}

fn evidence_from_output(output: &Value) -> Vec<AgentEvidence> {
    output
        .get("evidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|value| match value {
            Value::Object(value) => AgentEvidence {
                kind: value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("agent")
                    .into(),
                summary: value
                    .get("summary")
                    .or_else(|| value.get("evidence"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                reference: value
                    .get("reference")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            },
            value => AgentEvidence {
                kind: "agent".into(),
                summary: value.as_str().unwrap_or_default().into(),
                reference: None,
            },
        })
        .collect()
}

fn artifacts_from_output(output: &Value) -> Vec<AgentArtifact> {
    output
        .get("artifacts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .filter_map(|value| {
            Some(AgentArtifact {
                id: value.get("id")?.as_str()?.into(),
                name: value
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                kind: value
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                path: value
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect()
}

fn artifact_ids_from_output(output: &Value) -> Vec<String> {
    artifacts_from_output(output)
        .into_iter()
        .map(|artifact| artifact.id)
        .collect()
}

fn failed_backend_response(
    request_id: &str,
    error: String,
    child_frame_id: Option<String>,
) -> AgentDelegationResponse {
    AgentDelegationResponse {
        request_id: request_id.into(),
        status: DelegationStatus::Failed,
        output: Value::Object(Map::new()),
        artifact_ids: vec![],
        artifacts: vec![],
        evidence: vec![],
        usage: Default::default(),
        agent_session_id: None,
        child_frame_id,
        error: Some(error),
    }
}

fn acp_text(payload: &Value) -> Option<&str> {
    payload
        .get("content")
        .and_then(|content| content.get("text"))
        .and_then(|content| content.get("text"))
        .and_then(Value::as_str)
        .or_else(|| payload.get("text").and_then(Value::as_str))
}

fn bounded_json(value: &Value, limit: usize) -> String {
    let raw = serde_json::to_string(value).unwrap_or_default();
    if raw.len() <= limit {
        raw
    } else {
        format!("{}…", &raw[..raw.floor_char_boundary(limit)])
    }
}

fn json_u64(value: &Value, keys: &[&str]) -> u64 {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_u64))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wisp_core::{
        AgentTemplateRegistry, DelegationMode, DelegationPlanner, ValidatedAgentDelegationRequest,
    };
    use wisp_store::{AgentWorkflow, AgentWorkflowStep};

    #[test]
    fn structured_result_parser_rejects_prose_and_incomplete_results() {
        assert!(parse_result_object("done").is_err());
        assert!(parse_result_object(r#"{"summary":"done"}"#).is_err());
        assert!(parse_result_object(r#"{"summary":"done","files_changed":[],"diff_summary":"","artifacts":[],"evidence":[],"tests":[],"risks":[]}"#)
        .is_ok());
    }

    #[test]
    fn codex_profile_detection_handles_binary_and_npx_forms() {
        assert!(is_codex_profile(&acp::AcpAgentProfile {
            id: "direct".into(),
            label: "Codex".into(),
            command: "/usr/local/bin/codex-acp".into(),
            args: vec![],
        }));
        assert!(is_codex_profile(&acp::AcpAgentProfile {
            id: "npx".into(),
            label: "Codex".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "@agentclientprotocol/codex-acp".into()],
        }));
    }

    #[test]
    fn acp_permission_choice_respects_tool_and_write_ceiling() {
        let request = wisp_acp::AcpPermissionRequest {
            request_id: "p".into(),
            session_id: "s".into(),
            tool_call: json!({"title":"execute shell"}),
            options: vec![
                wisp_acp::AcpPermissionOption {
                    id: "allow".into(),
                    name: "Allow".into(),
                    kind: AcpPermissionKind::AllowOnce,
                },
                wisp_acp::AcpPermissionOption {
                    id: "reject".into(),
                    name: "Reject".into(),
                    kind: AcpPermissionKind::RejectOnce,
                },
            ],
        };
        assert_eq!(
            permission_option(
                &request,
                &PermissionSet {
                    tools: vec!["shell".into()],
                    write: true,
                    ..Default::default()
                }
            ),
            Some("allow".into())
        );
        assert_eq!(
            permission_option(
                &request,
                &PermissionSet {
                    tools: vec!["shell".into()],
                    write: false,
                    ..Default::default()
                }
            ),
            Some("reject".into())
        );
    }

    struct SuccessfulDelegator;

    #[async_trait]
    impl AgentDelegator for SuccessfulDelegator {
        async fn delegate_validated(
            &self,
            request: ValidatedAgentDelegationRequest,
        ) -> anyhow::Result<AgentDelegationResponse> {
            Ok(AgentDelegationResponse {
                request_id: request.as_request().request_id.clone(),
                status: DelegationStatus::Succeeded,
                output: json!({
                    "summary":"complete",
                    "files_changed":[],
                    "diff_summary":"",
                    "artifacts":[],
                    "evidence":[],
                    "tests":[],
                    "risks":[],
                    "findings":[],
                }),
                artifact_ids: vec![],
                artifacts: vec![],
                evidence: vec![],
                usage: AgentUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    tool_calls: 1,
                    cost_microunits: 2,
                },
                agent_session_id: None,
                child_frame_id: None,
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn store_observer_persists_the_complete_execution_lifecycle() {
        let path = std::env::temp_dir().join(format!(
            "wisp_delegation_observer_{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        let store = Store::open(&path).await.unwrap();
        store.create_project("p", "Project", "").await.unwrap();
        let plan = DelegationPlanner
            .suggest(
                "interpret biology gene evidence",
                DelegationMode::Automatic,
                "context",
                &[],
                &[],
                &AgentTemplateRegistry::builtins(),
            )
            .unwrap();
        let mut workflow = AgentWorkflow::new(&plan.id, "p", "workspace", "Delegation").unwrap();
        workflow.plan_json = serde_json::to_string(&plan).unwrap();
        let steps = plan
            .steps
            .iter()
            .enumerate()
            .map(|(position, planned)| {
                let mut step = AgentWorkflowStep::new(
                    &planned.id,
                    &plan.id,
                    position as i64,
                    &planned.spec.agent_id,
                    planned.spec.role.as_str(),
                    planned.spec.backend.as_str(),
                    &planned.spec.prompt_template,
                )
                .unwrap();
                step.template_id = planned.spec.template_id.clone();
                step.spec_json = serde_json::to_string(&planned.spec).unwrap();
                step
            })
            .collect::<Vec<_>>();
        store
            .create_agent_workflow_plan(&workflow, &steps)
            .await
            .unwrap();
        assert!(store
            .approve_agent_workflow_plan(&plan.id, 1)
            .await
            .unwrap());

        let result = DelegationExecutor::new(Arc::new(SuccessfulDelegator))
            .with_observer(Arc::new(StoreDelegationObserver::new(store.clone())))
            .execute(plan.clone())
            .await
            .unwrap();
        assert_eq!(result.status, DelegationExecutionStatus::Succeeded);
        assert_eq!(
            store
                .get_agent_workflow(&plan.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            AgentWorkflowStatus::Succeeded
        );
        let attempts = store.list_agent_workflow_attempts(&plan.id).await.unwrap();
        assert_eq!(attempts.len(), plan.steps.len());
        assert!(attempts
            .iter()
            .all(|attempt| attempt.status == AgentWorkflowAttemptStatus::Succeeded));
        assert!(attempts.iter().all(|attempt| attempt.input_tokens == 10));

        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
