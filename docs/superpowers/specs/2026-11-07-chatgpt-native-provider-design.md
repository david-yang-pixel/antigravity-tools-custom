# ChatGPT subscription accounts as a native provider

## Problem

ChatGPT Plus/Pro/Team subscriptions include Codex usage served by
`https://chatgpt.com/backend-api/codex/responses`. The gateway cannot pool these accounts today:
the only native provider is Qoder, whose shared layer still calls Qoder functions directly, and
every native request is converted to and from Chat Completions, which loses Responses-only state
(encrypted reasoning, hosted tools) that Codex-style clients replay between turns.

## Goal

Add `ChatGPT` as a second native provider (`source = "chatgpt:default"`) with parity across the
three client protocols, HTTP and the Codex Responses WebSocket:

- Accounts are added by importing a refresh token; tokens refresh automatically.
- Chat Completions, Responses and Messages clients reach ChatGPT models over HTTP, streaming and
  non-streaming. Responses clients get a passthrough that preserves every Responses item.
- `/responses/compact` works for ChatGPT-backed models.
- `GET /v1/responses` WebSocket sessions whose model is ChatGPT-backed are relayed to the
  upstream `wss://chatgpt.com/backend-api/codex/responses`, keeping `previous_response_id`.
- Model catalog is fetched from the upstream; quota is read from response headers.
- Desktop UI, headless HTTP admin API and environment variables stay at parity.

Non-goals (later iterations): browser OAuth login, Codex `auth.json` / personal access token
import, the `wham/usage` quota endpoint (needs a browser TLS fingerprint), image generation,
reset-credit consumption, Agent Identity, FedRAMP accounts, HTTP/2 (the shared reqwest build has
`default-features = false` without `http2`; enabling it would change every reqwest client).

Protocol facts are recorded in `docs/proxy/chatgpt-codex-protocol.md` (observed in sub2api,
LGPL-3.0). This is a clean-room reimplementation from those facts; no sub2api code is translated.

## Architecture

```
client ─► ip_filter ─► auth ─► model_registry ─► user_token_limits ─► monitor
        ─► workbuddy::route ─► openai_compat::route ─► native::route ─┬─ Qoder  (Chat wire)
                                                                      └─ ChatGPT (Responses wire)
GET /v1/responses (WS, master key only) ─► handle_websocket_session
        └─ first generating frame resolves to chatgpt:default ─► chatgpt websocket relay
```

### 1. Shared native-provider layer (`src-tauri/src/modules/native_providers*`)

- `Provider::ChatGpt` (`key "chatgpt"`, `name "ChatGPT"`, `source "chatgpt:default"`).
  `Provider::env` yields `ABV_CHATGPT_ENABLED`, `ABV_CHATGPT_MAX_CONCURRENCY`,
  `ABV_CHATGPT_EGRESS_ENABLED` with no extra code.
- Every direct Qoder call in shared code becomes a `match account.provider` dispatch:
  `accounts::fresh` (refresh), `accounts::fetch_metadata` (models, quota),
  `login::import` (refresh + identity), `http::official_url` (host allowlist),
  `commands/native_providers.rs` (client-version check, egress test URL, delete-time
  `qoder::sign::forget` only for Qoder), `model_registry_service::discover_native_account`.
- `http.rs` client cache stores a `Transport` enum: `Qoder(qoder_client::transport::Client)`
  and `Plain(reqwest::Client)`. Each provider builds its own kind; the cache key, egress route
  resolution (`client_for` / `client_on` / `login_client` / `forget`) and node-change rebuild
  logic are shared. Plain clients: no redirects, no environment proxies, proxy from the egress
  `Exit`, no overall timeout (callers bound by deadline). `read_json` decrypts only for Qoder.
- `Credentials` gains `#[serde(default)] id_token: String` and
  `#[serde(default)] chatgpt_account_id: String`. Existing vaults load unchanged.
  `Account::new` keeps generating Qoder's machine/device ids only for Qoder.
- ChatGPT accounts always use `Realm::Global`; the UI hides the realm selector for ChatGPT and
  the import request may omit `realm` for it.
- Dedup key stays `(realm, uid)`; ChatGPT `uid = "<chatgpt_account_id>:<chatgpt_user_id>"`
  (falls back to `sub`), so one person in two workspaces is two accounts.

### 2. ChatGPT provider module (`native_providers/chatgpt/`)

- `identity.rs` — decode JWT payloads without signature verification; read
  `email`, `sub`, `exp` and the `"https://api.openai.com/auth"` claims
  `chatgpt_account_id`, `chatgpt_user_id`, `chatgpt_plan_type`. id_token wins, access token
  fills gaps. Missing `chatgpt_account_id` is an import error.
