# Native Qoder accounts

Qoder is a built-in account source. Authorization, quota queries, refresh, model
discovery and inference run in the Rust application; no external Python/Go gateway
is required. The integration uses the client protocol described in the references
below. Live compatibility must be checked with an authorized account.

## Using an account

1. Open **Accounts → Qoder**, choose China or Global, and request an authorization
   link.
2. Complete sign-in on the official website. The gateway polls Qoder's PKCE device
   grant; no callback, local helper or server-side browser is involved. An existing
   account's reauthorization must return the same provider, region and user identity.
3. Sync balance and models. Quota failures preserve the previous snapshot and its
   timestamp. Unknown balance and unlimited balance are separate states.
4. In the model registry, fetch models from **Qoder**, explicitly test with your own
   system/user text, then publish public IDs. Discovery, sign-in and background
   maintenance do not run inference or automatically publish models.

Clients use the gateway's existing API address and client keys. Public model IDs
bind explicitly to `qoder:default` and a provider-native model ID.

Qoder's request payload carries no structured-output, tool-selection or
parallel-call field. Requests with `response_format` / Responses `text.format`,
`tool_choice` `required` or a forced function, or `parallel_tool_calls: false` while
tools are offered are rejected with 400 before account selection instead of running
unconstrained; `tool_choice: none` is honored by sending no tools.

## Maintenance and rotation

- Enabled accounts follow the global **API Proxy → Account Rotation & Scheduling**
  policy, shared with Antigravity and WorkBuddy. Cache First preserves the session
  account and waits when busy/cooling (at most the configured wait and request
  deadline); Balance preserves affinity and switches when that account is unavailable;
  Performance uses load/least-recent-use selection without session affinity.
- The source's fixed-account selector overrides its account pool only. Selecting
  an account excludes the other accounts; disablement, deletion or a missing model
  causes an explicit error. Automatic selection restores the global mode.
- Session keys include the downstream token identity, canonical conversation,
  provider and native model. Bindings expire after one idle hour and are bounded
  to 10,000 entries. Mode changes and the global session-reset button clear them.
- Circuit-breaker enablement, backoff steps and global/per-account cooldown resets
  also apply to native accounts. Retry-After is a lower bound on backoff. Disabling
  the breaker clears local cooldowns but retains concurrency, model admission and
  account/quota checks. Antigravity-specific 5h/weekly rules are not native credits.
- Background maintenance checks due accounts every minute. Successful metadata
  synchronization schedules the next check five minutes later. A token within
  five minutes of expiry is refreshed before use.
- Refresh exchanges are serialized per account. Rotating tokens are persisted
  before metadata requests. An HTTP cancellation does not cancel an owned refresh
  task before persistence; a new authorization or deletion rejects stale writes.
- Default concurrency is one active inference per account. The permit remains
  held through upstream streaming and is released on cancellation.
- Authentication failures, rate limits and transient failures cool the affected
  account before another is selected. Failover only occurs before client output;
  a partially delivered generation is not replayed on another account.
  Cache First does not move a rate-limited conversation to another account after
  an upstream 429; a later request observes its bound account's bounded wait.
- A known, recent zero balance excludes credit-consuming models from scheduling.
  Models that official metadata marks as free remain eligible, with the same
  concurrency, authorization and cooldown limits. Unknown or stale quota data
  does not invent an exhausted-account verdict. Zero Qoder credits are a valid
  balance, including Standard accounts that only expose free models.
- Removing an account removes its local credentials. Published bindings remain
  explicit records; requests need another eligible account for the same model.

## Storage and configuration

Files are relative to the existing application data directory (`ABV_DATA_DIR`):

| File | Contents |
| --- | --- |
| `qoder_accounts.enc` | Authenticated encrypted account vault (owner-only `0600`), including tokens and local account metadata |
| `qoder_settings.json` | Source enablement and the optional preferred account ID |

Encryption uses the application's existing machine-bound crypto utility. Web and
Tauri account views exclude credentials. A vault/settings error disables that
native source in discovery and leaves other sources available.

