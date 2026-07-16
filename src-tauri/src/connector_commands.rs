use super::{
    bio_domains, clear_idle_agents, connect_mcp, load_approval_scope, load_disabled_connectors,
    load_mcp_connections, load_skip_connectors, load_tool_approvals, refresh_approval_policy,
    save_json_setting, save_mcp_connections, AppState, ApprovalMode, McpConnection, McpTransport,
    Scope,
};
use serde::Serialize;
use tauri::{Emitter, Manager, State};

#[derive(Serialize, Clone)]
pub(super) struct McpConnectionsView {
    connections: Vec<McpConnection>,
}

#[tauri::command]
pub(super) async fn list_mcp_connections(
    state: State<'_, AppState>,
) -> Result<McpConnectionsView, String> {
    Ok(McpConnectionsView {
        connections: load_mcp_connections(&state.store).await,
    })
}

#[tauri::command]
pub(super) async fn add_mcp_connection(
    state: State<'_, AppState>,
    conn: McpConnection,
) -> Result<(), String> {
    let mut conns = load_mcp_connections(&state.store).await;
    conns.push(conn);
    save_mcp_connections(&state.store, &conns).await?;
    clear_idle_agents(&state).await;
    Ok(())
}

#[tauri::command]
pub(super) async fn update_mcp_connection(
    state: State<'_, AppState>,
    conn: McpConnection,
) -> Result<(), String> {
    let mut conns = load_mcp_connections(&state.store).await;
    match conns.iter_mut().find(|c| c.id == conn.id) {
        Some(slot) => *slot = conn,
        None => return Err("connection not found".into()),
    }
    save_mcp_connections(&state.store, &conns).await?;
    clear_idle_agents(&state).await;
    Ok(())
}

#[tauri::command]
pub(super) async fn delete_mcp_connection(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let mut conns = load_mcp_connections(&state.store).await;
    let removed_notion = conns.iter().any(|connection| {
        connection.id == id && matches!(&connection.transport, McpTransport::Notion)
    });
    conns.retain(|c| c.id != id);
    save_mcp_connections(&state.store, &conns).await?;
    if removed_notion {
        crate::notion::forget(&id);
    }
    clear_idle_agents(&state).await;
    Ok(())
}

#[tauri::command]
pub(super) async fn set_mcp_connection_enabled(
    state: State<'_, AppState>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let mut conns = load_mcp_connections(&state.store).await;
    if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
        c.enabled = enabled;
    }
    save_mcp_connections(&state.store, &conns).await?;
    clear_idle_agents(&state).await;
    Ok(())
}

// ── Connectors tree (multi-level Connections UI) ────────────────────────────

#[derive(Serialize, Clone)]
struct ConnectorTool {
    name: String,
    /// Effective approval mode: "allow" | "ask" | "deny".
    mode: String,
}

#[derive(Serialize, Clone)]
struct ConnectorInfo {
    /// Domain slug (bundled) or connection id (custom).
    key: String,
    name: String,
    /// "bundled" | "custom".
    kind: String,
    enabled: bool,
    skip_approvals: bool,
    /// "stdio" | "http" for custom connectors; empty for bundled.
    transport: String,
    /// Command/URL line for custom connectors; empty for bundled.
    subtitle: String,
    /// Tools for bundled connectors (static from domains.json). Custom
    /// connector tools are loaded on demand through `test_mcp_connection`.
    tools: Vec<ConnectorTool>,
}

#[derive(Serialize, Clone)]
pub(super) struct ConnectorsView {
    connectors: Vec<ConnectorInfo>,
    /// Global approval scope ("full" | "auto" | "ask").
    scope: String,
}

#[tauri::command]
pub(super) async fn list_connectors(state: State<'_, AppState>) -> Result<ConnectorsView, String> {
    let store = &state.store;
    let disabled = load_disabled_connectors(store).await;
    let approvals = load_tool_approvals(store).await;
    let skip = load_skip_connectors(store).await;

    let mut connectors = vec![];
    for d in bio_domains() {
        let skip_on = skip.contains(&d.slug);
        let tools = d
            .tools
            .iter()
            .map(|t| ConnectorTool {
                mode: if skip_on {
                    "allow".into()
                } else {
                    approvals.get(t).cloned().unwrap_or_else(|| "allow".into())
                },
                name: t.clone(),
            })
            .collect();
        connectors.push(ConnectorInfo {
            enabled: !disabled.contains(&d.slug),
            key: d.slug,
            name: d.name,
            kind: "bundled".into(),
            skip_approvals: skip_on,
            transport: String::new(),
            subtitle: String::new(),
            tools,
        });
    }
    for c in load_mcp_connections(store).await {
        let (transport, subtitle) = match &c.transport {
            McpTransport::Stdio { command, .. } => ("stdio", command.clone()),
            McpTransport::Http { url, .. } => ("http", url.clone()),
            McpTransport::Notion => ("notion", crate::notion::MCP_URL.into()),
        };
        connectors.push(ConnectorInfo {
            key: c.id,
            name: c.name,
            kind: "custom".into(),
            enabled: c.enabled,
            skip_approvals: false,
            transport: transport.into(),
            subtitle,
            tools: vec![],
        });
    }
    let scope = load_approval_scope(store).await.as_str().to_string();
    Ok(ConnectorsView { connectors, scope })
}

