//! Plaintext-token `request_approval`/`request_choice`, the same shape as
//! Discord/Matrix's fallback approval flow: post a prompt embedding a
//! short token, then wait for an operator reply of the form
//! `<token> approve|deny|always` (or `<token> <choice>` for
//! `request_choice`).
//!
//! This is the only viable approval surface here: there is no inbound
//! webhook to receive Adaptive Card button-click ("invoke") activities, so
//! interactive buttons are not an option.
//!
//! **Concurrency note**: `ComponentChannel` holds one mutex around the
//! whole `(Store, plugin)` pair, so while this function blocks waiting for
//! a reply, no other host call -- including `poll_message` for a
//! *different* configured team/channel on this same plugin instance --
//! can run. A long-outstanding approval therefore pauses inbound delivery
//! for every channel this instance manages, not just the one being asked.

use std::time::Duration;

use zeroclaw_plugin_sdk::channel::{ApprovalRequest, ApprovalResponse};

use crate::credentials;
use crate::graph;
use crate::graph::split_recipient;
use crate::mentions;

const DEFAULT_TIMEOUT_SECS: u64 = 300;
const POLL_INTERVAL_MS: u64 = 1_500;
const TOKEN_LEN: usize = 6;
const TOKEN_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

static TOKEN_COUNTER: std::sync::Mutex<u64> = std::sync::Mutex::new(0);

/// Not cryptographically secure -- doesn't need to be. At most one approval
/// is ever outstanding per plugin instance (the host's per-instance mutex
/// guarantees that), so the token only needs to disambiguate this prompt
/// from stray unrelated text in the channel, the same trust model Discord's
/// plaintext fallback uses.
fn generate_token() -> String {
    let mut counter = TOKEN_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    *counter = counter.wrapping_add(1);
    let seed = graph::now_ms().wrapping_mul(2_654_435_761).wrapping_add(*counter);
    encode_token(seed)
}

fn encode_token(mut seed: u64) -> String {
    let base = TOKEN_ALPHABET.len() as u64;
    let mut chars = Vec::with_capacity(TOKEN_LEN);
    for _ in 0..TOKEN_LEN {
        let idx = (seed % base) as usize;
        chars.push(TOKEN_ALPHABET[idx] as char);
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    }
    chars.into_iter().collect()
}

/// Strips HTML, then checks whether the first word case-insensitively
/// matches `token` and the second word is one of approve/deny/always.
fn parse_token_reply(body_content: &str, token: &str) -> Option<ApprovalResponse> {
    let text = mentions::plain_text(body_content);
    let mut words = text.split_whitespace();
    let first = words.next()?;
    if !first.eq_ignore_ascii_case(token) {
        return None;
    }
    match words.next()?.to_ascii_lowercase().as_str() {
        "approve" => Some(ApprovalResponse::Approve),
        "deny" => Some(ApprovalResponse::Deny),
        "always" => Some(ApprovalResponse::AlwaysApprove),
        _ => None,
    }
}

/// Same grammar as [`parse_token_reply`] but for `request_choice`: the word
/// after the token is matched against `choices` either as a 1-based index
/// or as case-insensitive text.
fn parse_token_choice(body_content: &str, token: &str, choices: &[String]) -> Option<String> {
    let text = mentions::plain_text(body_content);
    let mut words = text.split_whitespace();
    let first = words.next()?;
    if !first.eq_ignore_ascii_case(token) {
        return None;
    }
    let rest = words.collect::<Vec<_>>().join(" ");
    if let Ok(index) = rest.trim().parse::<usize>() {
        if index >= 1 && index <= choices.len() {
            return Some(choices[index - 1].clone());
        }
    }
    choices
        .iter()
        .find(|c| c.eq_ignore_ascii_case(rest.trim()))
        .cloned()
}

