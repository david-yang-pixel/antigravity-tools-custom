# ChatGPT Native Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pool ChatGPT subscription accounts (refresh-token import) as the native provider `chatgpt:default`, serving Chat Completions, Responses (passthrough), Messages, `/responses/compact` and the Codex Responses WebSocket.

**Architecture:** A provider-neutral refactor of `modules/native_providers` (plain reqwest transport cache, classified failures, provider dispatch) plus a ChatGPT module that owns the wire facts. A shared `responses_wire` converter bridges Chat-shaped clients to the Responses upstream; Responses clients are passed through. A WebSocket relay holds one upstream `wss` per client connection and replays from a gateway transcript on account switch.

**Tech Stack:** Rust (axum 0.7, reqwest 0.12 rustls, tokio, serde_json, sha2), `reqwest-websocket = "0.5.1"` (new, MIT), React + TypeScript frontend, i18next locales.

**Spec:** `docs/superpowers/specs/2026-11-07-chatgpt-native-provider-design.md`. Protocol facts: `docs/proxy/chatgpt-codex-protocol.md`. Read both before any task.

## Global Constraints

- Clean-room: implement from the spec and protocol doc only. Never open, copy or translate sub2api source.
- Qoder behavior must stay byte-identical. Existing tests under `native_providers` and `proxy::upstream::native` must pass unchanged (only new `Provider::ALL`-driven coverage may run them for ChatGPT too).
- `Provider::ChatGpt`: `key "chatgpt"`, `name "ChatGPT"`, `source "chatgpt:default"`, realm always `Realm::Global`.
- Env vars: `ABV_CHATGPT_ENABLED`, `ABV_CHATGPT_MAX_CONCURRENCY`, `ABV_CHATGPT_EGRESS_ENABLED` (via `Provider::env`), `ABV_CHATGPT_CLIENT_VERSION` (semver ≥ 0.144.0, default `0.146.0`).
- OAuth client id `app_EMoamEEZ73f0CkXaXp7hrann`; token URL `https://auth.openai.com/oauth/token`; backend base `https://chatgpt.com/backend-api`.
- Identity headers on every chatgpt.com request: `originator: codex_cli_rs`, `User-Agent: codex_cli_rs/<ver> (Ubuntu 22.4.0; x86_64) xterm-256color`, `version: <ver>` (auth.openai.com requests omit `version`).
- No live network in tests: every network test targets a local axum mock through the test endpoint override (Task 4).
- Tests: run only targeted modules, single-threaded (`--test-threads=1`, env-mutating tests share `SHARED_TEST_ENV_LOCK`). Do not run fmt/clippy/full suite (AGENTS.md pre-flight is release-only).
- Every commit message references `Ref PR #58` and ends with the session attribution trailers:
  `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` and `Claude-Session: https://claude.ai/code/session_015AKRXENfPH7Seg45S4S4aU`.
- Commands below run from `src-tauri/` unless stated.

## Review Focus

- Upstream SSE frames that carry only `data:` (no `event:` line): the event type must come from the JSON `type` field — tested in Task 3.
- Non-streaming Responses request whose answer is a tool call: the aggregated response must contain the `function_call` item, not an empty output — tested in Task 3.
- Messages client sending an image block: must reach the upstream as `input_image` with a data URL — tested in Task 3.
- Refresh response without `refresh_token`: the stored refresh token must be kept — tested in Task 4.
- Importing a ChatGPT refresh token that is already saved: rejected before any network call — tested in Task 5.

---

### Task 1: Provider-neutral transport cache and classified failures

**Files:**
- Modify: `src/modules/native_providers/http.rs`
- Modify: `src/modules/native_providers/accounts.rs` (`Lease`)
- Test: `src/modules/native_providers/tests.rs`

**Interfaces:**
- Produces (in `http.rs`):
  - `pub enum Penalty { Breaker { retry_after: Option<u64> }, Until { at: i64, message: &'static str }, Neutral, Disable { message: String } }` (`Clone, Debug`)
  - `Failure` gains `pub penalty: Option<Penalty>` and `pub failover: bool`; `Failure::new` sets `None`/`false`; new `pub fn classified(status: u16, message: impl Into<String>, penalty: Penalty, failover: bool) -> Self`.
  - `pub async fn plain_client_for(account: &Account) -> Result<reqwest::Client, Failure>`, `pub fn plain_client_on(account: &Account, exit: &Exit) -> Result<reqwest::Client, Failure>`, `pub fn plain_login_client(provider: Provider, exit: &Exit) -> Result<reqwest::Client, Failure>`; `forget(id)` drops both caches.
- Produces (in `accounts.rs`): `impl Lease { pub fn settle(&mut self, failure: &Failure) }`.