/// Enable/disable a bundled connector (domain). Custom connectors use
/// `set_mcp_connection_enabled` instead.
#[tauri::command]
pub(super) async fn set_connector_enabled(
    state: State<'_, AppState>,
    key: String,
    enabled: bool,
) -> Result<(), String> {
    let mut disabled = load_disabled_connectors(&state.store).await;
    if enabled {
        disabled.remove(&key);
    } else {
        disabled.insert(key);
    }
    let list: Vec<String> = disabled.into_iter().collect();
    save_json_setting(&state.store, "disabled_connectors", &list).await?;
    clear_idle_agents(&state).await;
    Ok(())
}

/// Set the approval mode ("allow" | "ask" | "deny") for a single tool. Enforced
/// live on the next tool call — no session rebuild needed.
#[tauri::command]
pub(super) async fn set_tool_approval(
    state: State<'_, AppState>,
    tool: String,
    mode: String,
) -> Result<(), String> {
    let mut approvals = load_tool_approvals(&state.store).await;
    // Store only overrides; "allow" is the default, so drop it to stay compact.
    if ApprovalMode::parse(&mode) == ApprovalMode::Allow {
        approvals.remove(&tool);
    } else {
        approvals.insert(tool, ApprovalMode::parse(&mode).as_str().into());
    }
    save_json_setting(&state.store, "tool_approvals", &approvals).await?;
    refresh_approval_policy(&state).await;
    Ok(())
}

/// Set the global approval scope ("full" | "auto" | "ask"). Enforced live on
/// the next tool call — no session rebuild needed.
#[tauri::command]
pub(super) async fn set_approval_scope(
    state: State<'_, AppState>,
    scope: String,
) -> Result<(), String> {
    // Normalize through `Scope` so only the three valid values ever persist.
    save_json_setting(
        &state.store,
        "approval_scope",
        &Scope::parse(&scope).as_str(),
    )
    .await?;
    refresh_approval_policy(&state).await;
    Ok(())
}

/// Toggle "Skip approvals" for a connector (force-allow all its tools).
#[tauri::command]
pub(super) async fn set_connector_skip_approvals(
    state: State<'_, AppState>,
    key: String,
    enabled: bool,
) -> Result<(), String> {
    let mut skip = load_skip_connectors(&state.store).await;
    if enabled {
        skip.insert(key);
    } else {
        skip.remove(&key);
    }
    let list: Vec<String> = skip.into_iter().collect();
    save_json_setting(&state.store, "skip_approval_connectors", &list).await?;
    refresh_approval_policy(&state).await;
    Ok(())
}

#[tauri::command]
pub(super) async fn test_mcp_connection(
    _state: State<'_, AppState>,
    conn: McpConnection,
) -> Result<Vec<wisp_mcp::RemoteTool>, String> {
    let client = connect_mcp(&conn).await.map_err(|e| format!("{e}"))?;
    let tools = client.tools_list().await.map_err(|e| format!("{e}"))?;
    Ok(tools)
}

/// Start Notion's OAuth authorization-code flow. The loopback listener is
/// bound before the authorization URL is opened so the callback is safe on
/// Windows, macOS, and Linux without a cloud redirect service.
#[tauri::command]
pub(super) async fn connect_notion(app: tauri::AppHandle) -> Result<(), String> {
    let (listener, pending) = crate::notion::begin_authorization()
        .await
        .map_err(|error| error.to_string())?;
    let authorization_url = pending.authorization_url().to_string();
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(&authorization_url, None::<&str>)
            .map_err(|error| format!("open Notion authorization page: {error}"))?;
    }
    let app_after_callback = app.clone();
    tokio::spawn(async move {
        let result = crate::notion::finish_authorization(listener, pending, "notion").await;
        let payload = match result {
            Ok(()) => {
                let state = app_after_callback.state::<AppState>();
                let mut connections = load_mcp_connections(&state.store).await;
                let connection = McpConnection {
                    id: "notion".into(),
                    name: "Notion".into(),
                    enabled: true,
                    transport: McpTransport::Notion,
                };
                if let Some(slot) = connections
                    .iter_mut()
                    .find(|connection| connection.id == "notion")
                {
                    *slot = connection;
                } else {
                    connections.push(connection);
                }
                match save_mcp_connections(&state.store, &connections).await {
                    Ok(()) => {
                        clear_idle_agents(&state).await;
                        serde_json::json!({ "ok": true, "message": "Notion connected." })
                    }
                    Err(error) => serde_json::json!({ "ok": false, "message": error }),
                }
            }
            Err(error) => serde_json::json!({ "ok": false, "message": error.to_string() }),
        };
        let _ = app_after_callback.emit("notion-auth-result", payload);
    });
    Ok(())
}
