//! Microsoft Teams channel plugin.
//!
//! Inbound delivery is entirely Graph-polling based (see `poll_gate`):
//! channel plugins cannot host an inbound HTTPS listener, so the classic
//! Bot Framework webhook model isn't usable here. A bot/Teams-app
//! registration is still recommended (see `teams-app-manifest/SETUP.md`)
//! purely so the bot appears in the native @-mention picker -- its
//! messaging endpoint is never used by this plugin.
//!
//! See `approval.rs` for an important concurrency caveat: `request_approval`
//! and `request_choice` block inside a single exported call, which (because
//! of the host's per-instance mutex) also pauses `poll_message` for every
//! team/channel this instance manages, not just the one being asked.

mod approval;
mod credentials;
mod graph;
mod mentions;
mod poll_gate;
mod state;

use zeroclaw_plugin_sdk::channel::{
    ApprovalRequest, ApprovalResponse, ChannelCapabilities, ChannelPlugin, InboundMessage,
    SendMessage,
};

struct TeamsChannel;

impl ChannelPlugin for TeamsChannel {
    fn plugin_info() -> (&'static str, &'static str) {
        ("microsoft-teams-channel", "0.1.0")
    }

    fn name() -> String {
        "microsoft-teams".to_string()
    }

    fn get_channel_capabilities() -> ChannelCapabilities {
        ChannelCapabilities::HEALTH_CHECK
            | ChannelCapabilities::SELF_HANDLE
            | ChannelCapabilities::SELF_ADDRESSED_MENTION
            | ChannelCapabilities::DROP_SELF_MESSAGE
            | ChannelCapabilities::REQUEST_APPROVAL
            | ChannelCapabilities::REQUEST_CHOICE
    }

    fn send(message: SendMessage) -> Result<(), String> {
        let creds = credentials::load()?;
        let (team_id, channel_id) = graph::split_recipient(&message.recipient)?;
        let token = graph::get_token(creds)?;

        match message.thread_ts {
            Some(root_message_id) => {
                graph::send_reply(&token, team_id, channel_id, &root_message_id, &message.content)
                    .map(|_| ())
            }
            None => graph::send_message(&token, team_id, channel_id, &message.content).map(|_| ()),
        }
    }

    fn poll_message() -> Option<InboundMessage> {
        poll_gate::poll()
    }

    fn health_check() -> bool {
        match credentials::load() {
            Ok(creds) => graph::get_token(creds).is_ok(),
            Err(_) => false,
        }
    }

    fn self_handle() -> Option<String> {
        credentials::load().ok().map(|c| c.bot_app_id.clone())
    }

    fn self_addressed_mention() -> Option<String> {
        credentials::load()
            .ok()
            .map(|c| format!("@{}", c.bot_display_name))
    }

    fn drop_self_message(msg: InboundMessage) -> bool {
        let Some(handle) = Self::self_handle() else {
            return false;
        };
        let handle_norm = handle.trim_start_matches('@').to_ascii_lowercase();
        let sender_norm = msg.sender.trim_start_matches('@').to_ascii_lowercase();
        !handle_norm.is_empty() && handle_norm == sender_norm
    }

    fn request_approval(
        recipient: String,
        request: ApprovalRequest,
    ) -> Result<Option<ApprovalResponse>, String> {
        approval::request_approval(&recipient, &request)
    }

    fn request_choice(
        question: String,
        choices: Vec<String>,
        timeout_secs: u64,
    ) -> Result<Option<String>, String> {
        // The SDK's `request_choice` signature has no `recipient` parameter
        // (it's meant to be answered in the conversation that's already
        // active), but our Graph-polling implementation needs to know
        // which team/channel to post the prompt to and watch for a reply.
        // Single-team/single-channel deployments are unambiguous; for
        // multi-channel deployments this targets the first configured
        // team/channel, which the operator should account for in setup.
        let creds = credentials::load()?;
        let Some(first) = creds.teams.first() else {
            return Err("no teams configured in credentials.toml".to_string());
        };
        let recipient = format!("{}/{}", first.team_id, first.channel_id);
        approval::request_choice(&recipient, &question, &choices, timeout_secs)
    }
}

zeroclaw_plugin_sdk::export_channel!(TeamsChannel);
