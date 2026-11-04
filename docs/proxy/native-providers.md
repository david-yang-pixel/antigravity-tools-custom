# Native TRAE and Qoder accounts

TRAE and Qoder are built-in account sources. Authorization, quota queries, refresh,
model discovery and inference run in the Rust application; no external Python/Go
gateway is required. These integrations use the client protocols described in the
references below. Live compatibility must be checked with an authorized account.

## Using an account

1. Open **Accounts → Qoder** or **Accounts → TRAE**, choose China or Global, and
   request an authorization link. TRAE SOLO accounts have a separate option.
2. Complete sign-in on the official website. Qoder's device grant is polled; TRAE
   delivers its grant with a browser request. An existing account's reauthorization must
   return the same provider, region and user identity.
3. TRAE's official page accepts only `http://127.0.0.1:<port>/authorize` as its
   callback. In a remote Web deployment, **Open official sign-in** opens a temporary
   server-owned Chromium session inside the account page. Scan or sign in there;
   its loopback receiver delivers the grant privately to Rust for PKCE exchange,
   identity validation and encrypted persistence. No local helper, extension or
   callback paste is required. Desktop mode binds its loopback listener on
   `ABV_TRAE_CALLBACK_PORT` before creating the authorization link; if another
   process already holds that port, sign-in stops instead of directing the grant to it.
   Server browsers need Node.js 22+ and Chromium/Chrome/Edge. Each session lasts at
   most ten minutes; success, cancellation, expiry and process failure close it
   and remove its temporary profile. At most two browsers run concurrently.
   The server must be able to access the selected official login website. A
   provider region restriction is reported explicitly and cannot be solved by
   changing the callback. It does not mean the account was authorized.
4. Sync balance and models. Quota failures preserve the previous snapshot and its
   timestamp. Unknown balance and unlimited balance are separate states.
5. In the model registry, fetch models from **Qoder** or **TRAE**, explicitly test
   with your own system/user text, then publish public IDs. Discovery, sign-in and
   background maintenance do not run inference or automatically publish models.

Clients use the gateway's existing API address and client keys. Public model IDs
bind explicitly to `qoder:default` or `trae:default` and a provider-native model ID;
two providers' identically named models remain separate candidates.

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
| `qoder_accounts.enc`, `trae_accounts.enc` | Authenticated encrypted account vaults, including tokens and local account metadata |
| `qoder_settings.json`, `trae_settings.json` | Source enablement and the optional preferred account ID |

Encryption uses the application's existing machine-bound crypto utility. Web and
Tauri account views exclude credentials. A vault/settings error disables that
native source in discovery and leaves other sources available.

Provider settings have this shape:

```json
{"enabled":true,"preferred_account_id":null}
```

| Environment variable | Default | Purpose |
| --- | --- | --- |
| `ABV_QODER_ENABLED`, `ABV_TRAE_ENABLED` | `true` | Override source enablement; the UI then treats its source settings as read-only |
| `ABV_QODER_MAX_CONCURRENCY`, `ABV_TRAE_MAX_CONCURRENCY` | `1` | Active requests per account, clamped to 1–8 |
| `ABV_QODER_VERSION` | `1.1.75` | COSY/client protocol version |
| `ABV_TRAE_VERSION` | `3.5.66` | TRAE client version in authorization and service requests |
| `ABV_TRAE_BROWSER_PATH` | Platform Chromium/Chrome/Edge executable | Server login browser executable |
| `ABV_TRAE_BROWSER_NODE` | `node` | Node.js 22+ executable for the private browser driver |
| `ABV_TRAE_BROWSER_NO_SANDBOX` | `false` | Set `true` only in a container where Chromium cannot use its sandbox |
| `ABV_TRAE_BROWSER_PROXY` | none | Optional HTTP(S) or SOCKS5 browser proxy URL without credentials; separate from Rust provider requests |
| `ABV_TRAE_CALLBACK_PORT` | `18080` | Desktop loopback callback port |