Provider settings have this shape:

```json
{"enabled":true,"preferred_account_id":null}
```

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `ABV_QODER_ENABLED` | `true` | Override source enablement; the UI then treats its source settings as read-only |
| `ABV_QODER_MAX_CONCURRENCY` | `1` | Active requests per account, clamped to 1–8 |
| `ABV_QODER_VERSION` | `1.1.75` | COSY/client protocol version |

The normal gateway upstream-proxy setting also applies to native account traffic.
Credential-bearing requests validate HTTPS service hosts and do not follow redirects.

## Headless admin API

The following authenticated admin endpoints share their implementation with the
desktop commands. All use POST with a JSON body; they require the existing admin
authentication, not an ordinary inference client key.

| Endpoint | Body |
| --- | --- |
| `/api/native-accounts/list` | `{"provider":"qoder"}` |
| `/api/native-login/start` | `{"provider":"qoder","realm":"cn","account_id":null}`; returns the session `id` and `auth_url` |
| `/api/native-login/poll`, `/api/native-login/cancel` | `{"provider":"qoder","id":"<login-session-id>"}` |
| `/api/native-accounts/import` | Provider, region and `auth` credential JSON |
| `/api/native-accounts/update` | Provider/account `id`; optional `enabled`, `auto_refresh`, or `refresh:true`; no flags synchronizes metadata |
| `/api/native-accounts/delete` | Provider/account `id` |
| `/api/native-providers/settings` | Provider |
| `/api/native-providers/settings/save` | Provider, `enabled`, and `preferred_account_id` |

Qoder credential import also accepts `personalToken`; it is exchanged for a job
token. Imported account identity is checked with the provider before persistence.

## Protocol scope and verification

The shared Chat codec/relay serves Chat Completions, HTTP Responses and Anthropic
Messages, including streaming and function-tool history. Common authentication,
model admission, limits, monitoring and usage accounting remain in the gateway.
Native sources do not reuse Gemini thought signatures or claim Gemini/image
generation endpoint support. Hosted Responses tools such as `web_search` remain
subject to the existing Chat codec's limitations.

Run focused checks on a development machine with the normal Rust/Tauri dependencies:

```bash
cd src-tauri
cargo test --lib native_providers
cargo test --lib proxy::upstream::native::tests
```

Frontend checks:

```bash
./node_modules/.bin/tsc --noEmit
node --test src/types/nativeProviders.test.mjs src/components/proxy/models/registryLogic.test.mjs
```

Tests cover vault/view credential isolation, stale-refresh rejection, quota
parsing, shared/unlimited balances, device grant cancellation, account rotation,
COSY signing, explicit model publication, source isolation and stream/tool/usage
conversion. Offline checks do not establish successful real-account authorization
or provider-side model permissions. Full desktop builds require the repository's
platform dependencies and CI.

## Protocol references

- [qoder2api-hub](https://github.com/shuishuipingan/qoder2api-hub): device token
  exchange, COSY public key/encoding, model catalog and quota wire layout (MIT).

The integration uses independently implemented Rust state management and protocol
adapters; it does not bundle or run the referenced gateway.

## Removed TRAE source

An earlier beta also integrated TRAE (`trae:default`). It has been removed. Existing
`trae_accounts.enc` / `trae_settings.json` files are no longer read and can be deleted.
Stored model-registry candidates and publications that reference `trae:default` still
load, so the registry stays readable, but they never route: inference on such a
publication returns 503 ("Model source is not integrated"), and discovery or a new
publication on that source is refused. Unpublish those models to clean them up.

## Rolling back the integration

Before reverting to a version that does not recognize the native Qoder source,
unpublish their models and back up the data directory. Restore the pre-integration
`model_registry.json` when reverting: older versions also reject native candidate
records, even after unpublishing. Keep existing Antigravity/WorkBuddy bindings. Account vaults use the current
machine's encryption identity and are not portable plaintext credential exports.
