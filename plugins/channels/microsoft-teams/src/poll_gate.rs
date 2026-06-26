//! Drives one Graph poll cycle per [`GRAPH_POLL_INTERVAL_MS`] window,
//! independent of how often the host calls `poll_message()` (as often as
//! every 50ms). Between windows, calls return `None` immediately and
//! cheaply -- no Graph traffic.
//!
//! A single cycle can surface a backlog across several configured
//! team/channels; results queue in [`BUFFER`] and drain one-per-call so a
//! busy cycle doesn't trigger redundant Graph calls while draining.

use std::collections::VecDeque;
use std::sync::Mutex;

use zeroclaw_plugin_sdk::channel::{InboundMessage, PluginAction};

use crate::credentials::{self, TeamChannelConfig};
use crate::graph;
use crate::mentions;
use crate::state;

pub const GRAPH_POLL_INTERVAL_MS: u64 = 5_000;

struct GateState {
    next_allowed_poll_ms: u64,
    round_robin_index: usize,
}

static GATE: Mutex<GateState> = Mutex::new(GateState {
    next_allowed_poll_ms: 0,
    round_robin_index: 0,
});
static BUFFER: Mutex<VecDeque<InboundMessage>> = Mutex::new(VecDeque::new());

/// Pure predicate: is it time for another real Graph poll cycle?
pub fn should_poll(next_allowed_poll_ms: u64, now_ms: u64) -> bool {
    now_ms >= next_allowed_poll_ms
}

pub fn next_poll_time(now_ms: u64, interval_ms: u64) -> u64 {
    now_ms.saturating_add(interval_ms)
}

/// Round-robin starting index for the next cycle, for fair coverage across
/// configured channels within Graph throttling budgets.
pub fn rotate_index(current: usize, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        (current + 1) % len
    }
}

pub fn poll() -> Option<InboundMessage> {
    if let Ok(mut buffer) = BUFFER.lock() {
        if let Some(msg) = buffer.pop_front() {
            return Some(msg);
        }
    }

    let now = graph::now_ms();
    {
        let mut gate = GATE.lock().ok()?;
        if !should_poll(gate.next_allowed_poll_ms, now) {
            return None;
        }
        gate.next_allowed_poll_ms = next_poll_time(now, GRAPH_POLL_INTERVAL_MS);
    }

    run_poll_cycle()
}

fn run_poll_cycle() -> Option<InboundMessage> {
    let creds = match credentials::load() {
        Ok(c) => c,
        Err(e) => {
            zeroclaw_plugin_sdk::channel::error("poll_message", PluginAction::Fail, &e);
            return None;
        }
    };
    if creds.teams.is_empty() {
        return None;
    }

    let token = match graph::get_token(creds) {
        Ok(t) => t,
        Err(e) => {
            zeroclaw_plugin_sdk::channel::error("poll_message", PluginAction::Fail, &e);
            return None;
        }
    };

    let mut state = state::load();
    let n = creds.teams.len();
    let start_index = {
        let mut gate = match GATE.lock() {
            Ok(g) => g,
            Err(_) => return None,
        };
        let idx = gate.round_robin_index % n;
        gate.round_robin_index = rotate_index(idx, n);
        idx
    };

    let mut new_messages: Vec<InboundMessage> = Vec::new();
    for offset in 0..n {
        let team_cfg = &creds.teams[(start_index + offset) % n];
        let key = format!("{}/{}", team_cfg.team_id, team_cfg.channel_id);
        let channel_state = state.channels.entry(key).or_default();

        let mut messages =
            match graph::list_channel_messages(&token, &team_cfg.team_id, &team_cfg.channel_id) {
                Ok(m) => m,
                Err(e) => {
                    zeroclaw_plugin_sdk::channel::error("poll_message", PluginAction::Fail, &e);
                    continue;
                }
            };

        // Graph returns newest-first by default; process oldest-first so
        // the watermark advances monotonically and messages surface in
        // chronological order.
        messages.sort_by(|a, b| a.created_date_time.cmp(&b.created_date_time));

        for msg in messages {
            if !state::is_new(channel_state, &msg.created_date_time, &msg.id) {
                continue;
            }
            state::record_seen(channel_state, &msg.created_date_time, &msg.id);

            if msg.sender_id() == Some(creds.bot_app_id.as_str()) {
                continue;
            }
            if !mentions::message_mentions_bot(&msg, creds) {
                continue;
            }

            new_messages.push(to_inbound_message(&msg, team_cfg));
        }
    }

    if let Err(e) = state::save(&state) {
        zeroclaw_plugin_sdk::channel::error("poll_message", PluginAction::Fail, &e);
    }

    if new_messages.is_empty() {
        return None;
    }

    let mut buffer = BUFFER.lock().ok()?;
    for msg in new_messages {
        buffer.push_back(msg);
    }
    buffer.pop_front()
}

