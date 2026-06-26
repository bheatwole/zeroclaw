//! Component round-trip test, mirroring
//! `zeroclaw-plugin-sdk/tests/spike_channel.rs`'s pattern. Exercises the
//! WIT plumbing and the credentials-missing failure path without needing a
//! live Azure tenant: no `credentials.toml` is written into the preopened
//! secrets directory, so `health_check()`/`self_handle()` must fail
//! gracefully (not panic) and the host must correctly compose the
//! capability-gated defaults for everything this plugin doesn't implement.
//!
//! Live-tenant behavior (real @-mention delivery, send/reply, approval
//! round-trip, token refresh across the `expires_in` boundary) is a manual
//! QA checklist in `teams-app-manifest/SETUP.md`, not something this
//! automated test can cover.

use std::path::Path;
use std::process::Command;

fn wasm32_wasip2_installed() -> bool {
    Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|line| line.trim() == "wasm32-wasip2")
        })
        .unwrap_or(false)
}

fn build_plugin() -> std::path::PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let status = Command::new("cargo")
        .args(["build", "--target", "wasm32-wasip2"])
        .current_dir(crate_dir)
        .status()
        .expect("failed to invoke cargo build for microsoft-teams-channel");
    assert!(status.success(), "plugin failed to build for wasm32-wasip2");

    crate_dir
        .join("target/wasm32-wasip2/debug/microsoft_teams_channel.wasm")
        .canonicalize()
        .expect("wasm artifact not found after build")
}

#[tokio::test]
async fn teams_channel_round_trips_through_plugin_host_without_credentials() {
    if !wasm32_wasip2_installed() {
        eprintln!("skipping: wasm32-wasip2 target not installed");
        return;
    }

    let wasm_path = build_plugin();

    let workdir = tempfile::tempdir().expect("tempdir");
    let plugin_dir = workdir.path().join("plugins/teams");
    let secrets_dir = workdir.path().join("secrets");
    let state_dir = workdir.path().join("state");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::create_dir_all(&secrets_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::copy(&wasm_path, plugin_dir.join("teams.wasm")).unwrap();

    // Deliberately no credentials.toml in `secrets_dir` -- exercises the
    // missing-credentials failure path without any live Graph/Azure AD call.
    std::fs::write(
        plugin_dir.join("manifest.toml"),
        format!(
            r#"
name = "microsoft-teams"
version = "0.1.0"
description = "spike: microsoft-teams channel round trip"
wasm_path = "teams.wasm"
capabilities = ["channel"]

[[fine_grained_permissions]]
type = "dir"
value = {{ host_path = "{secrets}", guest_path = "/secrets", dir_read = true, dir_write = false, file_read = true, file_write = false }}

[[fine_grained_permissions]]
type = "dir"
value = {{ host_path = "{state}", guest_path = "/state", dir_read = true, dir_write = true, file_read = true, file_write = true }}
"#,
            secrets = secrets_dir.display(),
            state = state_dir.display(),
        ),
    )
    .unwrap();

    let host = zeroclaw_plugins::host::PluginHost::new(workdir.path()).expect("PluginHost::new");
    let channel = host
        .instantiate_channel_plugin("microsoft-teams", None, None)
        .await
        .expect("instantiate_channel_plugin");

    assert_eq!(
        zeroclaw_api::channel::Channel::name(&*channel),
        "microsoft-teams"
    );

    // Capability-gated, missing credentials -> these must fail gracefully
    // (None / false), not panic or hang the test.
    assert_eq!(
        zeroclaw_api::channel::Channel::self_handle(&*channel),
        None
    );
    assert_eq!(
        zeroclaw_api::channel::Channel::self_addressed_mention(&*channel),
        None
    );
    assert!(!zeroclaw_api::channel::Channel::health_check(&*channel).await);

    // Capabilities this plugin does NOT declare (typing indicators) must
    // fall back to the host-composed trait default rather than erroring,
    // proving the host never calls into the guest for an unset flag.
    zeroclaw_api::channel::Channel::start_typing(&*channel, "someone")
        .await
        .expect("start_typing should resolve via the unset-capability default");
}