- [ ] **Step 1: Write failing tests** in `tests.rs`:
  - `lease_settle_until_blocks_even_with_breaker_disabled`: acquire a lease for a Qoder test account with the breaker disabled, `settle(&Failure::classified(429, "q", Penalty::Until { at: now() + 3600, message: "Account quota exhausted" }, true))`, assert `accounts::runtime(id).cooldown_until >= now() + 3599` and `last_error == Some("Account quota exhausted")`.
  - `lease_settle_neutral_leaves_counters_untouched`: `Penalty::Neutral` → `failures == 0`, `cooldown_until == 0`.
  - `lease_settle_disable_turns_account_off`: `Penalty::Disable { message: "dead".into() }` → `store::get(..).enabled == false`, `auth_error == Some("dead")`.
  - `lease_settle_without_penalty_matches_failure_after`: `Failure::new(429, ..)` settles exactly like `failure_after(429, None)` (compare runtime snapshots of two accounts).
  - `plain_clients_are_cached_per_account_and_route`: two calls to `plain_client_on(&a, &Exit::Direct)` return without error; `forget(&a.id)` succeeds (cache coverage is structural; assert no panic and `Ok`).
- [ ] **Step 2: Run** `cargo test --locked --lib native_providers::tests::lease_settle -- --test-threads=1` → FAIL (no `settle`/`Penalty`).
- [ ] **Step 3: Implement.** Make the existing cache generic (`fn cached<T: Clone>(map, owner, exit, fingerprint, build)`) and keep Qoder's public functions and fingerprint unchanged. Plain clients: `reqwest::Client::builder().redirect(Policy::none()).no_proxy()` then `.proxy(reqwest::Proxy::all(url))` when `proxy_url(exit)` is non-empty; no overall timeout; fingerprint `(route, node)`. `settle`: `None` → `failure_after(status, None)`; `Breaker` → `failure_after(status, retry_after)`; `Until` → count a failure and set `cooldown_until = max(current, at)` regardless of breaker; `Neutral` → mark finished without counters; `Disable` → `store::update(provider, id, None, |a| { a.enabled = false; a.auth_error = Some(message) })` then count a failure.
- [ ] **Step 4: Run** `cargo test --locked --lib native_providers -- --test-threads=1` → all PASS (existing Qoder tests included).
- [ ] **Step 5: Commit** `refactor(native): share a plain transport cache and classified lease outcomes`.

### Task 2: ChatGPT pure protocol rules (identity, headers, quota, payload, errors)

**Files:**
- Create: `src/modules/native_providers/chatgpt.rs` (module root, declares submodules with `#[path = "chatgpt/<name>.rs"]` like `qoder.rs`)
- Create: `src/modules/native_providers/chatgpt/{identity,headers,quota,payload,errors}.rs`
- Modify: `src/modules/native_providers.rs` (declare `pub mod chatgpt` with `#[path]`; add `Credentials` fields; add the `Provider::ChatGpt` variant with `key/name/source` values from Global Constraints but keep `ALL = [Self::Qoder]` until Task 6; `Account::new` creates machine/device ids only for Qoder)
- Modify: every exhaustive `match` on `Provider` (compiler-listed: `accounts.rs`, `http.rs`, `wire.rs`, `commands/native_providers.rs`, `model_registry_service.rs`, `proxy/upstream/native.rs`) with a ChatGPT arm returning `Failure::new(501, "ChatGPT support is incomplete")` (or the `String` error equivalent); Tasks 5–6 replace them
- Test: `src/modules/native_providers/chatgpt/tests.rs` (`#[cfg(test)] #[path = "chatgpt/tests.rs"] mod tests;` in `chatgpt.rs`)

**Interfaces:**
- Consumes: `Failure`, `Penalty` (Task 1).
- Produces:
  - `Credentials` gains `#[serde(default)] pub id_token: String` and `#[serde(default)] pub chatgpt_account_id: String`.
  - `identity::Claims { email, sub, user_id, account_id, plan: String, exp: i64 }`; `pub fn claims(jwt: &str) -> Option<Claims>`; `pub fn apply(a: &mut Account) -> Result<(), Failure>` — id_token wins, access token fills empty fields; sets `credentials.chatgpt_account_id`, `credentials.user_type = plan`, `nickname = email`, `uid = "<account_id>:<user_id or sub>"`, `expires_at` from access-token `exp` when 0; missing account id → `Failure::new(400, "This ChatGPT token has no chatgpt_account_id claim")`.
  - `headers::DEFAULT_CLIENT_VERSION = "0.146.0"`; `pub fn client_version() -> String`; `pub fn user_agent(version: &str) -> String`; `pub fn apply_identity(h: &mut HeaderMap, with_version: bool)`.
  - `quota::from_headers(h: &HeaderMap, at: i64) -> Option<Quota>` (packs named `"5h"` and `"7d"`, unit `"%"`); `quota::exhausted_until(h: &HeaderMap, body: &Value, at: i64) -> Option<i64>`.
  - `payload::Mode { Http, Compact, WebSocket }`; `payload::DEFAULT_INSTRUCTIONS = "You are a helpful assistant."`; `pub fn prepare(body: Value, model: &str, mode: Mode) -> Result<Value, Failure>`.
  - `errors::classify(status: u16, headers: &HeaderMap, body: &str, at: i64) -> Failure` (always `Failure::classified`); `errors::classify_event(event: &Value, at: i64) -> Failure` for SSE/WS `error` and `response.failed` events.