pub fn request_approval(
    recipient: &str,
    request: &ApprovalRequest,
) -> Result<Option<ApprovalResponse>, String> {
    let creds = credentials::load()?;
    let (team_id, channel_id) = split_recipient(recipient)?;
    let token = generate_token();

    let prompt = format!(
        "APPROVAL REQUIRED [{token}]\nTool: {}\nArgs: {}\n\nReply `{token} approve`, `{token} deny`, or `{token} always`.",
        request.tool_name, request.arguments_summary
    );

    let send_token = graph::get_token(creds)?;
    let sent = graph::send_message(&send_token, team_id, channel_id, &prompt)?;
    let mut since = sent.created_date_time;

    let deadline = graph::now_ms() + DEFAULT_TIMEOUT_SECS * 1_000;
    loop {
        if graph::now_ms() >= deadline {
            return Ok(Some(ApprovalResponse::Deny));
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        let poll_token = graph::get_token(creds)?;
        let mut messages = graph::list_channel_messages(&poll_token, team_id, channel_id)?;
        messages.sort_by(|a, b| a.created_date_time.cmp(&b.created_date_time));

        for msg in &messages {
            if msg.created_date_time.as_str() <= since.as_str() {
                continue;
            }
            if msg.sender_id() == Some(creds.bot_app_id.as_str()) {
                continue;
            }
            if let Some(resp) = parse_token_reply(&msg.body.content, &token) {
                return Ok(Some(resp));
            }
        }
        if let Some(last) = messages.last() {
            since = last.created_date_time.clone();
        }
    }
}

pub fn request_choice(
    recipient: &str,
    question: &str,
    choices: &[String],
    timeout_secs: u64,
) -> Result<Option<String>, String> {
    let creds = credentials::load()?;
    let (team_id, channel_id) = split_recipient(recipient)?;
    let token = generate_token();

    let options = choices
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = format!(
        "QUESTION [{token}]\n{question}\n{options}\n\nReply `{token} <number or choice text>`."
    );

    let send_token = graph::get_token(creds)?;
    let sent = graph::send_message(&send_token, team_id, channel_id, &prompt)?;
    let mut since = sent.created_date_time;

    let deadline = graph::now_ms() + timeout_secs.max(1) * 1_000;
    loop {
        if graph::now_ms() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

        let poll_token = graph::get_token(creds)?;
        let mut messages = graph::list_channel_messages(&poll_token, team_id, channel_id)?;
        messages.sort_by(|a, b| a.created_date_time.cmp(&b.created_date_time));

        for msg in &messages {
            if msg.created_date_time.as_str() <= since.as_str() {
                continue;
            }
            if msg.sender_id() == Some(creds.bot_app_id.as_str()) {
                continue;
            }
            if let Some(choice) = parse_token_choice(&msg.body.content, &token, choices) {
                return Ok(Some(choice));
            }
        }
        if let Some(last) = messages.last() {
            since = last.created_date_time.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_recipient_parses_team_and_channel() {
        let (team, channel) = split_recipient("team-1/channel-1").unwrap();
        assert_eq!(team, "team-1");
        assert_eq!(channel, "channel-1");
    }

    #[test]
    fn split_recipient_rejects_missing_separator() {
        assert!(split_recipient("just-a-team").is_err());
    }

    #[test]
    fn parse_token_reply_matches_case_insensitively() {
        assert!(matches!(
            parse_token_reply("abc123 Approve", "ABC123"),
            Some(ApprovalResponse::Approve)
        ));
        assert!(matches!(
            parse_token_reply("ABC123 deny", "abc123"),
            Some(ApprovalResponse::Deny)
        ));
        assert!(matches!(
            parse_token_reply("abc123 ALWAYS", "abc123"),
            Some(ApprovalResponse::AlwaysApprove)
        ));
    }

    #[test]
    fn parse_token_reply_rejects_wrong_token() {
        assert!(parse_token_reply("xyz999 approve", "abc123").is_none());
    }

    #[test]
    fn parse_token_reply_rejects_unknown_verb() {
        assert!(parse_token_reply("abc123 maybe", "abc123").is_none());
    }

    #[test]
    fn parse_token_reply_strips_html() {
        assert!(matches!(
            parse_token_reply("<p>abc123 approve</p>", "abc123"),
            Some(ApprovalResponse::Approve)
        ));
    }

    #[test]
    fn parse_token_choice_matches_by_index() {
        let choices = vec!["Yes".to_string(), "No".to_string()];
        assert_eq!(
            parse_token_choice("abc123 2", "abc123", &choices),
            Some("No".to_string())
        );
    }

    #[test]
    fn parse_token_choice_matches_by_text_case_insensitively() {
        let choices = vec!["Yes".to_string(), "No".to_string()];
        assert_eq!(
            parse_token_choice("abc123 yes", "abc123", &choices),
            Some("Yes".to_string())
        );
    }

    #[test]
    fn parse_token_choice_rejects_out_of_range_index() {
        let choices = vec!["Yes".to_string(), "No".to_string()];
        assert_eq!(parse_token_choice("abc123 5", "abc123", &choices), None);
    }

    #[test]
    fn generated_tokens_have_expected_length_and_alphabet() {
        let token = generate_token();
        assert_eq!(token.len(), TOKEN_LEN);
        assert!(token.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn consecutive_tokens_differ() {
        let a = generate_token();
        let b = generate_token();
        assert_ne!(a, b);
    }
}
