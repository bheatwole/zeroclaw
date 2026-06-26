//! Durable poll state, persisted in the read-write preopened directory
//! declared by the second `dir` `fine_grained_permission` in
//! `manifest.toml`. Surviving restarts is what makes the watermark-based
//! dedup in `poll_gate.rs` safe: without this, every plugin restart would
//! re-surface the most recent page of messages per channel.
//!
//! `delta_link` is reserved for a future upgrade to Microsoft Graph's
//! `@odata.deltaLink` mechanism (server-side "what's new" semantics, fewer
//! redundant fetches) once that's verified to work reliably for channel
//! messages under *application* (not delegated) permissions against a real
//! tenant -- unverified today, so v1 relies solely on the
//! `last_seen`/`seen_ids` watermark. The field is wired through end-to-end
//! now so that upgrade is a small change confined to `graph.rs`/`poll_gate.rs`,
//! not a state-format migration.

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

const STATE_PATH: &str = "/state/teams_state.json";

/// Bound on how many recently-seen message ids we keep per channel, to
/// tie-break messages that share an identical `createdDateTime` (Graph
/// timestamps are second-granularity, ties happen) without growing the
/// state file unboundedly.
const SEEN_IDS_RING_SIZE: usize = 200;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelState {
    /// Most recent `createdDateTime` (ISO-8601, lexically sortable) seen so
    /// far for this channel.
    pub last_seen: Option<String>,
    /// Message ids seen at `last_seen`'s timestamp, to disambiguate ties.
    #[serde(default)]
    pub seen_ids: VecDeque<String>,
    /// Reserved for a future Graph delta-link upgrade; unused in v1.
    #[serde(default)]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    /// Keyed by `"{team_id}/{channel_id}"`.
    #[serde(default)]
    pub channels: HashMap<String, ChannelState>,
}

pub fn load() -> State {
    std::fs::read_to_string(STATE_PATH)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(state: &State) -> Result<(), String> {
    let raw = serde_json::to_string(state).map_err(|e| format!("serialize state: {e}"))?;
    std::fs::write(STATE_PATH, raw).map_err(|e| format!("write {STATE_PATH}: {e}"))
}

/// Whether a message with the given `(created_date_time, id)` is new
/// relative to this channel's watermark.
pub fn is_new(state: &ChannelState, created_date_time: &str, id: &str) -> bool {
    match &state.last_seen {
        None => true,
        Some(last_seen) => match created_date_time.cmp(last_seen.as_str()) {
            std::cmp::Ordering::Greater => true,
            std::cmp::Ordering::Equal => !state.seen_ids.contains(&id.to_string()),
            std::cmp::Ordering::Less => false,
        },
    }
}

/// Record a message as seen, advancing the watermark and maintaining the
/// bounded tie-break ring. Call only for messages that were already
/// confirmed `is_new`.
pub fn record_seen(state: &mut ChannelState, created_date_time: &str, id: &str) {
    let advanced = match &state.last_seen {
        None => true,
        Some(last_seen) => created_date_time > last_seen.as_str(),
    };
    if advanced {
        state.last_seen = Some(created_date_time.to_string());
        state.seen_ids.clear();
    }
    state.seen_ids.push_back(id.to_string());
    while state.seen_ids.len() > SEEN_IDS_RING_SIZE {
        state.seen_ids.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_message_ever_is_new() {
        let state = ChannelState::default();
        assert!(is_new(&state, "2024-01-01T00:00:00Z", "m1"));
    }

    #[test]
    fn message_strictly_newer_than_watermark_is_new() {
        let state = ChannelState {
            last_seen: Some("2024-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        assert!(is_new(&state, "2024-01-01T00:00:01Z", "m2"));
    }

    #[test]
    fn message_older_than_watermark_is_not_new() {
        let state = ChannelState {
            last_seen: Some("2024-01-01T00:00:01Z".to_string()),
            ..Default::default()
        };
        assert!(!is_new(&state, "2024-01-01T00:00:00Z", "m1"));
    }

    #[test]
    fn tie_at_watermark_dedups_by_seen_ids() {
        let mut seen_ids = VecDeque::new();
        seen_ids.push_back("m1".to_string());
        let state = ChannelState {
            last_seen: Some("2024-01-01T00:00:00Z".to_string()),
            seen_ids,
            delta_link: None,
        };
        assert!(!is_new(&state, "2024-01-01T00:00:00Z", "m1"));
        assert!(is_new(&state, "2024-01-01T00:00:00Z", "m2"));
    }

    #[test]
    fn record_seen_advances_watermark_and_resets_ring() {
        let mut state = ChannelState {
            last_seen: Some("2024-01-01T00:00:00Z".to_string()),
            seen_ids: VecDeque::from(vec!["m1".to_string()]),
            delta_link: None,
        };
        record_seen(&mut state, "2024-01-01T00:00:05Z", "m2");
        assert_eq!(state.last_seen.as_deref(), Some("2024-01-01T00:00:05Z"));
        assert_eq!(state.seen_ids.len(), 1);
        assert_eq!(state.seen_ids[0], "m2");
    }

    #[test]
    fn record_seen_at_same_timestamp_appends_to_ring() {
        let mut state = ChannelState {
            last_seen: Some("2024-01-01T00:00:00Z".to_string()),
            seen_ids: VecDeque::from(vec!["m1".to_string()]),
            delta_link: None,
        };
        record_seen(&mut state, "2024-01-01T00:00:00Z", "m2");
        assert_eq!(state.seen_ids.len(), 2);
    }

    #[test]
    fn record_seen_bounds_ring_size() {
        let mut state = ChannelState::default();
        for i in 0..(SEEN_IDS_RING_SIZE + 10) {
            record_seen(&mut state, "2024-01-01T00:00:00Z", &format!("m{i}"));
        }
        assert_eq!(state.seen_ids.len(), SEEN_IDS_RING_SIZE);
    }

    #[test]
    fn state_round_trips_through_json() {
        let mut state = State::default();
        let mut cs = ChannelState::default();
        record_seen(&mut cs, "2024-01-01T00:00:00Z", "m1");
        state.channels.insert("team/chan".to_string(), cs);

        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.channels.get("team/chan").unwrap().last_seen.as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
    }
}