- [ ] **Step 1: Write failing tests** (table-driven where noted):
  - `claims_prefer_id_token_and_fill_from_access_token`: build unsigned JWTs (`base64url(header).base64url(payload).sig`) with `"https://api.openai.com/auth": {"chatgpt_account_id":"acc","chatgpt_user_id":"u1","chatgpt_plan_type":"plus"}`; assert `uid == "acc:u1"`, `user_type == "plus"`, nickname = id_token email.
  - `claims_without_account_id_is_import_error` → status 400.
  - `client_version_respects_floor`: env `0.150.1` → used; `0.100.0` and `garbage` → `"0.146.0"`.
  - `user_agent_is_canonical`: `user_agent("0.146.0") == "codex_cli_rs/0.146.0 (Ubuntu 22.4.0; x86_64) xterm-256color"`.
  - `quota_windows_map_smaller_to_5h`: primary 10080 min / secondary 300 min → pack `"5h"` from secondary; one window 300 → `"5h"`; no window minutes → primary `"7d"`, secondary `"5h"`; `used == used_percent`, `remaining == 100 - used`, `expires_at == at + reset_after`.
  - `exhausted_until_from_body_or_full_window`: body `{"error":{"type":"usage_limit_reached","resets_in_seconds":"120"}}` → `Some(at+120)`; `resets_at` number wins over seconds; header used 100 % with reset 50 → `Some(at+50)`; used 99 % → `None`.
  - `payload_rules` table: one case per spec §2 payload bullet — forced `stream/store` (Compact deletes both), blank instructions → default, system messages moved (joined `"\n\n"`, prepended), string input wrapped, `include` gains `reasoning.encrypted_content` only with `reasoning`, every removed field gone, `previous_response_id` removed for Http/Compact and kept for WebSocket, `background` removed, `minimal` → `none`, reasoning item loses `id` and gets `summary: []`, replayed item ids dropped, a 70-char `call_id` becomes `call_` + 59 hex chars in both call and output, orphan `function_call_output` removed, Chat-style function tool flattened, replayed `web_search_call` adds the web_search tool and `tool_choice:"none"` when no tools were declared; `model` set to the argument.
  - `classify` table: 429 usage-limit body → `Penalty::Until`, failover true; 429 without reset → `Breaker{retry_after: Retry-After or Some(5)}`, failover true; 402 → `Disable`, failover true; body code `workspace_deactivated` (any status) → `Disable`; 401 `token_revoked` → `Disable`, failover true; other 401 → `Breaker`, failover true; 403 with `cf-mitigated: challenge` or body `Just a moment` → `Neutral`, failover true, client status 502; 403 JSON → `Breaker`, failover true; 500/503/529 → `Breaker { retry_after: Some(5) }`, failover true; 400 `server_is_overloaded` / "selected model is at capacity" → `Neutral`, failover true, status 503; 400 `context_length_exceeded` → penalty `Neutral`, failover false, status 400; other 400 → `Neutral`, failover false.
  - `classify_event_maps_stream_errors`: `{"type":"error","error":{"code":"server_is_overloaded"}}` → failover true; `{"type":"response.failed","response":{"error":{"code":"context_length_exceeded"}}}` → failover false, status 400; `usage_limit_reached` event → `Until`.
- [ ] **Step 2: Run** `cargo test --locked --lib native_providers::chatgpt -- --test-threads=1` → FAIL.
- [ ] **Step 3: Implement** the five files to the spec §2 values. JWT decode: split on `.`, base64url (no pad) decode segment 2, `serde_json`; no signature check. Use `sha2` for call-id hashing.
- [ ] **Step 4: Run** the same command → PASS.
- [ ] **Step 5: Commit** `feat(chatgpt): encode Codex identity, quota, payload and error rules`.

### Task 3: Shared Responses wire converters

**Files:**
- Create: `src/proxy/upstream/responses_wire.rs` (+ `pub mod responses_wire;` in `src/proxy/upstream/mod.rs`)
- Test: `src/proxy/upstream/responses_wire/tests.rs` (`#[cfg(test)] #[path = "responses_wire/tests.rs"] mod tests;`)

