//! OAuth support for Notion's hosted MCP server.
//!
//! The Notion MCP service uses OAuth 2.0 authorization-code flow with PKCE and
//! dynamic client registration.  Connection metadata is stored as a normal
//! MCP connection, while every credential remains in `wisp_store::secrets`.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use url::Url;

pub const MCP_URL: &str = "https://mcp.notion.com/mcp";
const CALLBACK_PATH: &str = "/callback";
const AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

#[derive(Clone)]
pub struct PendingAuthorization {
    authorization_url: String,
    state: String,
    code_verifier: String,
    redirect_uri: String,
    client_id: String,
    client_secret: Option<String>,
    token_endpoint: String,
}

impl PendingAuthorization {
    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    authorization_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct ClientRegistration {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Credential {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
    token_endpoint: String,
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn secret_name(connection_id: &str) -> String {
    format!("notion_oauth:{connection_id}")
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("build Notion OAuth HTTP client")
}

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut value = vec![0_u8; bytes];
    SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| anyhow!("generate secure random OAuth value"))?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

fn code_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn callback_url(listener: &TcpListener) -> Result<String> {
    let address = listener
        .local_addr()
        .context("read Notion callback address")?;
    Ok(format!(
        "http://127.0.0.1:{}{CALLBACK_PATH}",
        address.port()
    ))
}

async fn json_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    operation: &str,
) -> Result<T> {
    let status = response.status();
    let text = response
        .text()
        .await
        .context("read Notion OAuth response")?;
    if !status.is_success() {
        return Err(anyhow!(
            "{operation} failed with {status}: {}",
            text.chars().take(300).collect::<String>()
        ));
    }
    serde_json::from_str(&text).with_context(|| format!("parse {operation} response"))
}

/// Bind the loopback callback listener before registering a dynamic client, so
/// the exact redirect URI is known to Notion before the user opens a browser.
pub async fn begin_authorization() -> Result<(TcpListener, PendingAuthorization)> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind local Notion OAuth callback")?;
    let redirect_uri = callback_url(&listener)?;
    let client = http_client()?;

    let protected_url = format!("{MCP_URL}/.well-known/oauth-protected-resource");
    let protected: ProtectedResourceMetadata = json_response(
        client
            .get(&protected_url)
            .send()
            .await
            .context("discover Notion protected resource")?,
        "Notion OAuth discovery",
    )
    .await?;
    let auth_server = protected
        .authorization_servers
        .first()
        .ok_or_else(|| anyhow!("Notion OAuth discovery returned no authorization server"))?;
    let metadata_url = Url::parse(auth_server)
        .context("parse Notion authorization server URL")?
        .join("/.well-known/oauth-authorization-server")
        .context("build Notion OAuth metadata URL")?;
    let metadata: AuthorizationServerMetadata = json_response(
        client
            .get(metadata_url)
            .send()
            .await
            .context("discover Notion authorization server")?,
        "Notion authorization-server discovery",
    )
    .await?;
    let registration_endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
        anyhow!("Notion OAuth server does not support dynamic client registration")
    })?;
    let registration: ClientRegistration = json_response(
        client
            .post(registration_endpoint)
            .header("accept", "application/json")
            .json(&json!({
                "client_name": "Wisp Science",
                "client_uri": "https://github.com/chewice/wisp-science-notionMCP",
                "redirect_uris": [redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            }))
            .send()
            .await
            .context("register Wisp with Notion")?,
        "Notion dynamic client registration",
    )
    .await?;

    let code_verifier = random_urlsafe(32)?;
    let state = random_urlsafe(32)?;
    let mut authorization_url = Url::parse(&metadata.authorization_endpoint)
        .context("parse Notion authorization endpoint")?;
    authorization_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &registration.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &code_challenge(&code_verifier))
        .append_pair("code_challenge_method", "S256")
        .append_pair("resource", MCP_URL)
        .append_pair("prompt", "consent");
    Ok((
        listener,
        PendingAuthorization {
            authorization_url: authorization_url.into(),
            state,
            code_verifier,
            redirect_uri,
            client_id: registration.client_id,
            client_secret: registration.client_secret,
            token_endpoint: metadata.token_endpoint,
        },
    ))
}

fn callback_parameters(request: &str) -> Result<(Option<String>, String, Option<String>)> {
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| anyhow!("invalid local OAuth callback request"))?;
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .context("parse local OAuth callback URL")?;
    if url.path() != CALLBACK_PATH {
        return Err(anyhow!("unexpected local OAuth callback path"));
    }
    let params = url
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let error = params.get("error").map(|s| s.to_string());
    let state = params
        .get("state")
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("OAuth callback is missing state"))?;
    let code = params.get("code").map(|s| s.to_string());
    Ok((code, state, error))
}