The normal gateway upstream-proxy setting also applies to native account traffic.
Credential-bearing requests validate HTTPS service hosts and do not follow redirects.

## Headless admin API

The following authenticated admin endpoints share their implementation with the
desktop commands. All use POST with a JSON body; they require the existing admin
authentication, not an ordinary inference client key.

| Endpoint | Body |
| --- | --- |
| `/api/native-accounts/list` | `{"provider":"qoder"}` |
| `/api/native-login/start` | `{"provider":"trae","realm":"cn","solo":false,"account_id":null,"server_browser":true}` |
| `/api/native-login/poll`, `/api/native-login/cancel` | `{"provider":"trae","id":"<login-session-id>"}` |
| `/api/native-login/browser/snapshot` | Provider and login `id`; returns a no-store JPEG frame, dimensions and a URL without query/fragment |
| `/api/native-login/browser/input` | Provider, login `id`, and bounded `action` (`click`, `text`, `key`, `scroll`, `reload`) |
| `/api/native-login/complete` | Provider, login `id`, and `callback_url` |
| `/api/native-accounts/import` | Provider, region and `auth` credential JSON |
| `/api/native-accounts/update` | Provider/account `id`; optional `enabled`, `auto_refresh`, or `refresh:true`; no flags synchronizes metadata |
| `/api/native-accounts/delete` | Provider/account `id` |
| `/api/native-providers/settings` | Provider |
| `/api/native-providers/settings/save` | Provider, `enabled`, and `preferred_account_id` |

Qoder credential import also accepts `personalToken`; it is exchanged for a job
token. Imported account identity is checked with the provider before persistence.

The browser endpoints require administrator authentication and a live TRAE session
started with `server_browser:true`. They expose no arbitrary navigation, script
execution, CDP endpoint or files. Chromium's debugging endpoint and grant receiver
bind only to loopback. The driver communicates with Rust through private process
pipes; callback URLs/grants, text input and screenshots are not logged or written
as application data. Chromium's temporary profile may contain official cookies
while the session is live and is removed when it ends.

Server-browser callbacks require the session's exact trace and an official TRAE
origin (or a top-level navigation without Origin). Grant acknowledgement occurs
only after the existing PKCE/identity/persistence service accepts it. Cancellation
also prevents a grant exchange in progress from saving a new account. Screenshots
and input may contain sensitive sign-in information and use the existing HTTPS
admin channel. Downloads are disabled in this private browser.

Desktop direct callbacks and credential import retain their existing behavior.

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
node --test src-tauri/src/modules/native_providers/browser_runner.test.mjs src/types/nativeProviders.test.mjs src/components/proxy/models/registryLogic.test.mjs
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
- [trae-api-proxy](https://github.com/muskke/trae-api-proxy): PKCE, token exchange,
  current TRAE model/inference payloads and SSE events (MIT).
- [deepseek-harness-trae quota reader](https://github.com/Fleey/deepseek-harness-trae/blob/main/src/trae/quota.ts):
  v1/v2 entitlement response field layouts and unknown-quota behavior.
- [TRAE official community network troubleshooting](https://forum.trae.cn/t/topic/15428):
  the domestic `trae-api-cn.mchost.guru` core endpoint.
- [dsh-account-pool](https://github.com/cliii-one/dsh-account-pool): browser-fetch
  delivery and why a loopback callback cannot reach a remote deployment.
- [trae-cn-provider](https://github.com/JsonLing/trae-cn-provider): direct `userJwt`
  callbacks and account-specific token service hosts.

The integration uses independently implemented Rust state management and protocol
adapters; it does not bundle or run the referenced gateways.

## Rolling back the integration

Before reverting to a version that does not recognize native TRAE/Qoder sources,
unpublish their models and back up the data directory. Restore the pre-integration
`model_registry.json` when reverting: older versions also reject native candidate
records, even after unpublishing. Keep existing Antigravity/WorkBuddy bindings. Account vaults use the current
machine's encryption identity and are not portable plaintext credential exports.