**Interfaces:**
- Produces:
  - `pub fn from_chat(chat: Value) -> Result<Value, String>` (input: output of `chat_codec::request_for` + `openai_compat::prepare`).
  - `pub fn event_type(data: &Value) -> Option<&str>` (reads `type`).
  - `pub struct ChatChunks` with `pub fn new() -> Self` and `pub fn push(&mut self, event: &Value) -> Result<Vec<String>, String>` returning Chat chunk JSON strings and, on a terminal success, the finishing chunk, a usage chunk and `"[DONE]"`; terminal failure → `Err(message)` (callers classify the raw event themselves).
  - `pub fn aggregate(events: &[Value]) -> Result<Value, String>`.
  - `pub fn carries_output(event: &Value) -> bool`.

- [ ] **Step 1: Write failing tests:**
  - `from_chat_maps_roles_tools_and_options`: system+user+assistant(tool_calls)+tool messages → `instructions`-candidate system message kept as `role: system` item (Task 2 `prepare` moves it), user `input_text`, assistant `output_text`, `function_call{call_id,name,arguments}`, `function_call_output{call_id,output}`; `tools` flattened Responses tools; `reasoning_effort: "high"` → `reasoning: {effort:"high", summary:"auto"}`; `response_format` → `text.format`; `max_tokens` dropped.
  - `from_chat_maps_image_parts_to_input_image` (Review Focus): user content `[{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}}]` → `{"type":"input_image","image_url":"data:image/png;base64,AAAA"}`.
  - `chat_chunks_text_reasoning_tools_and_usage`: feed `response.created`, `output_text.delta`×2, `reasoning_summary_text.delta`, `output_item.added`(function_call name `f`, call_id `c1`), `function_call_arguments.delta`×2, `response.completed` with usage `{input_tokens:10,output_tokens:5,input_tokens_details:{cached_tokens:3}}`; assert deltas, `tool_calls[0].id == "c1"`, `finish_reason == "tool_calls"`, usage `prompt_tokens 10 / completion_tokens 5 / total 15 / prompt_tokens_details.cached_tokens 3`, last item `"[DONE]"`.
  - `chat_chunks_incomplete_max_tokens_is_length` and `chat_chunks_failed_is_error`.
  - `data_only_frames_use_json_type` (Review Focus): `event_type(&json!({"type":"response.output_text.delta"}))` works without an SSE event name.
  - `aggregate_takes_completed_response`, `aggregate_rebuilds_empty_output_from_item_done` (dedup by id), `aggregate_rebuilds_from_deltas_when_no_items`, `aggregate_tool_call_only_answer` (Review Focus: `stream:false` answer with only a `function_call` item keeps it), `aggregate_failed_is_error`, `aggregate_fills_created_at`.
  - `carries_output_detects_output_events`: deltas and non-message `output_item.added` → true; `response.created`/`in_progress` → false.
- [ ] **Step 2: Run** `cargo test --locked --lib proxy::upstream::responses_wire -- --test-threads=1` → FAIL.
- [ ] **Step 3: Implement** per spec §3. No ChatGPT-specific rule lives here.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(upstream): add shared Responses wire converters`.

### Task 4: ChatGPT network client (endpoints, refresh, models, send)

**Files:**
- Create: `src/modules/native_providers/chatgpt/{endpoints,auth,models,client}.rs`
- Modify: `src/modules/native_providers/chatgpt.rs` (declare), `src/modules/native_providers/http.rs` (`official_url` gains `Provider::ChatGpt => &["chatgpt.com", "openai.com"]` once Task 5 adds the variant — in this task call `endpoints::url()` which validates ChatGPT hosts itself)
- Test: `src/modules/native_providers/chatgpt/tests.rs`

**Interfaces:**
- Consumes: Task 2 modules, `Penalty`/`Failure` (Task 1).
- Produces:
  - `endpoints::Endpoints { pub auth: String, pub backend: String }`; `pub fn current() -> Endpoints` (prod: `https://auth.openai.com`, `https://chatgpt.com/backend-api`); `#[cfg(test)] pub fn set_test(e: Option<Endpoints>)`; `pub fn url(base: &str, path: &str) -> Result<url::Url, Failure>` — https + host `chatgpt.com`/`*.openai.com`/`openai.com` only, except `http://127.0.0.1:*` while a test override is set.
  - `auth::CLIENT_ID`; `pub async fn refresh(client: &reqwest::Client, a: &mut Account) -> Result<(), Failure>` (dead credential → `Failure::classified(401, msg, Penalty::Disable{..}, false)`; config errors → `Failure::new(503, ..)`).
  - `models::fetch(client: &reqwest::Client, a: &Account) -> Result<Vec<Model>, Failure>`; `models::parse(v: &Value) -> Result<Vec<Model>, Failure>`.
  - `client::Endpoint { Responses, Compact }`; `pub fn session_id(scope: &str, account_id: &str, key: &str) -> String` (UUID-formatted SHA-256); `pub fn headers(a: &Account, session: &str, forwarded: &HeaderMap, endpoint: Endpoint) -> Result<HeaderMap, Failure>` (everything `send` sets, reused by the WebSocket relay); `pub async fn send(client: &reqwest::Client, a: &Account, body: Value, endpoint: Endpoint, session: &str, forwarded: &HeaderMap, timeout: Duration) -> Result<reqwest::Response, DispatchFailure>` — sets auth, `chatgpt-account-id`, identity, accept, `session_id`/`conversation_id`, `x-codex-beta-features: remote_compaction_v2` unless forwarded, copies only `accept-language`, `x-codex-turn-state`, `x-codex-turn-metadata`, `x-codex-window-id`, `x-codex-installation-id`; records `quota::from_headers` on every answer via `quota::record(a, headers)` (throttled 30 s per account, `store::update` of `quota` only).
  - `quota::record(a: &Account, h: &HeaderMap)` (added to `quota.rs`).