async fn reply_callback(stream: &mut TcpStream, ok: bool, message: &str) {
    let title = if ok {
        "Notion connected"
    } else {
        "Notion connection failed"
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title><h1>{title}</h1><p>{message}</p><p>You can close this tab and return to Wisp.</p>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn exchange_code(pending: &PendingAuthorization, code: &str) -> Result<Credential> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("client_id", pending.client_id.clone()),
        ("redirect_uri", pending.redirect_uri.clone()),
        ("code_verifier", pending.code_verifier.clone()),
    ];
    if let Some(secret) = &pending.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let tokens: TokenResponse = json_response(
        http_client()?
            .post(&pending.token_endpoint)
            .header("accept", "application/json")
            .form(&form)
            .send()
            .await
            .context("exchange Notion authorization code")?,
        "Notion token exchange",
    )
    .await?;
    Ok(Credential {
        client_id: pending.client_id.clone(),
        client_secret: pending.client_secret.clone(),
        token_endpoint: pending.token_endpoint.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens
            .expires_in
            .map(|seconds| Utc::now().timestamp() + seconds),
    })
}

/// Wait for the browser redirect, validate its CSRF state, then persist the
/// OAuth credential under the connection-specific keyring entry.
pub async fn finish_authorization(
    listener: TcpListener,
    pending: PendingAuthorization,
    connection_id: &str,
) -> Result<()> {
    let (mut stream, _) = tokio::time::timeout(AUTH_TIMEOUT, listener.accept())
        .await
        .map_err(|_| anyhow!("Notion authorization timed out after 10 minutes"))?
        .context("accept Notion OAuth callback")?;
    let mut request = vec![0_u8; 16 * 1024];
    let n = stream
        .read(&mut request)
        .await
        .context("read Notion OAuth callback")?;
    let result = async {
        let request = std::str::from_utf8(&request[..n]).context("decode OAuth callback")?;
        let (code, state, error) = callback_parameters(request)?;
        if state != pending.state {
            return Err(anyhow!(
                "Notion OAuth state did not match; authorization was rejected"
            ));
        }
        if let Some(error) = error {
            return Err(anyhow!("Notion authorization was declined: {error}"));
        }
        let code = code.ok_or_else(|| anyhow!("Notion OAuth callback is missing code"))?;
        let credential = exchange_code(&pending, &code).await?;
        let secret = serde_json::to_string(&credential).context("serialize Notion credential")?;
        wisp_store::secrets::Secret::set(&secret_name(connection_id), &secret)
            .context("save Notion credential in OS keyring")?;
        Ok(())
    }
    .await;
    match &result {
        Ok(()) => reply_callback(&mut stream, true, "Authorization completed successfully.").await,
        Err(error) => reply_callback(&mut stream, false, &error.to_string()).await,
    }
    result
}

async fn refresh(connection_id: &str, credential: &mut Credential) -> Result<()> {
    let refresh_token = credential
        .refresh_token
        .clone()
        .ok_or_else(|| anyhow!("Notion access token expired; reconnect Notion"))?;
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.clone()),
        ("client_id", credential.client_id.clone()),
    ];
    if let Some(secret) = &credential.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let tokens: TokenResponse = json_response(
        http_client()?
            .post(&credential.token_endpoint)
            .header("accept", "application/json")
            .form(&form)
            .send()
            .await
            .context("refresh Notion access token")?,
        "Notion token refresh",
    )
    .await?;
    credential.access_token = tokens.access_token;
    if let Some(rotated) = tokens.refresh_token {
        credential.refresh_token = Some(rotated);
    }
    credential.expires_at = tokens
        .expires_in
        .map(|seconds| Utc::now().timestamp() + seconds);
    let secret =
        serde_json::to_string(credential).context("serialize refreshed Notion credential")?;
    wisp_store::secrets::Secret::set(&secret_name(connection_id), &secret)
        .context("save refreshed Notion credential in OS keyring")?;
    Ok(())
}

/// Connect the agent to an authorized Notion workspace, refreshing expiring
/// access tokens before the MCP handshake.
pub async fn connect(connection_id: &str) -> Result<wisp_mcp::McpClient> {
    let raw = wisp_store::secrets::Secret::get(&secret_name(connection_id))
        .map_err(|_| anyhow!("Notion is not authorized; connect it from Settings → Connections"))?;
    let mut credential: Credential =
        serde_json::from_str(&raw).context("parse saved Notion credential")?;
    if credential
        .expires_at
        .is_some_and(|expires_at| expires_at <= Utc::now().timestamp() + 60)
    {
        refresh(connection_id, &mut credential).await?;
    }
    wisp_mcp::McpClient::connect_http(
        MCP_URL,
        &[(
            "Authorization".to_string(),
            format!("Bearer {}", credential.access_token),
        )],
    )
    .await
}

pub fn forget(connection_id: &str) {
    let _ = wisp_store::secrets::Secret::delete(&secret_name(connection_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        assert_eq!(
            code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn callback_parser_decodes_code_and_state() {
        let (code, state, error) = callback_parameters(
            "GET /callback?code=abc%2B123&state=expected HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        )
        .unwrap();
        assert_eq!(code.as_deref(), Some("abc+123"));
        assert_eq!(state, "expected");
        assert!(error.is_none());
    }

    #[test]
    fn callback_parser_rejects_other_paths() {
        assert!(callback_parameters("GET /wrong?state=s HTTP/1.1\r\n\r\n").is_err());
    }
}