- `auth.rs` — `refresh(client, &mut Account)`: form POST to
  `https://auth.openai.com/oauth/token` with `grant_type=refresh_token`, `refresh_token`,
  `client_id=app_EMoamEEZ73f0CkXaXp7hrann`, `scope=openid profile email`, canonical
  `User-Agent` + `originator` (no `version`). Stores `access_token`, `id_token`,
  `expires_at = now + expires_in`; replaces `refresh_token` only when the response carries a
  non-empty one. Errors containing `invalid_grant`, `invalid_refresh_token`, `token_expired`,
  `refresh_token_reused`, `refresh_token_invalidated`, `app_session_terminated`,
  `access_denied` set `auth_error` and stop auto refresh (dead credential); `invalid_client`,
  `unauthorized_client`, `invalid_scope` are reported as provider configuration errors without
  marking the account dead. Request-path refresh when `expires_at - now < 180 s`; maintenance
  refresh when `< 1800 s` (existing `fresh` / maintenance loop, provider-specific thresholds).
- `identity_headers.rs` — one canonical client identity on every chatgpt.com request:
  `originator: codex_cli_rs`, `User-Agent: codex_cli_rs/<ver> (<os>; <arch>) xterm-256color`,
  `version: <ver>`. `<ver>` = `ABV_CHATGPT_CLIENT_VERSION` if it parses as semver ≥ 0.144.0,
  else the built-in default `0.146.0`. Client-supplied identity headers are never forwarded.
- `models.rs` — `GET https://chatgpt.com/backend-api/codex/models?client_version=<ver>` with
  auth + `chatgpt-account-id` + identity headers; map `models[].{slug, display_name,
  context_window, input_modalities, supported_reasoning_levels, visibility, supported_in_api}`
  into `Model` (skip slugs containing `*`, skip `visibility == "hide"`). On failure keep the
  previous catalog and set `models_error`. No hard-coded model list.
- `quota.rs` — parse `x-codex-{primary,secondary}-{used-percent,reset-after-seconds,
  window-minutes}` from every inference response (HTTP and WS handshake). Window mapping: the
  smaller known window is "5h", the other "7d"; one known window ≤ 360 min is "5h"; none known →
  primary "7d", secondary "5h". Stored as `Quota { unit: "%", packs: [5h, 7d] }` with
  `used = used_percent`, `remaining = 100 - used`, `total = 100`, `expires_at = now + reset`.
  Persisted at most once per 30 s per account.
- `payload.rs` — Codex request rules applied to any Responses body before sending:
  - force `stream: true`, `store: false` (compact: delete both);
  - `instructions` must exist; missing/blank → the default Codex base instructions shipped as a
    text asset (`include_str!`), chosen as one current prompt (not per model family);
  - `role: "system"` input messages are moved into `instructions` (prepended, blank-line joined);
  - string `input` → one user message item;
  - `include` gains `reasoning.encrypted_content` when `reasoning` is present;
  - delete `max_output_tokens`, `max_completion_tokens`, `temperature`, `top_p`,
    `frequency_penalty`, `presence_penalty`, `user`, `metadata`, `prompt_cache_retention`,
    `safety_identifier`, `stream_options`, `truncation`, `stop_sequences`, `background`;
    delete `previous_response_id` on HTTP only;
  - `reasoning.effort: "minimal"` → `"none"`;
  - reasoning items: drop `id`, keep `encrypted_content`, default `summary: []`;
  - drop item `id`s on replayed input items; normalize tool call ids to `fc_` prefix (hash ids
    longer than 64 chars), applying the same mapping to the matching `function_call_output`;
  - remove `function_call_output` items with no matching call;
  - Chat-style function tools `{type:function,function:{...}}` flattened to top-level
    `name/description/parameters/strict`;
  - replayed `web_search_call` without a declared web_search tool → add
    `{"type":"web_search","external_web_access":false}` (and `tool_choice: "none"` if the
    caller declared no tools).
- `errors.rs` — classify an upstream answer into the existing lease outcomes:
  - 429 `usage_limit_reached` / `rate_limit_exceeded` with `resets_at` or `resets_in_seconds`,
    or a quota header at ≥ 100 % → quota cooldown until the reset time (the lease's existing
    quota outcome with `retry_after = reset - now`), plan updated from `plan_type`;
  - other 429 → rate-limit cooldown honoring `Retry-After`;
  - 401 → existing forced refresh + one same-account retry; `token_revoked` /
    `token_invalidated` / `{"detail":"Unauthorized"}` → dead credential;
  - 402, `deactivated_workspace`, `{account|workspace|organization}_{deactivated|disabled|suspended}`
    → account disabled with `auth_error`;
  - 403 HTML or Cloudflare markers (`cf-mitigated: challenge`, "Just a moment", `error code: 1010`)
    → no account penalty, 502 to the client; other 403 → auth cooldown;
  - context-length, policy and other 4xx validation errors → passed to the client unchanged;
  - upstream answers are **not replayed on another account** (existing native policy); only
    preparation failures and one connection failure fail over.
