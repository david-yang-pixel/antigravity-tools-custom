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

## Binding China and Global IDs to one public name

China and Global accounts often list different native IDs for the same model (for
example `shared-model` and `shared-model-sg`). Bind them to one public name so
clients request a single ID:

- The first ID is the primary `native_model`; the rest go in `extra_native_models`.
  All bound IDs share one account pool, scheduled by the existing policy with no
  preference between them.
- Each request is sent with the native ID of the selected account: an account must
  list that ID in its model catalog. An account listing several bound IDs uses the
  first one in the order above. Free-model and exhausted-quota checks apply per ID.
- Billing, statistics and the exposed model name always use the public ID, never a
  native ID. `x-mapped-model` reports the native ID actually sent.
- Headless configuration: add the same JSON field to the published entry, either by
  editing `model_registry.json` or by submitting it through the admin API
  `save_model_registry` (`POST /api/model-registry`). Without the field, files and
  behavior are unchanged.
- Rollback and downgrade: remove the extra IDs (save the publication without them)
  to use only the primary ID. **Clear every publication's extra IDs before
  downgrading to a version that does not know the field**: an older build rejects a
  `model_registry.json` containing `extra_native_models`, the registry fails to load
  and all inference requests are refused. Registries that never used the field are
  unaffected.

```json
{
  "public_id": "qoder-model",
  "source_id": "qoder:default",
  "native_model": "shared-model",
  "extra_native_models": ["shared-model-sg"]
}
```

## Maintenance and rotation

- Enabled accounts follow the global **API Proxy → Account Rotation & Scheduling**
  policy, shared with Antigravity and WorkBuddy. Cache First preserves the session
  account and waits when busy/cooling (at most the configured wait and request
  deadline); Balance preserves affinity and switches when that account is unavailable;
  Performance uses load/least-recent-use selection without session affinity.
- The source switch and fixed-account selector live in **API Proxy → Models →
  Source configuration**, next to WorkBuddy's (`/api-proxy?section=sources`).
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
- Background maintenance checks due accounts every minute. Model and quota polls
  follow real use: they run about every 15–18 minutes (a stable per-account offset)
  while the account completed a generation in the last six hours, and stop when it
  is idle. An idle account whose last known balance is zero keeps a quota-only
  recovery check at the same 15–18 minute interval, including after a restart.
  This check does not fetch models or initialize UMID, and stops once credits
  return. The 15-minute quota admission TTL is unchanged. Other idle accounts are
  only renewed when their token is within five minutes of expiry. A catalog that
  was never fetched is always due, and manual sync is unaffected. Activity is
  tracked in memory, so after a restart normal catalog polls resume with the first
  request. Disabling the source, account or automatic refresh also stops recovery.
- A token within five minutes of expiry is refreshed before use.
- Refresh exchanges are serialized per account. Rotating tokens are persisted
  before metadata requests. An HTTP cancellation does not cancel an owned refresh
  task before persistence; a new authorization or deletion rejects stale writes.
- Default concurrency is five active inferences per account. The permit remains
  held through upstream streaming and is released on cancellation.
- Authentication failures, rate limits and transient failures cool the affected
  account. A request moves to another account only when nothing reached the
  upstream (refresh, route or request-preparation failure), or once after a
  connection failure without an HTTP response. Unsent attempts do not consume
  that connection retry allowance; the five-account limit and shared deadline
  still apply. An upstream answer — 401/403 after one refresh, 429, 5xx, or a
  stream error before output — is returned to the client and is not replayed on
  other accounts; a later request is scheduled away from the cooling account.
  A failure during the same-account authorization retry retains the original
  rejection, even if that retry fails before sending. A partially delivered
  generation is never replayed.
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
| `ABV_QODER_MAX_CONCURRENCY` | `5` | Active requests per account, clamped to 1–8; explicit deployment overrides take precedence |
| `ABV_QODER_VERSION` | `1.1.83` | Qoder client version (defaults to `1.1.75` in legacy mode) |
| `ABV_QODER_LEGACY_PROTOCOL` | `false` | Explicit rollback to the previous Rust COSY signer and rustls transport; restart after changing |
| `ABV_QODER_ALLOW_DIRECT_UMID` | `false` | Explicitly allow the official identity SDK to connect directly when Qoder uses a proxy; exposes the host network/machine identity outside that proxy |

Without egress, the normal gateway upstream-proxy setting applies to native account
traffic. Credential-bearing requests validate HTTPS service hosts and do not follow
redirects.

The official native UMID SDK uses its own network stack. When Qoder uses an account
egress node or the general upstream proxy, modern model/inference signing fails
before starting that SDK unless `ABV_QODER_ALLOW_DIRECT_UMID=true` is explicitly
set. Alternatively, use the legacy protocol. Release detection does not invoke UMID.