- [ ] **Step 1: Write failing tests** with a local axum mock bound to `127.0.0.1:0` and `endpoints::set_test`:
  - `refresh_rotates_and_parses_identity`: mock returns `access_token`, `id_token`, `expires_in: 3600`, `refresh_token: "r2"`; assert form fields (`grant_type=refresh_token`, `client_id`, `scope=openid profile email`), headers `originator` present and `version` absent; account gets `r2`, identity applied, `expires_at ≈ now+3600`.
  - `refresh_without_refresh_token_keeps_old_one` (Review Focus).
  - `refresh_invalid_grant_disables` → `Penalty::Disable`; `refresh_invalid_client_is_config_error` → status 503, no penalty.
  - `models_skip_hidden_and_wildcard`: response with `visibility:"hide"` and slug `gpt-*` skipped; `context_window` and `input_modalities` containing `image` → `vision: true`; request carries `client_version` query and `version` header.
  - `send_sets_headers_and_filters_forwarded`: mock echoes headers; forwarded `authorization`, `cookie`, `x-forwarded-for`, `user-agent` dropped; `x-codex-turn-state` kept; `chatgpt-account-id` = account claim; compact uses `/codex/responses/compact` and `accept: application/json`.
  - `send_records_quota_headers`: response headers with primary/secondary → stored account `quota.packs` has `5h` and `7d`.
  - `url_rejects_foreign_hosts`: `https://evil.com/x` → 400; `http://chatgpt.com` → 400.
- [ ] **Step 2: Run** `cargo test --locked --lib native_providers::chatgpt -- --test-threads=1` → FAIL.
- [ ] **Step 3: Implement.** Bound response bodies read for JSON to 32 MiB.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `feat(chatgpt): refresh tokens, fetch models and send Codex requests`.

### Task 5: Register `Provider::ChatGpt` across the shared layer, commands and registry

**Files:**
- Modify: `src/modules/native_providers.rs` (variant, `ALL: [Self; 2]`, `Account::new` creates machine/device ids only for Qoder, `can_refresh` unchanged)
- Modify: `src/modules/native_providers/{accounts.rs,login.rs,http.rs,wire.rs}`
- Modify: `src/commands/native_providers.rs`, `src/modules/model_registry_service.rs`, `src/modules/model_registry.rs` (realm validation for ChatGPT accepts/ignores realm). `proxy/upstream/native.rs` keeps its Task 2 stubs until Task 6.
- Test: `src/modules/native_providers/tests.rs` (Sandbox clears `ABV_CHATGPT_ENABLED`, `ABV_CHATGPT_MAX_CONCURRENCY`, `ABV_CHATGPT_EGRESS_ENABLED`, `ABV_CHATGPT_CLIENT_VERSION`)

