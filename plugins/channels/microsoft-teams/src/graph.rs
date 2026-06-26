//! Microsoft Graph client: OAuth2 client-credentials token cache/refresh and
//! the small set of Teams channel-message operations this plugin needs.
//!
//! Built directly on `waki` (a synchronous, `wasi:http`-native client) since
//! `reqwest`/`oauth2` assume a tokio reactor unavailable on `wasm32-wasip2`.
//! Every outbound call still goes through the host's `PluginHttpHooks`
//! (allow-list + proxy enforcement), the same as the SDK's `http-helpers`
//! convenience functions -- this is not a parallel security boundary.

use serde::{Deserialize, Serialize};

use crate::credentials::Credentials;

const TOKEN_REFRESH_MARGIN_MS: u64 = 60_000;

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at_unix_ms: u64,
}

static TOKEN: std::sync::Mutex<Option<CachedToken>> = std::sync::Mutex::new(None);

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Pure predicate, kept free of `SystemTime::now()` so it's directly
/// unit-testable with fixed timestamps.
fn token_needs_refresh(token: &CachedToken, now_ms: u64, margin_ms: u64) -> bool {
    now_ms + margin_ms >= token.expires_at_unix_ms
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

fn fetch_token(creds: &Credentials) -> Result<CachedToken, String> {
    let url = format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        creds.tenant_id
    );
    let resp = waki::Client::new()
        .post(&url)
        .form([
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("scope", "https://graph.microsoft.com/.default"),
            ("grant_type", "client_credentials"),
        ])
        .send()
        .map_err(|e| format!("token request failed: {e}"))?;

    let status = resp.status_code();
    if status >= 400 {
        let body = resp.body().unwrap_or_default();
        return Err(format!(
            "token request returned {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }
    let parsed: TokenResponse = resp
        .json()
        .map_err(|e| format!("parse token response: {e}"))?;
    Ok(CachedToken {
        access_token: parsed.access_token,
        expires_at_unix_ms: now_ms() + parsed.expires_in.saturating_mul(1000),
    })
}

/// Return a valid access token, refreshing lazily if the cached one is
/// missing or within [`TOKEN_REFRESH_MARGIN_MS`] of expiry. No background
/// timer -- refresh is driven entirely by actual Graph calls.
pub fn get_token(creds: &Credentials) -> Result<String, String> {
    let mut guard = TOKEN.lock().map_err(|e| e.to_string())?;
    let now = now_ms();
    if let Some(tok) = guard.as_ref() {
        if !token_needs_refresh(tok, now, TOKEN_REFRESH_MARGIN_MS) {
            return Ok(tok.access_token.clone());
        }
    }
    let fresh = fetch_token(creds)?;
    let access_token = fresh.access_token.clone();
    *guard = Some(fresh);
    Ok(access_token)
}

// ── Graph data types ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct GraphIdentity {
    pub id: String,
    /// Unused by current matching logic (which keys on `id`), kept for
    /// diagnostics/logging call sites that want a human-readable name.
    #[serde(rename = "displayName", default)]
    #[allow(dead_code)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphFrom {
    #[serde(default)]
    pub user: Option<GraphIdentity>,
    #[serde(default)]
    pub application: Option<GraphIdentity>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphBody {
    /// Unused today: `mentions::plain_text`'s HTML-stripping pass is a
    /// harmless no-op on plain text, so we don't need to branch on this to
    /// handle both `"html"` and `"text"` content types correctly. Kept for
    /// future use (e.g. choosing a different rendering for outbound replies).
    #[serde(rename = "contentType", default)]
    #[allow(dead_code)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GraphMentionedIdentitySet {
    /// Unused: mention matching only cares about `application` (the bot's
    /// own mention chip). Kept so the struct mirrors Graph's full
    /// `chatMessageMentionedIdentitySet` schema for future user-mention use.
    #[serde(default)]
    #[allow(dead_code)]
    pub user: Option<GraphIdentity>,
    #[serde(default)]
    pub application: Option<GraphIdentity>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphMention {
    /// Unused by matching logic (which checks `mentioned.application.id`),
    /// kept for diagnostics/logging.
    #[serde(rename = "mentionText", default)]
    #[allow(dead_code)]
    pub mention_text: Option<String>,
    #[serde(default)]
    pub mentioned: GraphMentionedIdentitySet,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GraphChatMessage {
    pub id: String,
    #[serde(rename = "createdDateTime")]
    pub created_date_time: String,
    #[serde(default)]
    pub from: GraphFrom,
    pub body: GraphBody,
    #[serde(default)]
    pub mentions: Vec<GraphMention>,
    #[serde(rename = "replyToId", default)]
    pub reply_to_id: Option<String>,
}

impl GraphChatMessage {
    /// AAD object id of whoever sent this message: the app id if sent by a
    /// bot/app identity, otherwise the user id.
    pub fn sender_id(&self) -> Option<&str> {
        self.from
            .application
            .as_ref()
            .map(|a| a.id.as_str())
            .or_else(|| self.from.user.as_ref().map(|u| u.id.as_str()))
    }
}

#[derive(Debug, Deserialize)]
struct MessagesPage {
    #[serde(default)]
    value: Vec<GraphChatMessage>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorBody {
    error: GraphErrorDetail,
}

#[derive(Debug, Deserialize)]
struct GraphErrorDetail {
    message: String,
}

fn handle_response<T: serde::de::DeserializeOwned>(resp: waki::Response) -> Result<T, String> {
    let status = resp.status_code();
    if status >= 400 {
        let body = resp.body().unwrap_or_default();
        let text = String::from_utf8_lossy(&body).to_string();
        let message = serde_json::from_str::<GraphErrorBody>(&text)
            .map(|e| e.error.message)
            .unwrap_or(text);
        return Err(format!("graph api returned {status}: {message}"));
    }
    resp.json::<T>()
        .map_err(|e| format!("parse graph response: {e}"))
}

/// Teams channel messages are scoped to a specific channel within a
/// specific team, so [`crate::credentials::TeamChannelConfig`]'s
/// `"{team_id}/{channel_id}"` pair is encoded into `SendMessage.recipient`
/// / `InboundMessage.reply_target` as a single string. This splits it back
/// apart.
pub fn split_recipient(recipient: &str) -> Result<(&str, &str), String> {
    recipient.split_once('/').ok_or_else(|| {
        format!("invalid recipient (expected \"team_id/channel_id\"): {recipient}")
    })
}

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

pub fn list_channel_messages(
    token: &str,
    team_id: &str,
    channel_id: &str,
) -> Result<Vec<GraphChatMessage>, String> {
    let url = format!("{GRAPH_BASE}/teams/{team_id}/channels/{channel_id}/messages");
    let resp = waki::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .query([("$top", "50")])
        .send()
        .map_err(|e| format!("list channel messages failed: {e}"))?;
    handle_response::<MessagesPage>(resp).map(|p| p.value)
}

/// Not called from `poll_gate` yet -- v1 polls root channel messages only
/// and defers thread-reply ingestion as a stretch goal (per the
/// implementation plan). Reserved for that follow-up.
#[allow(dead_code)]
pub fn list_replies(
    token: &str,
    team_id: &str,
    channel_id: &str,
    message_id: &str,
) -> Result<Vec<GraphChatMessage>, String> {
    let url =
        format!("{GRAPH_BASE}/teams/{team_id}/channels/{channel_id}/messages/{message_id}/replies");
    let resp = waki::Client::new()
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .map_err(|e| format!("list replies failed: {e}"))?;
    handle_response::<MessagesPage>(resp).map(|p| p.value)
}

#[derive(Serialize)]
struct SendBody<'a> {
    body: SendBodyContent<'a>,
}

#[derive(Serialize)]
struct SendBodyContent<'a> {
    content: &'a str,
}

pub fn send_message(
    token: &str,
    team_id: &str,
    channel_id: &str,
    content: &str,
) -> Result<GraphChatMessage, String> {
    let url = format!("{GRAPH_BASE}/teams/{team_id}/channels/{channel_id}/messages");
    let resp = waki::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&SendBody {
            body: SendBodyContent { content },
        })
        .send()
        .map_err(|e| format!("send message failed: {e}"))?;
    handle_response(resp)
}

pub fn send_reply(
    token: &str,
    team_id: &str,
    channel_id: &str,
    root_message_id: &str,
    content: &str,
) -> Result<GraphChatMessage, String> {
    let url = format!(
        "{GRAPH_BASE}/teams/{team_id}/channels/{channel_id}/messages/{root_message_id}/replies"
    );
    let resp = waki::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&SendBody {
            body: SendBodyContent { content },
        })
        .send()
        .map_err(|e| format!("send reply failed: {e}"))?;
    handle_response(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_token_does_not_need_refresh() {
        let tok = CachedToken {
            access_token: "abc".to_string(),
            expires_at_unix_ms: 1_000_000,
        };
        assert!(!token_needs_refresh(&tok, 0, TOKEN_REFRESH_MARGIN_MS));
    }

    #[test]
    fn token_within_margin_of_expiry_needs_refresh() {
        let tok = CachedToken {
            access_token: "abc".to_string(),
            expires_at_unix_ms: 100_000,
        };
        // now + margin >= expiry
        assert!(token_needs_refresh(&tok, 100_000 - 60_000 + 1, 60_000));
    }

    #[test]
    fn already_expired_token_needs_refresh() {
        let tok = CachedToken {
            access_token: "abc".to_string(),
            expires_at_unix_ms: 100,
        };
        assert!(token_needs_refresh(&tok, 200, 0));
    }

    #[test]
    fn chat_message_deserializes_with_application_mention() {
        let raw = r#"{
            "id": "163...",
            "createdDateTime": "2024-01-01T00:00:00Z",
            "from": {
                "user": { "id": "user-1", "displayName": "Alice" }
            },
            "body": { "contentType": "html", "content": "<at id=\"0\">ZeroClaw</at> hello" },
            "mentions": [
                {
                    "mentionText": "ZeroClaw",
                    "mentioned": {
                        "application": { "id": "bot-app-id", "displayName": "ZeroClaw" }
                    }
                }
            ],
            "replyToId": null
        }"#;
        let msg: GraphChatMessage = serde_json::from_str(raw).expect("must parse");
        assert_eq!(msg.sender_id(), Some("user-1"));
        assert_eq!(msg.mentions.len(), 1);
        assert_eq!(
            msg.mentions[0].mentioned.application.as_ref().unwrap().id,
            "bot-app-id"
        );
    }

    #[test]
    fn chat_message_sender_id_prefers_application_over_user() {
        let raw = r#"{
            "id": "1",
            "createdDateTime": "2024-01-01T00:00:00Z",
            "from": {
                "user": { "id": "user-1" },
                "application": { "id": "bot-app-id" }
            },
            "body": { "content": "hi" }
        }"#;
        let msg: GraphChatMessage = serde_json::from_str(raw).expect("must parse");
        assert_eq!(msg.sender_id(), Some("bot-app-id"));
    }
}