- `client.rs` — `send(account, model, body, deadline)` POSTs to
  `…/codex/responses` (or `…/compact`) with `Authorization`, `chatgpt-account-id`, identity
  headers, `accept: text/event-stream` (compact: `application/json`), `session_id` and
  `conversation_id` set to a stable id derived from
  `sha256("<scope>:<chatgpt_account_id>:<prompt_cache_key or session key>")` formatted as a
  UUID, and `x-codex-beta-features: remote_compaction_v2` when the client sent none. Allowed
  client passthrough headers: `accept-language`, `x-codex-turn-state`, `x-codex-turn-metadata`,
  `x-codex-window-id`, `x-codex-installation-id`. Response header `x-codex-turn-state` is
  returned to the client. Quota headers are recorded on every answer.

### 3. Responses wire, shared (`src-tauri/src/proxy/upstream/responses_wire.rs`)

Protocol-generic converters so a future Responses-mode custom upstream can reuse them:

- `from_chat(chat) -> responses` — Chat request (output of `chat_codec::request_for`) to a
  Responses body: messages → input items (system/developer → `instructions` candidates, user
  parts → `input_text`/`input_image`, assistant text → `output_text`, `tool_calls` →
  `function_call`, `tool` → `function_call_output`), `tools`, `tool_choice`,
  `parallel_tool_calls`, `reasoning_effort` → `reasoning{effort, summary:"auto"}`,
  `response_format` → `text.format`.
- `ChatChunks` — stateful Responses SSE → Chat chunk adapter used by `native::normalize`:
  `output_text.delta` → `delta.content`; `reasoning_summary_text.delta`/`reasoning_text.delta`
  → `delta.reasoning_content`; `output_item.added` (function_call) +
  `function_call_arguments.delta` → `delta.tool_calls[i]`; terminal `response.completed`
  → `finish_reason` (`tool_calls` / `length` for `incomplete` max tokens / `stop`) + `usage`
  (`prompt_tokens = input_tokens`, `completion_tokens = output_tokens`, cached tokens in
  details) + `[DONE]`; `response.failed` / `error` → typed failure.
- `aggregate(events) -> response` — non-stream Responses result: the `response` of the first
  `response.completed`/`response.done` (else `incomplete`/`cancelled`); empty `output` rebuilt
  from `output_item.done` items (dedup by id), else from text/argument deltas; `created_at`
  filled when missing.
- `carries_output(event)` — Responses analogue of the Chat check (text/reasoning/tool deltas,
  `output_item.added` of a non-message item, terminal events with output).

### 4. Native dispatch (`src-tauri/src/proxy/upstream/native.rs`)

`Provider::wire()` returns `Wire::Chat` (Qoder) or `Wire::Responses` (ChatGPT).

- Chat wire: unchanged.
- Responses wire, Chat/Messages clients: `chat_codec::request_for` → `responses_wire::from_chat`
  → `chatgpt::payload` → `open()` with a normalizer that runs `ChatChunks` → existing `Relay`.
  `dialect` allows structured output; `representable` accepts every `tool_choice` and
  `parallel_tool_calls` (the Responses wire carries them).
- Responses wire, Responses clients (passthrough): body → `chatgpt::payload` → `open()` with a
  normalizer that yields raw Responses events and `responses_wire::carries_output`. Streaming:
  events are re-emitted as `event: <type>\ndata: <json>` with every `response.model` rewritten
  to the public alias. Non-stream: `responses_wire::aggregate`. Namespaced tool names follow the
  existing `codec::namespaced_tools` mapping.
- `/responses/compact`: for ChatGPT only, the same failover path with a unary JSON exchange;
  other native sources keep the current 400.
- `open()` becomes generic over the per-wire normalizer/output check; failover staging
  (`Reached`, one connection failover, same-account auth retry) is unchanged.
- Probe (`native::probe`) sends a minimal Responses request for ChatGPT and checks for a
  `response.output_text.delta` or a completed response.
- Response headers `x-upstream-provider`, `x-account-email`, `x-mapped-model` as today, so the
  monitor middleware bills and logs without changes (it already reads Responses
  `response.usage` and Chat usage).

### 5. WebSocket relay (`src-tauri/src/proxy/upstream/chatgpt_ws.rs`)

