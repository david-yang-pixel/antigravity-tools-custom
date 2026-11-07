# Native provider accounts

Native providers are account sources implemented in the Rust application: import,
refresh, model discovery, scheduling and inference run in the gateway, with no
external helper. The only native provider is **ChatGPT** (Codex backend); its
details follow the shared sections below.

## Using an account

1. Open **Accounts → ChatGPT** and import credentials (see
   [Import and re-import](#import-and-re-import)).
2. Sync models. Quota is reported by the upstream on inference; a failure keeps the
   previous snapshot and its timestamp.
3. In the model registry, fetch models from **ChatGPT**, explicitly test with your
   own system/user text, then publish public IDs. Discovery, import and background
   maintenance do not run inference or automatically publish models.

Clients use the gateway's existing API address and client keys. Public model IDs
bind explicitly to `chatgpt:default` and a provider-native model ID.

## Binding several native IDs to one public name

A publication may bind more than one native ID of the same source, so clients
request a single public ID:

- The first ID is the primary `native_model`; the rest go in `extra_native_models`.
  All bound IDs share one account pool, scheduled by the existing policy with no
  preference between them.
- Each request is sent with the native ID of the selected account: an account must
  list that ID in its model catalog. An account listing several bound IDs uses the
  first one in the order above. Quota checks apply per ID.
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
  "public_id": "gpt-model",
  "source_id": "chatgpt:default",
  "native_model": "gpt-model",
  "extra_native_models": ["gpt-model-latest"]
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
- Background maintenance checks due accounts every minute. Model polls follow
  real use: they run about every 15–18 minutes (a stable per-account offset) while
  the account completed a generation in the last six hours, and stop when it is
  idle. An idle account whose last known quota is exhausted keeps a recovery check
  at the same interval, including after a restart, and stops once quota returns. The 15-minute quota admission TTL is unchanged. Other idle accounts are
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
- Failures are charged by the provider's own classification (see **Failover
  policy** below). A request moves to another account when nothing reached the
  upstream (refresh, route or request-preparation failure), once after a
  connection failure without an HTTP response, or when the upstream answer is
  classified as failover before any output. A partially delivered generation is
  never replayed.
- A known, recent exhausted quota window excludes the account from scheduling
  until that window resets. Unknown or stale quota data does not invent an
  exhausted-account verdict.
- Removing an account removes its local credentials. Published bindings remain
  explicit records; requests need another eligible account for the same model.

## Storage and configuration

Files are relative to the existing application data directory (`ABV_DATA_DIR`):

| File | Contents |
| --- | --- |
| `chatgpt_accounts.enc` | Authenticated encrypted account vault (owner-only `0600`), including tokens and local account metadata |
| `chatgpt_settings.json` | Source enablement and the optional preferred account ID |

Encryption uses the application's existing machine-bound crypto utility. Web and
Tauri account views exclude credentials. A vault/settings error disables that
native source in discovery and leaves other sources available.

Provider settings have this shape:

```json
{"enabled":true,"preferred_account_id":null}
```

Without egress, the normal gateway upstream-proxy setting applies to native account
traffic. Credential-bearing requests validate HTTPS service hosts and do not follow
redirects.

## Account isolation and egress nodes

- Every account has its own HTTP connection pool (no socket carries two accounts'
  tokens); a route change rebuilds only that account's client.
- **Accounts → ChatGPT → Egress nodes** routes each account through one node of an
  airport subscription, like WorkBuddy (see `docs/WORKBUDDY_UPSTREAM.md`). The
  subscription link, node filters and auto-assign are shared with WorkBuddy, and
  one mihomo process serves both; the switch and bindings are per source. Within
  ChatGPT a node serves one account; a WorkBuddy account and a ChatGPT account may
  share a node because they talk to different services.
- Credential import validation, refresh, model and inference requests all use the
  account's node. An import claims a free node and the new account keeps it; a
  re-import uses the account's bound node. An import of a saved account through a
  different node is refused. A bound account whose node is unusable fails instead
  of falling back to the server's address.
- `DIRECT` sends that account from this server's own address. With egress off the
  upstream-proxy setting applies, as before.
- Subscription entries named `DIRECT` (case/whitespace variants included), or
  reported as type `Direct`, are excluded from the node list and auto-assignment.
  If no proxy candidates remain, assignment fails instead of selecting direct.
  Administrators can still explicitly choose `DIRECT` for an account.
- Bindings are stored under `pools.chatgpt` in `workbuddy_egress.json` (the file
  keeps its name; older versions ignore the field).

| Environment variable | Purpose |
| --- | --- |
| `ABV_CHATGPT_EGRESS_ENABLED` | `true`/`false`; locks ChatGPT's egress switch |
| `ABV_EGRESS_SUBSCRIPTION` | Shared subscription link (`ABV_WORKBUDDY_EGRESS_SUBSCRIPTION` is still read) |
| `ABV_MIHOMO_PATH` | mihomo executable |

## Headless admin API

The following authenticated admin endpoints share their implementation with the
desktop commands. All use POST with a JSON body; they require the existing admin
authentication, not an ordinary inference client key.

| Endpoint | Body |
| --- | --- |
| `/api/native-accounts/list` | `{"provider":"chatgpt"}` |
| `/api/native-accounts/import` | Provider, `auth` credential, optional `account_id` to re-import onto a row |
| `/api/native-accounts/import-batch` | ChatGPT multi-account export (`auth.accounts`); per-account report |
| `/api/native-accounts/update` | Provider/account `id`; optional `enabled`, `auto_refresh`, or `refresh:true`; no flags synchronizes metadata |
| `/api/native-accounts/delete` | Provider/account `id` |
| `/api/native-providers/settings` | Provider |
| `/api/native-providers/settings/save` | Provider, `enabled`, and `preferred_account_id` |
| `/api/native-egress` | Provider; egress settings, nodes and this source's bindings |
| `/api/native-egress/save` | Provider plus any of `enabled`, `subscription_url`, `include`, `exclude`, `auto_assign` |
| `/api/native-egress/refresh`, `/api/native-egress/test-nodes` | Provider |
| `/api/native-egress/account` | Provider, account `id`, `node` (name, `"DIRECT"`, or `null` for auto-assign) |
| `/api/native-egress/account/test` | Provider and account `id`; latency along the account's real path |

Imported account identity is checked with the provider before persistence. An
access or refresh token already saved for this provider (or a refresh token that
was imported before and has since rotated) is rejected before obtaining a login
route or making a network request. Re-import that account from its own row
instead; this keeps its existing route and avoids discarding a rotated refresh
token when an import is refused.

## ChatGPT accounts

ChatGPT (Codex backend) is the native provider `provider: "chatgpt"`. Protocol
facts are in [chatgpt-codex-protocol.md](chatgpt-codex-protocol.md).

> **Risk note.** Using a ChatGPT subscription outside Codex may violate OpenAI's
> terms of service and can get the account rate-limited or suspended. Use accounts
> you can afford to lose and review the terms before enabling the source.

### Import and re-import

There is no browser sign-in for ChatGPT (hence no realm; it is always `global`).
Open **Accounts → ChatGPT → Import credentials** and paste any of:

- the bare refresh token, or JSON such as `{"refresh_token":"..."}`;
- one account's credentials (`access_token`, `id_token`, `refresh_token`,
  `chatgpt_account_id`, `expires_at`), bare or under `credentials`;
