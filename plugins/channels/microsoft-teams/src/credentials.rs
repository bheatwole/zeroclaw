//! Reads operator-supplied Azure AD credentials and the configured
//! team/channel list from the read-only preopened directory declared by the
//! `dir` `fine_grained_permission` in `manifest.toml`. There is no
//! `get-secret` WIT mechanism yet (deliberately deferred upstream), so this
//! is the only way a channel plugin can receive a secret today.

use std::sync::OnceLock;

use serde::Deserialize;

/// Fixed guest-side path; must match the `guest_path` of the read-only
/// `dir` permission in `manifest.toml`.
const CREDENTIALS_PATH: &str = "/secrets/teams_credentials.toml";

#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub bot_app_id: String,
    pub bot_display_name: String,
    #[serde(default)]
    pub teams: Vec<TeamChannelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamChannelConfig {
    pub team_id: String,
    pub channel_id: String,
    pub alias: Option<String>,
}

static CREDENTIALS: OnceLock<Result<Credentials, String>> = OnceLock::new();

/// Load (and cache) credentials from [`CREDENTIALS_PATH`]. Read once per
/// plugin instance lifetime -- the file doesn't change while the plugin is
/// running, and re-reading on every call would be wasted filesystem I/O.
pub fn load() -> Result<&'static Credentials, String> {
    CREDENTIALS
        .get_or_init(|| {
            let raw = std::fs::read_to_string(CREDENTIALS_PATH)
                .map_err(|e| format!("failed to read {CREDENTIALS_PATH}: {e}"))?;
            toml::from_str::<Credentials>(&raw)
                .map_err(|e| format!("failed to parse {CREDENTIALS_PATH}: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE_TOML: &str = include_str!("../credentials.toml.example");

    #[test]
    fn example_file_parses() {
        let creds: Credentials = toml::from_str(EXAMPLE_TOML).expect("must parse");
        assert_eq!(creds.tenant_id, "00000000-0000-0000-0000-000000000000");
        assert_eq!(creds.bot_display_name, "ZeroClaw");
        assert_eq!(creds.teams.len(), 2);
        assert_eq!(creds.teams[0].alias.as_deref(), Some("engineering"));
        assert_eq!(creds.teams[1].alias.as_deref(), Some("support"));
    }

    #[test]
    fn teams_list_defaults_to_empty() {
        let toml_str = r#"
            tenant_id = "t"
            client_id = "c"
            client_secret = "s"
            bot_app_id = "b"
            bot_display_name = "Bot"
        "#;
        let creds: Credentials = toml::from_str(toml_str).expect("must parse");
        assert!(creds.teams.is_empty());
    }
}
