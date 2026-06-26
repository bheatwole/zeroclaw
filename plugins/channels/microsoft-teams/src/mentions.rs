//! Mention detection, in priority order:
//! 1. Real @-mention chip: `mentions[].mentioned.application.id == bot_app_id`.
//!    Only populated when the operator completed the Teams app/Bot Framework
//!    registration in `teams-app-manifest/SETUP.md`.
//! 2. Plain-text `@{bot_display_name}` fallback, for messages typed without
//!    using the mention picker.
//!
//! Either match routes the message to the agent; neither matching drops it
//! (channels default to mention-gated, matching Slack/Discord's typical
//! bot-mention behavior).

use crate::credentials::Credentials;
use crate::graph::GraphChatMessage;

pub fn message_mentions_bot(msg: &GraphChatMessage, creds: &Credentials) -> bool {
    chip_match(msg, &creds.bot_app_id) || text_fallback_match(&msg.body.content, &creds.bot_display_name)
}

fn chip_match(msg: &GraphChatMessage, bot_app_id: &str) -> bool {
    msg.mentions.iter().any(|m| {
        m.mentioned
            .application
            .as_ref()
            .is_some_and(|app| app.id.eq_ignore_ascii_case(bot_app_id))
    })
}

fn text_fallback_match(body_content: &str, bot_display_name: &str) -> bool {
    if bot_display_name.is_empty() {
        return false;
    }
    let text = plain_text(body_content);
    let text_lower = text.to_ascii_lowercase();
    let name_lower = bot_display_name.to_ascii_lowercase();

    let at_form = format!("@{name_lower}");
    if text_lower.contains(&at_form) {
        return true;
    }
    // Whole-word match against the bare name, in case the picker stripped
    // the leading "@" but kept the rest of the chip's plain-text rendering.
    text_lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word == name_lower)
}

/// Minimal HTML-to-text pass: Graph returns `body.content` as HTML when
/// `contentType == "html"` (the common case for Teams messages). Strips
/// tags and decodes the handful of entities Teams actually emits -- not a
/// general-purpose HTML parser, just enough for trigger-text matching and
/// for the plain-text `InboundMessage.content` the agent sees.
pub fn plain_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{GraphBody, GraphChatMessage, GraphFrom, GraphIdentity, GraphMention, GraphMentionedIdentitySet};

    fn creds_with(bot_app_id: &str, bot_display_name: &str) -> Credentials {
        Credentials {
            tenant_id: "t".to_string(),
            client_id: "c".to_string(),
            client_secret: "s".to_string(),
            bot_app_id: bot_app_id.to_string(),
            bot_display_name: bot_display_name.to_string(),
            teams: vec![],
        }
    }

    fn message_with(content: &str, mentions: Vec<GraphMention>) -> GraphChatMessage {
        GraphChatMessage {
            id: "1".to_string(),
            created_date_time: "2024-01-01T00:00:00Z".to_string(),
            from: GraphFrom::default(),
            body: GraphBody {
                content_type: Some("html".to_string()),
                content: content.to_string(),
            },
            mentions,
            reply_to_id: None,
        }
    }

    #[test]
    fn chip_match_takes_priority() {
        let creds = creds_with("bot-app-id", "ZeroClaw");
        let mentions = vec![GraphMention {
            mention_text: Some("ZeroClaw".to_string()),
            mentioned: GraphMentionedIdentitySet {
                user: None,
                application: Some(GraphIdentity {
                    id: "bot-app-id".to_string(),
                    display_name: Some("ZeroClaw".to_string()),
                }),
            },
        }];
        let msg = message_with("hi <at id=\"0\">ZeroClaw</at>", mentions);
        assert!(message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn chip_for_different_app_does_not_match() {
        let creds = creds_with("bot-app-id", "ZeroClaw");
        let mentions = vec![GraphMention {
            mention_text: Some("OtherBot".to_string()),
            mentioned: GraphMentionedIdentitySet {
                user: None,
                application: Some(GraphIdentity {
                    id: "other-app-id".to_string(),
                    display_name: Some("OtherBot".to_string()),
                }),
            },
        }];
        let msg = message_with("hi <at id=\"0\">OtherBot</at>, no actual mention", mentions);
        assert!(!message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn text_fallback_matches_at_prefixed_name_case_insensitively() {
        let creds = creds_with("bot-app-id", "ZeroClaw");
        let msg = message_with("hey @zeroclaw can you help", vec![]);
        assert!(message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn text_fallback_matches_bare_whole_word_name() {
        let creds = creds_with("bot-app-id", "ZeroClaw");
        let msg = message_with("ZeroClaw please summarize this thread", vec![]);
        assert!(message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn text_fallback_does_not_match_substring_of_a_longer_word() {
        let creds = creds_with("bot-app-id", "Claw");
        let msg = message_with("ZeroClawBot please help", vec![]);
        assert!(!message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn no_match_drops_message() {
        let creds = creds_with("bot-app-id", "ZeroClaw");
        let msg = message_with("just chatting, no bot involved", vec![]);
        assert!(!message_mentions_bot(&msg, &creds));
    }

    #[test]
    fn html_tags_are_stripped_before_matching() {
        assert_eq!(plain_text("<p>hello <b>world</b></p>"), "hello world");
    }

    #[test]
    fn html_entities_are_decoded() {
        assert_eq!(plain_text("a&amp;b&nbsp;c"), "a&b c");
    }
}