User keys are already rejected at the WebSocket handshake (`user_token_limits`); WebSocket
sessions are master-key only, so no per-turn user billing is introduced.

- `handle_websocket_session` resolves each frame's model with `resolve_public_model` instead of
  `antigravity_model` first. When the first generating `response.create` of a connection
  resolves to `chatgpt:default`, the socket is handed to the ChatGPT relay for its remaining
  lifetime. A frame resolving to ChatGPT after Antigravity turns, or to another source inside a
  ChatGPT session, gets an `error` event; other native sources keep the current error.
- The relay leases an account through the scheduler (sticky for the connection, lease released
  between turns so concurrency counts in-flight turns only), opens the upstream with
  `reqwest-websocket 0.5` on the account's egress `Plain` client (HTTP/1.1 upgrade, works through
  HTTP and SOCKS proxies, pure Rust, MIT), headers as §2 `client.rs` plus
  `OpenAI-Beta: responses_websockets=2026-02-06`.
- Client → upstream: each `response.create` gets `chatgpt::payload` with WebSocket rules
  (`previous_response_id` kept, `background` removed, model rewritten to the leased native id).
  Upstream → client: events relayed with `response.model` rewritten to the public alias.
- Upstream quota headers from the handshake and `usage_limit_reached` errors update the account
  exactly like HTTP. Upstream close/reconnect: up to 3 reconnects with 200 ms–2 s backoff while
  no turn is in flight; a mid-turn drop sends an `error` event and the client retries.
  `previous_response_not_found` is passed to the client (Codex then resends full history).
- Each completed turn records success/failure on the lease and the account runtime counters
  (`successes`, `failures`, `last_error`). Master-key WebSocket turns are not written to the
  monitor log, matching the existing Antigravity WebSocket path.

### 6. UI, headless and docs

- `src/types/nativeProviders.ts`: `NativeProvider = 'qoder' | 'chatgpt'`.
- `src/pages/Accounts.tsx` provider tabs, `NativeAccounts.tsx` (realm selector and Qoder
  client-version panel only for Qoder; ChatGPT import hint: paste a refresh token or
  `{"refresh_token":"..."}`; quota packs shown as 5h / 7d percentage bars),
  `SourceSettings.tsx` + `NativeSourceSettings.tsx` second instance, `registryLogic.ts`
  protocol list, all 12 `src/locales/*.json` (`nativeProviders.*`, provider-specific strings
  parameterized).
- Headless: the existing `/api/native-accounts/*`, `/api/native-providers/*`,
  `/api/native-egress/*` routes take `provider: "chatgpt"` with no change; env vars
  `ABV_CHATGPT_ENABLED`, `ABV_CHATGPT_MAX_CONCURRENCY`, `ABV_CHATGPT_EGRESS_ENABLED`,
  `ABV_CHATGPT_CLIENT_VERSION`.
- Docs: `docs/proxy/native-providers.md` ChatGPT section, `docs/proxy/chatgpt-codex-protocol.md`,
  README feature line (stable release only, per release guide).

## Testing

Mock upstream (local axum server) — no live chatgpt.com calls in tests:

- JWT claim parsing; refresh success, rotation (empty refresh token keeps the old one), dead
  credential errors, configuration errors.
- `payload.rs` rules table-driven (each rule one case), compact and WS variants.
- `responses_wire`: Chat→Responses request; Responses SSE → Chat chunks (text, reasoning, tool
  calls, finish reasons, usage); aggregate (completed, empty output rebuild, failed).
- Quota header window mapping, 429 classification and cooldown, Cloudflare 403 no-penalty.
- `native::route` end-to-end against the mock for Chat, Messages, Responses streaming and
  non-streaming, and compact; existing `Provider::ALL` loop tests cover ChatGPT.
- WebSocket relay against a mock `wss` upstream: model rewrite, `previous_response_id` kept,
  quota headers, mid-turn drop.
- Frontend: `registryLogic.test.mjs` protocol list; `npm run build`.

## Risks and rollback

- Using ChatGPT subscriptions outside Codex may violate OpenAI's terms; accounts can be banned.
  The provider carries no traffic until an administrator imports an account and publishes a
  model from `chatgpt:default`; `ABV_CHATGPT_ENABLED=false` locks it off on headless hosts.
- Upstream identity/version rules change often (404 below the minimum version); the version is
  overridable without a release via `ABV_CHATGPT_CLIENT_VERSION`.
- Shared-layer refactor touches Qoder paths (transport enum, dispatch matches). Qoder behavior
  must stay byte-identical; its existing tests are the guard.
- Rollback: revert the feature commits; vault files gain only defaulted fields, so older builds
  still read Qoder vaults; `chatgpt_accounts.enc` is ignored by older builds.