fn to_inbound_message(
    msg: &graph::GraphChatMessage,
    team_cfg: &TeamChannelConfig,
) -> InboundMessage {
    let reply_target = format!("{}/{}", team_cfg.team_id, team_cfg.channel_id);
    InboundMessage {
        id: msg.id.clone(),
        sender: msg.sender_id().unwrap_or_default().to_string(),
        reply_target,
        content: mentions::plain_text(&msg.body.content),
        channel: "microsoft-teams".to_string(),
        channel_alias: team_cfg.alias.clone(),
        timestamp: parse_iso8601_to_unix_ms(&msg.created_date_time).unwrap_or(0),
        thread_ts: Some(
            msg.reply_to_id
                .clone()
                .unwrap_or_else(|| msg.id.clone()),
        ),
        interruption_scope_id: msg.reply_to_id.clone(),
        attachments: vec![],
        subject: None,
    }
}

/// Parse a Graph `createdDateTime` (`"2024-01-01T00:00:00Z"`, optionally
/// with fractional seconds) into Unix milliseconds. Hand-rolled rather than
/// pulling in `chrono` -- Graph timestamps are always UTC ("Z"-suffixed),
/// so this only needs to handle that one fixed shape.
fn parse_iso8601_to_unix_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let sec_field = time_parts.next()?;
    let (sec_whole, millis): (i64, i64) = match sec_field.split_once('.') {
        Some((whole, frac)) => {
            let whole: i64 = whole.parse().ok()?;
            let frac_padded = format!("{:0<3}", &frac[..frac.len().min(3)]);
            (whole, frac_padded.parse().ok()?)
        }
        None => (sec_field.parse().ok()?, 0),
    };

    let days = days_from_civil(year, month, day);
    let unix_seconds = days * 86_400 + hour * 3600 + minute * 60 + sec_whole;
    let unix_ms = unix_seconds * 1000 + millis;
    if unix_ms < 0 {
        None
    } else {
        Some(unix_ms as u64)
    }
}

/// Howard Hinnant's `days_from_civil`: proleptic-Gregorian days since the
/// Unix epoch (1970-01-01), pure integer math, no external date crate.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_closed_before_interval_elapses() {
        assert!(!should_poll(5_000, 4_999));
    }

    #[test]
    fn gate_open_once_interval_elapses() {
        assert!(should_poll(5_000, 5_000));
        assert!(should_poll(5_000, 5_001));
    }

    #[test]
    fn next_poll_time_advances_by_interval() {
        assert_eq!(next_poll_time(1_000, 5_000), 6_000);
    }

    #[test]
    fn rotate_index_wraps_around() {
        assert_eq!(rotate_index(0, 3), 1);
        assert_eq!(rotate_index(2, 3), 0);
    }

    #[test]
    fn rotate_index_with_zero_channels_stays_zero() {
        assert_eq!(rotate_index(0, 0), 0);
    }

    #[test]
    fn parses_basic_utc_timestamp() {
        let ms = parse_iso8601_to_unix_ms("1970-01-01T00:00:00Z").unwrap();
        assert_eq!(ms, 0);
    }

    #[test]
    fn parses_timestamp_with_fractional_seconds() {
        let ms = parse_iso8601_to_unix_ms("1970-01-01T00:00:01.500Z").unwrap();
        assert_eq!(ms, 1_500);
    }

    #[test]
    fn parses_known_date() {
        // 2024-01-01T00:00:00Z is a well-known epoch-ms value.
        let ms = parse_iso8601_to_unix_ms("2024-01-01T00:00:00Z").unwrap();
        assert_eq!(ms, 1_704_067_200_000);
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(parse_iso8601_to_unix_ms("not-a-date").is_none());
    }
}