**Interfaces:**
- Consumes: Tasks 1, 2, 4.
- Produces: `accounts::fetch_metadata` returns `(Option<Result<Vec<Model>, Failure>>, Option<Result<Quota, Failure>>)` (`None` quota = leave stored quota untouched; ChatGPT returns `None`, Qoder `Some(..)`); `login::import(Provider::ChatGpt, _, value)` accepts a JSON string or `{"refresh_token": ".."}` (also `refreshToken`, `rt`), forces `Realm::Global`, always refreshes first, applies identity; `ImportRequest.realm` becomes `#[serde(default)]` (default `Cn` keeps Qoder's current default).
- Dead-credential refresh failures (`Penalty::Disable`) in `accounts::refresh_owned` set `enabled = false` and `auth_error`.

- [ ] **Step 1: Write failing tests:**
  - `chatgpt_import_accepts_bare_and_object_refresh_tokens` (mock token endpoint via `endpoints::set_test`): both inputs create one account with realm Global, `uid == "acc:u1"`, `credentials.refresh_token` rotated value.
  - `chatgpt_import_rejects_saved_refresh_token_before_network` (Review Focus): second import of the same token fails with the existing "already saved" message and the mock records exactly one token request.
  - `chatgpt_dead_refresh_disables_account`: `accounts::fresh(ChatGpt, id, true)` against an `invalid_grant` mock → `enabled == false`, `auth_error` set.
  - `chatgpt_sync_keeps_quota_and_updates_models`: stored quota survives `accounts::sync`; models replaced from the mock.
- [ ] **Step 2: Run** `cargo test --locked --lib native_providers -- --test-threads=1` → new tests FAIL.
- [ ] **Step 3: Implement** (replacing the Task 2 stubs) every `match` arm listed in spec §1: `fresh`, `fetch_metadata`, `official_url`, `wire::event` (ChatGPT never reaches it: return `Err(Failure::new(500, ..))`), `login::import` (ChatGPT branch: no Qoder identity call, no machine/device id carry-over), `check_native_client_version` (ChatGPT → `Err("Client version check is not available for ChatGPT")`), `delete_native_account` (`qoder::sign::forget` only for Qoder), `test_native_account_egress` (ChatGPT URL `https://chatgpt.com/` via `plain_client_on`), `discover_native_account` (ChatGPT → `chatgpt::models::fetch` with `plain_client_for`).
- [ ] **Step 4: Run** `cargo test --locked --lib native_providers -- --test-threads=1` and `cargo test --locked --lib model_registry -- --test-threads=1` → PASS.
- [ ] **Step 5: Commit** `feat(chatgpt): register ChatGPT as a native provider with refresh-token import`.

### Task 6: Native dispatch for ChatGPT (bridging, passthrough, compact, failover, probe)

**Files:**
- Modify: `src/proxy/upstream/native.rs`
- Create: `src/proxy/upstream/responses_relay.rs` (+ `pub(crate) mod responses_relay;`)
- Test: `src/proxy/upstream/native.rs` test module (existing at the bottom) and `src/modules/native_providers/tests.rs` (route end-to-end)

**Interfaces:**
- Consumes: Tasks 1–5 (`chatgpt::payload::prepare`, `chatgpt::client::{send, session_id, Endpoint}`, `chatgpt::errors::{classify, classify_event}`, `responses_wire::*`, `Lease::settle`, `http::plain_client_for`).
- Produces:
  - `enum WireMode { Chat, ResponsesToChat, Passthrough, Unary }` selecting normalizer and output check inside `open()`; `open()` gains `mode: WireMode` and `forwarded: &HeaderMap`, returns the upstream response headers alongside events so `x-codex-turn-state` reaches the client.
  - `pub(crate) struct ResponsesRelay { pub alias: String, pub headers: HeaderMap, pub error_code: &'static str }` with `pub async fn run(self, events: Events, streaming: bool) -> Response` — streaming re-emits `event: <type>\ndata: <json>\n\n` with `response.model` (and top-level `model`) set to `alias`; a post-output `Err` becomes a final `response.failed` event; non-streaming uses `responses_wire::aggregate`.
  - Failover in `open()`: a `Failure` with `failover == true` may move to another account (ChatGPT cap: 3 accounts per request, within the deadline); without the flag the existing `Reached` rules apply unchanged; every failed attempt calls `lease.settle(&failure)`; pre-output stream failures from the ChatGPT normalizers come from `classify_event` and follow the same rule.

- [ ] **Step 1: Write failing end-to-end tests** (mock upstream + `endpoints::set_test`, publish `chatgpt-chat` → `chatgpt:default`/`gpt-test`, request through the middleware stack like the existing `Provider::ALL` route test):
  - `chatgpt_chat_client_streams_and_bills`: `/v1/chat/completions` stream → Chat chunks with text, `x-upstream-provider: chatgpt`, upstream body has `store:false`, `stream:true`, non-empty `instructions`, no `max_tokens`.
  - `chatgpt_messages_client_non_stream`: `/v1/messages` `stream:false` → Messages JSON text.
  - `chatgpt_responses_passthrough_streams_raw_events`: `/v1/responses` stream → client receives the mock's events in order with `response.model == "chatgpt-chat"`, a replayed `reasoning` item with `encrypted_content` reaches the upstream intact (minus `id`).
  - `chatgpt_responses_non_stream_aggregates`.
  - `chatgpt_compact_returns_upstream_json`: `/responses/compact` → upstream path `/codex/responses/compact`, body without `store`/`stream`; Qoder sources still get 400 on compact.
  - `chatgpt_fails_over_429_before_output`: two accounts; first answers 429 `usage_limit_reached` (`resets_in_seconds: 600`) → request served by the second; first account `cooldown_until ≥ now+599`.
  - `chatgpt_fails_over_pre_output_overloaded_event`: first account streams `{"type":"error","error":{"code":"server_is_overloaded"}}` before output → second account serves.
  - `chatgpt_stops_after_three_accounts`: four accounts all 503 → client 503-mapped error; mock saw exactly 3 requests.
  - `chatgpt_context_length_is_not_replayed`: 400 `context_length_exceeded` → client 400, one upstream request.
  - `chatgpt_post_output_failure_is_not_replayed`: text delta then `response.failed` → client stream ends with `response.failed`, one upstream request.
  - `chatgpt_turn_state_header_returned`.
  - `chatgpt_probe_verifies_text`: `native::probe(ChatGpt, ..)` with mock text → `"verified"`.
- [ ] **Step 2: Run** `cargo test --locked --lib chatgpt_ -- --test-threads=1` → FAIL.
- [ ] **Step 3: Implement.** Chat/Messages: `request_for` → `openai_compat::prepare` → `responses_wire::from_chat` → `payload::prepare(.., Mode::Http)` → `open(.., WireMode::ResponsesToChat)` → existing `Relay`. Responses: body → `payload::prepare(.., Mode::Http)` → `open(.., WireMode::Passthrough)` → `ResponsesRelay`. Compact (`/responses/compact`, ChatGPT only): `Mode::Compact`, `WireMode::Unary` (normalizer reads ≤ 32 MiB JSON, yields it once then `"[DONE]"`), JSON response with `model` set to the alias. `dialect(ChatGpt)` → `structured_output: true`; `representable` returns `Ok` for ChatGPT before the Qoder checks. Non-2xx ChatGPT answers: read ≤ 1 MiB body, `classify`; Qoder answers keep today's path. Session id: `client::session_id(scope, account.credentials.chatgpt_account_id, prompt_cache_key or session key)`. Probe: minimal Responses body, success = text delta + completed. Extend `Provider::ALL` to `[Self::Qoder, Self::ChatGpt]` now that dispatch works; existing `Provider::ALL`-driven tests then cover ChatGPT and must pass.
- [ ] **Step 4: Run** `cargo test --locked --lib native_providers -- --test-threads=1` and `cargo test --locked --lib proxy::upstream -- --test-threads=1` → PASS (existing Qoder failover tests unchanged).
- [ ] **Step 5: Commit** `feat(chatgpt): serve Chat, Messages, Responses passthrough and compact with account failover`.

### Task 7: ChatGPT WebSocket relay with transcript replay

**Files:**
- Modify: `src-tauri/Cargo.toml` (`reqwest-websocket = "0.5.1"`), `Cargo.lock`
- Create: `src/proxy/upstream/chatgpt_ws.rs`, `src/proxy/upstream/chatgpt_ws/transcript.rs`, `src/proxy/upstream/chatgpt_ws/tests.rs`
- Modify: `src/proxy/upstream/mod.rs`, `src/proxy/handlers/openai.rs` (`handle_websocket_session`)

**Interfaces:**
- Consumes: Tasks 1–6 (`payload::prepare(.., Mode::WebSocket)`, `client::headers` (Task 4), `errors::classify_event`, `quota::record`, scheduler `acquire`, `Lease::settle`, `endpoints::url`).
- Produces:
  - `transcript::Transcript` with `pub fn new(limit_bytes: usize) -> Self`, `record_request(&mut self, input: &[Value])`, `record_response(&mut self, response_id: &str, output: &[Value])`, `rebuild(&self, previous_response_id: &str, new_input: &[Value]) -> Rebuild`, `reject(&mut self, items: &[Value])`, `strip_rejected(&self, items: &mut Vec<Value>)`; `pub enum Rebuild { Full(Vec<Value>), Unknown, Orphaned, Dropped }`. Limit constant `TRANSCRIPT_LIMIT = 32 * 1024 * 1024`; rejected digests capped at 512.
  - `pub async fn run(socket: axum::extract::ws::WebSocket, first: Value, published: PublishedModel, state: AppState)`.
  - `handle_websocket_session`: before the existing Antigravity checks, resolve the frame model with `resolve_public_model`; first generating `response.create` resolving to `chatgpt:default` hands the socket to `chatgpt_ws::run`; a later ChatGPT frame in an Antigravity session → `error` event.

- [ ] **Step 1: Write failing tests:**
  - Transcript: `rebuild_includes_history_up_to_reference_plus_new_input`; `rebuild_strips_encrypted_reasoning_and_compaction`; `rebuild_unknown_id_is_unknown`; `rebuild_orphaned_output_is_refused`; `memory_bound_drops_oldest_and_reports_dropped`; `rejected_digests_strip_matching_items_and_cap_at_512`.
  - Relay against a mock `wss` upstream (axum ws server on `127.0.0.1:0` via `endpoints::set_test`): `ws_relays_turns_and_rewrites_model`; `ws_keeps_previous_response_id_on_same_account`; `ws_switch_rebuilds_full_input_without_previous_id` (first account's handshake returns 429 on turn 2 → second account receives full input, no `previous_response_id`, no `encrypted_content`); `ws_unknown_previous_id_forwarded_unchanged`; `ws_previous_response_not_found_replays_once`; `ws_invalid_encrypted_content_strips_and_retries_once`; `ws_idle_socket_pings_before_turn` (mock records ping after 20 s using `tokio::time::pause`); `ws_client_idle_closes_after_300s`; `ws_handshake_quota_headers_recorded`; `ws_antigravity_session_rejects_chatgpt_frame`.
- [ ] **Step 2: Run** `cargo test --locked --lib chatgpt_ws -- --test-threads=1` → FAIL.
- [ ] **Step 3: Implement** per spec §5: per-turn lease (sticky session key `ws:<connection uuid>`), upstream socket per bound account opened with `reqwest_websocket::RequestBuilderExt::upgrade` on `plain_client_for(account)` plus `OpenAI-Beta: responses_websockets=2026-02-06`; reconnect ≤ 3 times (200 ms → 2 s) while idle; failover ≤ 3 accounts per turn using `Failure::failover`; ping after ≥ 20 s idle with 5 s pong deadline; 300 s client idle close (code 1000).
- [ ] **Step 4: Run** `cargo test --locked --lib chatgpt_ws -- --test-threads=1` and `cargo test --locked --lib handlers::openai -- --test-threads=1` → PASS.
- [ ] **Step 5: Commit** `feat(chatgpt): relay Codex WebSocket sessions with transcript replay on account switch`.

### Task 8: Frontend, locales and docs

**Files:**
- Modify: `src/types/nativeProviders.ts`, `src/pages/Accounts.tsx`, `src/components/accounts/NativeAccounts.tsx`, `src/components/proxy/models/NativeSourceSettings.tsx`, `src/components/proxy/models/SourceSettings.tsx`, `src/components/proxy/models/registryLogic.ts`, `src/components/proxy/models/registryLogic.test.mjs`, `src/locales/*.json` (12 files)
- Modify: `docs/proxy/native-providers.md`
(paths relative to repo root)

**Interfaces:**
- Consumes: backend `provider: "chatgpt"` on existing commands/routes.
- Produces: `NativeProvider = 'qoder' | 'chatgpt'`; i18n keys `nativeProviders.chatgpt.importHint`, `nativeProviders.chatgpt.importPlaceholder`, `nativeProviders.quota.window5h`, `nativeProviders.quota.window7d`; Qoder-specific strings (`clientVersion.title`, scheduling subtitles) parameterized by `{{provider}}` where they apply to both.

- [ ] **Step 1: Write failing test** in `registryLogic.test.mjs`: protocol list includes `'chatgpt'`; `UpstreamModels` realm check stays `['workbuddy','qoder']` (assert ChatGPT has no realm).
- [ ] **Step 2: Run** (repo root) `node --test src/components/proxy/models/registryLogic.test.mjs` → FAIL.
- [ ] **Step 3: Implement.** ChatGPT tab in Accounts; `NativeAccounts` hides realm selector and Qoder client-version panel for ChatGPT, import textarea placeholder `{"refresh_token":"..."}` or bare token, quota packs rendered as 5h / 7d percentage bars with reset time; second `NativeSourceSettings provider="chatgpt"` in `SourceSettings`; all 12 locales get the new keys (English source text, translated for zh/zh-TW/ja/ko, English fallback acceptable for the rest); docs section: import, env vars, failover policy, WebSocket behavior, risk note.
- [ ] **Step 4: Run** `node --test src/components/proxy/models/registryLogic.test.mjs` → PASS; `npm run build` → exit 0.
- [ ] **Step 5: Commit** `feat(ui): manage ChatGPT accounts and sources`.

---

## Final verification (after Task 8)

- [ ] `cd src-tauri && cargo test --locked --lib native_providers -- --test-threads=1`
- [ ] `cd src-tauri && cargo test --locked --lib proxy::upstream -- --test-threads=1`
- [ ] `cd src-tauri && cargo test --locked --lib chatgpt -- --test-threads=1`
- [ ] `npm run build`
- [ ] Whole-branch review against the spec, then push `claude/zealous-clarke-t8m5jo`.