The Qoder account page checks the official npm `latest` release when opened and
offers a manual refresh. It shows the bundled version and last successful check;
version overrides and legacy mode are displayed separately. Checks use the general
upstream proxy without account credentials, cache successful results for 24 hours,
and retain the last successful result if npm is unavailable. Manual requests are
limited to one per minute per gateway. No component is installed automatically.
Headless administrators can use `POST /api/native-providers/client-version` with
`{"provider":"qoder","refresh":false}` (or `true` for manual refresh).

## Account isolation and egress nodes

- Every account has its own HTTP connection pool and TLS session cache (no socket
  carries two accounts' tokens), and persisted machine/device ids. The default
  1.1.83 protocol uses the official UMID machine code/token/type and the server's
  normalized hostname. These describe the real host; separate accounts on the
  same host are not presented as different physical machines. The older derived
  address/hostname is used only with the explicit legacy protocol setting.
- **Accounts → Qoder → Egress nodes** routes each account through one node of an
  airport subscription, like WorkBuddy (see `docs/WORKBUDDY_UPSTREAM.md`). The
  subscription link, node filters and auto-assign are shared with WorkBuddy, and
  one mihomo process serves both; the switch and bindings are per source. Within
  Qoder a node serves one account; a WorkBuddy account and a Qoder account may share
  a node because they talk to different services.
- Sign-in polling, credential import validation, refresh, model, quota and inference
  requests all use the account's node. Login and import claim a free node and the
  new account keeps it; reauthorization uses the account's bound node. A sign-in or
  import of a saved account through a different node is refused. A bound account
  whose node is unusable fails instead of falling back to the server's address.
- The official native UMID component initializes its own device identity outside
  this HTTP transport; its SDK traffic is not covered by the per-account proxy.
  It is invoked lazily, uses a bounded process lifetime and caches successful
  results, rather than introducing another background account heartbeat.
- `DIRECT` sends that account from this server's own address. With egress off the
  upstream-proxy setting applies, as before.
- Subscription entries named `DIRECT` (case/whitespace variants included), or
  reported as type `Direct`, are excluded from the node list and auto-assignment.
  If no proxy candidates remain, assignment fails instead of selecting direct.
  Administrators can still explicitly choose `DIRECT` for an account.
- Bindings are stored under `pools.qoder` in `workbuddy_egress.json` (the file keeps
  its name; older versions ignore the new field).

| Environment variable | Purpose |
| --- | --- |
| `ABV_QODER_EGRESS_ENABLED` | `true`/`false`; locks Qoder's egress switch |
| `ABV_EGRESS_SUBSCRIPTION` | Shared subscription link (`ABV_WORKBUDDY_EGRESS_SUBSCRIPTION` is still read) |
| `ABV_MIHOMO_PATH` | mihomo executable |

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
| `/api/native-egress` | Provider; egress settings, nodes and this source's bindings |
| `/api/native-egress/save` | Provider plus any of `enabled`, `subscription_url`, `include`, `exclude`, `auto_assign` |
| `/api/native-egress/refresh`, `/api/native-egress/test-nodes` | Provider |
| `/api/native-egress/account` | Provider, account `id`, `node` (name, `"DIRECT"`, or `null` for auto-assign) |
| `/api/native-egress/account/test` | Provider and account `id`; latency along the account's real path |

Qoder credential import also accepts `personalToken`; it is exchanged for a job
token. Imported account identity is checked with the provider before persistence.
An access, refresh or personal token already saved for this provider is rejected
before obtaining a login route or making a network request, even if the selected
region differs. Refresh or reauthorize that account from its own row instead;
this keeps its existing route and avoids discarding a rotated refresh token when
an import is refused. Fresh credentials continue through normal import validation.

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
cargo test -p qoder-client --lib --locked
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
- `@qoder-ai/qodercli@1.1.83`: pinned Apache-2.0 WASM signer/decoder and native
  UMID components. Provenance, npm integrity, per-file checksums and license are
  in `src-tauri/crates/qoder-client/vendor/`. See
  [client compatibility](../QODER_CLIENT_FINGERPRINT.md) for the transport profile,
  offline evidence, runtime requirements and rollback setting.

The integration uses independently implemented Rust state management and protocol
adapters; it does not bundle or run the referenced gateway.

The modern Qoder protocol embeds OpenSSL 3.5, the WASM module and the native
identity helper for the build target. No Node installation or runtime downloads
are needed. The first model/inference request extracts the verified helper under
`<data-dir>/qoder-runtime/`; that directory must be writable and permit execution.
The official helper supports Linux/macOS x64 and ARM64, and Windows x64. For a
protocol regression or unsupported helper platform, set
`ABV_QODER_LEGACY_PROTOCOL=true` and restart to select the previous COSY/rustls
path. Modern failures do not automatically downgrade or replay the request.

## Rolling back the integration

Before reverting to a version that does not recognize the native Qoder source,
unpublish their models and back up the data directory. Restore the pre-integration
`model_registry.json` when reverting: older versions also reject native candidate
records, even after unpublishing. Keep existing Antigravity/WorkBuddy bindings. Account vaults use the current
machine's encryption identity and are not portable plaintext credential exports.
