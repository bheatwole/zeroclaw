# Microsoft Teams channel: operator setup

This plugin delivers inbound messages by **polling Microsoft Graph**, never
by receiving a webhook. Channel plugins run sandboxed in WASM and cannot
host an inbound HTTPS listener, so the classic Bot Framework "messaging
endpoint" model doesn't apply here. The Bot Framework / Teams-app
registration in step 3 below exists **only** so the bot appears in the
native Teams @-mention picker (and so `mentions[].mentioned.application.id`
gets populated in Graph's API) -- its messaging endpoint is configured but
never actually used by this plugin. This is non-obvious, so it's called out
explicitly rather than left as an unexplained extra step.

## 1. Azure AD app registration

1. In the Azure portal, register a new application (**App registrations** →
   **New registration**). Note the **Application (client) ID** and
   **Directory (tenant) ID**.
2. Create a client secret (**Certificates & secrets** → **New client
   secret**). Copy the secret value immediately -- it's not retrievable
   later.
3. Grant **Application** (not delegated) Microsoft Graph API permissions:
   - `ChannelMessage.Read.All`
   - `ChannelMessage.Send`
4. Have a tenant administrator grant admin consent for these permissions
   (**API permissions** → **Grant admin consent**).

You now have `tenant_id`, `client_id`, `client_secret` for
`credentials.toml` (see `../credentials.toml.example`).

## 2. Teams application access policy

Application permissions on `ChannelMessage.*` additionally require a Teams
**application access policy** before the app can call these Graph
endpoints against real teams -- admin consent above is not sufficient by
itself. Using Teams PowerShell, as a tenant admin:

```powershell
Connect-MicrosoftTeams
New-CsApplicationAccessPolicy -Identity "ZeroClawTeamsAccess" -AppIds "<client_id>"
Grant-CsApplicationAccessPolicy -PolicyName "ZeroClawTeamsAccess" -Identity <user-or-team-owner-upn>
```

This is a real, non-automatable step the tenant admin must perform; the
plugin has no way to do this on its own behalf.

## 3. Bot Framework / Teams app registration (mention-picker only)

1. Register an **Azure Bot** resource (Azure portal → **Create a resource**
   → **Azure Bot**) using the same `client_id`/`tenant_id` from step 1. Fill
   in any HTTPS URL as the messaging endpoint -- it is never called by this
   plugin, since there is no listener behind it. Note the resulting AAD app
   id; it should match `client_id` if you reused the same registration (or
   record it separately as `bot_app_id` in `credentials.toml` if not).
2. Add a **Microsoft Teams** channel to the Azure Bot resource so it's
   eligible to be referenced from a Teams app manifest's `bots` array.
3. Fill in `manifest.json` in this directory: set `id` and `bots[0].botId`
   to `bot_app_id`.
4. Supply real icons per `ICONS.md`, then zip `manifest.json` + `color.png` + `outline.png` into an app package.
5. Sideload the package into the target team (Teams client → **Apps** →
   **Manage your apps** → **Upload an app** → **Upload a custom app**), or
   publish it to your org's app catalog for broader rollout.

Once installed, users can @-mention the bot by name in that team's
channels and the mention will appear as a real chip, populating
`mentions[].mentioned.application.id` with `bot_app_id` -- the primary
signal this plugin uses to detect it's being addressed (see
`src/mentions.rs`). Until this step is done, the plain-text `@DisplayName`
fallback still works for testing.

## 4. credentials.toml and manifest.toml

1. Copy `../credentials.toml.example` to a file named exactly
   `teams_credentials.toml`, fill in `tenant_id`, `client_id`,
   `client_secret`, `bot_app_id`, `bot_display_name`, and the `[[teams]]`
   entries for each channel the bot should watch (team/channel IDs are
   visible in a Teams channel's "Get link to channel" URL).
2. Place that file in a host directory of your choosing, e.g.
   `/etc/zeroclaw/secrets/teams/`. Restrict its permissions -- it contains
   a client secret.
3. Create a separate, writable directory for persisted poll state, e.g.
   `/var/lib/zeroclaw/plugins/teams-state/`. This must stay writable by the
   process running the plugin host; it holds a small `teams_state.json`
   the plugin uses to avoid re-surfacing old messages after a restart.
4. Copy `../manifest.toml.example` to `manifest.toml` next to the built
   `.wasm`, and update both `dir` permissions' `host_path` values to the
   directories from steps 2-3.

## 5. Troubleshooting

- **401 from the token endpoint**: wrong `client_id`/`client_secret`/
  `tenant_id`, or the client secret expired.
- **403 from Graph `ChannelMessage.Read.All`/`ChannelMessage.Send` calls**:
  either admin consent (step 1.4) or the application access policy (step 2)
  is missing -- both are required, separately, for app-only access to
  channel messages.
- **Bot never appears in the @-mention picker**: the Teams app package
  (step 3) hasn't been installed in that team, or `bots[0].botId` in
  `manifest.json` doesn't match `bot_app_id`. The plain-text `@DisplayName`
  fallback still works in the meantime.
- **Messages re-surface after a restart**: the state directory (step 4.3)
  isn't actually writable by the plugin process, or its `dir_write`/
  `file_write` permissions in `manifest.toml` are `false`.