- a Codex `auth.json` (`{"tokens": {...}}`);
- a sub2api export (`{"accounts": [...]}`): every `openai` account is imported one
  by one, other platforms are skipped, and the result lists each account.

When the paste carries an access token that is valid for more than five minutes,
it is kept and proven with one model-catalog request, so the import does not spend
the refresh token; it is refreshed later, close to expiry. An expired or rejected
access token falls back to the refresh token. Identity is verified with OpenAI
before saving. Stop using an imported account in any other tool (sub2api, Codex):
refresh tokens rotate, and two systems refreshing the same account can get its
whole token family revoked.

Headless: `import_native_account` / `POST /api/native-accounts/import` with
`{"provider":"chatgpt","auth":<any single-account shape above>}`, and
`import_native_accounts_batch` / `POST /api/native-accounts/import-batch` with
`{"provider":"chatgpt","auth":{"accounts":[...]}}`, which returns one
`{label, status: imported|skipped|failed, message, account}` per account.

Each account row has a **Re-import** action. It opens the same input and sends
`account_id`, replacing that account's credentials in place. A token that belongs
to a different ChatGPT account is rejected.

A credential that OpenAI rejects disables the account automatically; the row shows
`auth_error` with a hint to re-import. Quota is shown as one percentage bar per
rate-limit window the upstream reports, named by the window's own length
(`x-codex-*-window-minutes`, e.g. 5 hours and 7 days on paid plans, or a 30-day
window on free accounts), with its reset time.

### Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `ABV_CHATGPT_ENABLED` | `true` | Override source enablement; the UI then treats its source settings as read-only |
| `ABV_CHATGPT_MAX_CONCURRENCY` | `5` | Active requests per account, clamped to 1–8 |
| `ABV_CHATGPT_EGRESS_ENABLED` | `true`/`false` | Locks ChatGPT's egress switch |
| `ABV_CHATGPT_CLIENT_VERSION` | `0.167.0` | Advertised Codex client version; semver, must be `>= 0.144.0`. The model catalog depends on it: an old version is offered fewer, older models (e.g. no `gpt-6-*`), so raise it when Codex releases add models |

### Endpoints served

Chat Completions, Responses passthrough, Anthropic Messages, `/responses/compact`
and the Responses WebSocket. Public model IDs bind to `chatgpt:default` as with
other native sources; nothing is published automatically.

### Failover policy

Before any output has been sent, a 429, 5xx, capacity error or account
deactivation fails over to another account, up to 3 accounts per request. A
Cloudflare 403 is not charged to the account (no penalty). Context-length, policy
and validation errors are never replayed on another account.

### WebSocket behavior

When a Responses WebSocket session moves to another account (failover or
rotation), the gateway replays the session transcript onto the new account so the
conversation continues; the previous account's server-side state is not reused.

## Protocol scope and verification

ChatGPT speaks the Responses wire: Responses clients get it passed through, and
Chat Completions / Anthropic Messages clients are bridged through the shared Chat
relay, including streaming and function-tool history. Common authentication, model
admission, limits, monitoring and usage accounting remain in the gateway. Native
sources do not reuse Gemini thought signatures or claim Gemini/image generation
endpoint support.

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

Offline checks do not establish successful real-account authorization or
provider-side model permissions. Full desktop builds require the repository's
platform dependencies and CI.

## Removed Qoder source

The native Qoder source was removed. On upgrade:

- `model_registry.json` records bound to `qoder:default` (candidates, publications,
  ignore marks and verification results) are dropped when the registry loads and
  disappear from the file on the next saved change. Clients requesting a former
  Qoder public model ID get the usual unknown-model answer.
- `qoder_accounts.enc`, `qoder_settings.json` and the `pools.qoder` egress bindings
  are no longer read; delete them by hand if you no longer need them. The
  `ABV_QODER_*` environment variables have no effect.
- The device-grant sign-in endpoints (`/api/native-login/*`) and the client version
  check (`/api/native-providers/client-version`) were removed with it.

Account vaults use the current machine's encryption identity and are not portable
plaintext credential exports.
