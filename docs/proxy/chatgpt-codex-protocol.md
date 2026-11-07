# ChatGPT subscription ("Codex OAuth") upstream: protocol facts observed in sub2api

Scope: wire-level behavior that sub2api (Go, LGPL-3.0, source references are to
its repository) shows when it uses ChatGPT subscription accounts
(OpenAI OAuth / Codex accounts) as an upstream. This is a factual protocol description for a
clean-room reimplementation. It does not translate their code. All file references are relative to
`backend/internal/`. "UNCONFIRMED" marks anything the code does not settle.

Note: the snapshot's static model lists include names such as gpt-5.5, gpt-5.6-*, gpt-6-* and
gpt-6.1-*. They are copied here as found. Treat them as point-in-time data and do not hard-code them.

---

## 1. OAuth login (authorization-code + PKCE)

| Item | Value | Ref |
|---|---|---|
| Client ID | `app_EMoamEEZ73f0CkXaXp7hrann` (the Codex CLI public client) | pkg/openai/oauth.go:19 |
| Authorize URL | `https://auth.openai.com/oauth/authorize` | pkg/openai/oauth.go:22 |
| Token URL | `https://auth.openai.com/oauth/token` | pkg/openai/oauth.go:23 |
| Default redirect URI | `http://localhost:1455/auth/callback` (port 1455, path `/auth/callback`) | pkg/openai/oauth.go:26 |
| Authorize scopes | `openid profile email offline_access` | pkg/openai/oauth.go:29 |
| Refresh scopes | `openid profile email` (no offline_access) | pkg/openai/oauth.go:31 |
| PKCE method | `S256` | pkg/openai/oauth.go:199 |
| code_verifier | 64 random bytes, **hex-encoded** (128 hex chars), not base64url | pkg/openai/oauth.go:155-163 |
| code_challenge | base64url(SHA-256(verifier ASCII)), padding stripped | pkg/openai/oauth.go:165-177 |
| state | 32 random bytes, hex | pkg/openai/oauth.go:137-144 |
| Pending-login TTL (local) | 30 min | pkg/openai/oauth.go:34 |

Authorize query parameters (pkg/openai/oauth.go:192-205):
```
response_type=code
client_id=app_EMoamEEZ73f0CkXaXp7hrann
redirect_uri=<redirect>
scope=openid profile email offline_access
state=<state>
code_challenge=<challenge>
code_challenge_method=S256
id_token_add_organizations=true
codex_cli_simplified_flow=true
```

The gateway does not listen on the redirect port. The admin opens the URL in a browser, the
redirect to localhost:1455 fails, and the admin pastes the `code` and `state` from the address bar
back into the UI. The state is compared in constant time (service/openai_oauth_service.go:139-144).

**Code exchange** (repository/openai_oauth_service.go:26-70):
- `POST https://auth.openai.com/oauth/token` with `Content-Type: application/x-www-form-urlencoded`
- Form fields: `grant_type=authorization_code`, `client_id`, `code`, `redirect_uri`
  (the same one used to authorize), `code_verifier`.
- Extra headers: `User-Agent: <canonical Codex UA>` and `originator: codex_cli_rs` (see §4).
  The `version` header is **not** sent on auth.openai.com (service/openai_codex_identity.go:86-94).
- HTTP timeout 120 s. A configured per-account proxy is used. If no proxy is set and the request
  fails at transport level, the user is told to configure one (repository/openai_oauth_service.go:122-144).
- Response JSON: `access_token`, `id_token`, `token_type`, `expires_in` (seconds), `refresh_token`,
  `scope` (pkg/openai/oauth.go:224-231). Stored as `expires_at = now + expires_in`.

**Post-login enrichment (optional, best effort)**, in service/openai_oauth_service.go:260-308 and
service/openai_privacy_service.go. All calls go to chatgpt.com through a client with a browser TLS
fingerprint (Firefox impersonation; Chrome impersonation got Cloudflare challenges, see
repository/req_client_pool.go:20,53-59). Headers: `Authorization: Bearer <access>`,
`Origin: https://chatgpt.com`, `Referer: https://chatgpt.com/`, `Accept: application/json`.
- `GET https://chatgpt.com/backend-api/accounts/check/v4-2023-04-27` returns
  `accounts{<id>: {account:{is_default,...}, entitlement:{expires_at}, ...}}`. Used for plan_type
  and subscription expiry. The entry keyed by the access-token `poid` claim is preferred; otherwise
  the order is default, then non-free, then any. `chatgpt_plan_type` from the id_token wins over
  this endpoint.
- `GET https://chatgpt.com/backend-api/subscriptions?account_id=<chatgpt_account_id>` returns
  `{plan_type, active_until, will_renew, id}`.
- `PATCH https://chatgpt.com/backend-api/settings/account_user_setting?feature=training_allowed&value=false`
  turns off training-data sharing. A 403/503 with `cf-mitigated: challenge` or a body containing
  "Just a moment"/"cloudflare" counts as a Cloudflare block.

**Alternative credential imports**:
1. **Refresh token only** (`refresh_token` or `rt`, optional `client_id`): the token is refreshed
   immediately to mint an access token (handler/admin/openai_oauth_handler.go:197-249).
2. **Codex session / auth.json import** (handler/admin/account_codex_import.go:383-660). Input can
   be one JSON object, a JSON array, NDJSON, or bare lines (each bare line is an access token).
   Recognized keys, first match wins:
   - access token: `tokens.access_token`, `tokens.accessToken`, `access_token`, `accessToken`, `token`
   - refresh token: `tokens.refresh_token`, `tokens.refreshToken`, `refresh_token`, `refreshToken`
   - id token: `tokens.id_token`, `tokens.idToken`, `id_token`, `idToken`
   - account id: `chatgpt_account_id`, `chatgptAccountId`, `account_id`, `accountId`, `account.id`, ...
   - user id, plan_type, organization_id, email, name; expiry: `tokens.expires_at`, `expires_at`, ...
   - A ChatGPT web `sessionToken` / `session_token` is **ignored**. It is not stored as a refresh token.
   - An access token that already expired (120 s skew) is rejected. `expires_at` falls back to the
     JWT `exp`. When a refresh token is present, `client_id` is set to the Codex client ID.
     Identity fields are backfilled from JWT claims (§3).
3. **Codex personal access token** (prefix `at-`) (service/openai_codex_pat_service.go):
   validated with `GET https://auth.openai.com/api/accounts/v1/user-auth-credential/whoami`
   (`authorization: Bearer at-...`, `accept: application/json`, canonical UA and originator).
   The response must contain `email`, `chatgpt_user_id`, `chatgpt_account_id`,
   `chatgpt_plan_type`, `chatgpt_account_is_fedramp` (bool). 401/403 means invalid. A PAT has no
   refresh lifecycle: refresh/id_token/expiry fields are removed and the PAT is sent as a Bearer token.
4. **Agent Identity** (optional, advanced): Ed25519 runtime key. The Authorization header becomes
   `AgentAssertion base64url(JSON{agent_runtime_id, task_id, timestamp(RFC3339), signature})`. The
   signature covers `runtime_id:task_id:timestamp`. Tasks are registered at
   `https://auth.openai.com/api/accounts/v1/agent/<runtime_id>/task/register`
   (service/openai_agent_identity.go:26,115-131,203). Skip unless needed.

## 2. Token refresh

- Endpoint: `POST https://auth.openai.com/oauth/token`, form-encoded:
  `grant_type=refresh_token`, `refresh_token`, `client_id` (the stored one, default Codex ID),
  `scope=openid profile email`. Headers: canonical `User-Agent` and `originator`, no `version`
  (repository/openai_oauth_service.go:85-120).
- Response has the same shape as the code exchange. The new `refresh_token` replaces the stored
  one **only when it is non-empty** (rotation is supported, and an empty value never overwrites)
  (service/openai_oauth_service.go:400-403). Credentials are merged: fields that are missing in
  the new set are kept (service/oauth_refresh_api.go MergeCredentials). The id_token is parsed
  again and identity fields are refreshed. Enrichment (§1) runs again.
- When it refreshes:
  - On the request path, if `expires_at` is missing or within **3 min** of now
    (service/openai_token_provider.go:14,158). The cached access-token TTL is
    `expires_at - 5 min`; after a failed refresh the TTL is 1 min (openai_token_provider.go:15,242-262).
  - A background job checks every 5 min and refreshes when less than **30 min** remain
    (`token_refresh.check_interval_minutes=5`, `refresh_before_expiry_hours=0.5`, max 3 retries
    with 2 s base backoff; config/config.go:2677-2687; service/token_refresher.go:103-116).
  - A distributed lock serializes refreshes per account. Waiters poll the cache with backoff from
    20 to 120 ms, 5 attempts.
- Refresh race recovery: when a refresh fails with `invalid_grant`, the account is read again. If
  its stored refresh_token has changed in the meantime, another worker already rotated it and the
  result counts as success (service/oauth_refresh_api.go:263-283,416-437).
- **Permanently dead (account set to error, no retries)**. The error text contains any of
  (service/token_refresh_service.go:1409-1439): `invalid_grant`, `invalid_refresh_token`,
  `token_expired`, `app_session_terminated` (team workspace deleted), `refresh_token_reused`,
  `refresh_token_invalidated`, `invalid_client`, `unauthorized_client`, `access_denied`,
  `no refresh token available`, `invalid_scope`, `unknown scope`.
  Exception: `invalid_client`, `unauthorized_client`, `invalid_scope` and `unknown scope` are first
  treated as provider-wide configuration errors, and the whole refresh cycle is paused instead of
  disabling accounts (token_refresh_service.go:983-986, `isSharedProviderRefreshError`).
- If the access token has expired and no refresh token exists, the account is disabled immediately
  (openai_token_provider.go:159-167).

## 3. Identity (JWT claims)

The JWT payload is decoded without verifying the signature. OpenAI-specific claims are nested under
the key `"https://api.openai.com/auth"` (pkg/openai/oauth.go:241-272):

| Claim | Source token | Use |
|---|---|---|
| `email` (top level) | id_token | display / dedup |
| `sub` | either | fallback user id |
| `exp` | either | expiry (120 s skew when parsing the id_token) |
| `…/auth.chatgpt_account_id` | id_token (also access_token) | **required** for the `chatgpt-account-id` header |
| `…/auth.chatgpt_user_id` | id_token | identity namespace |
| `…/auth.chatgpt_plan_type` | id_token / access_token | plan (free/plus/pro/team/...) |
| `…/auth.user_id` | id_token | fallback user id |
| `…/auth.poid` | **access_token** | organization id used to pick the right workspace in accounts/check |
| `…/auth.organizations[] {id, role, title, is_default}` | id_token (present because of `id_token_add_organizations=true`) | org id = the default org, else the first |

FedRAMP flag (`chatgpt_account_is_fedramp`) comes from PAT whoami or Agent Identity import. When it
is set, the request carries `x-openai-fedramp: true`.

## 4. Inference request

**URL**: `POST https://chatgpt.com/backend-api/codex/responses`
(service/openai_gateway_service.go:31). Sub-paths are appended to that base URL, for example
`/compact` becomes `…/codex/responses/compact` and `/input_tokens` is handled separately
(service/openai_gateway_request_body.go:925-977). The Host header is `chatgpt.com`
(openai_gateway_forward.go:1457).

**Headers the gateway sets** (openai_gateway_forward.go:1399-1563, openai_codex_identity.go, openai_chatgpt_headers.go):

| Header | Value |
|---|---|
| `Authorization` | `Bearer <access_token>` |
| `chatgpt-account-id` | `<chatgpt_account_id>` |
| `x-openai-fedramp` | `true` only for FedRAMP accounts, otherwise removed |
| `originator` | `codex_cli_rs` by default (identity enforcement, see below) |
| `User-Agent` | `codex_cli_rs/<ver> (Ubuntu 22.4.0; x86_64) xterm-256color` |
| `version` | `<ver>`. The built-in default is `0.146.0`. The upstream returns **404 below 0.144.0** (openai_codex_identity.go:16; openai_gateway_service.go:40-64) |
| `accept` | `text/event-stream`; `application/json` for `/responses/compact` |
| `content-type` | `application/json` |
| `session_id` | derived from `prompt_cache_key` (see below) |
| `conversation_id` | same derived value as `session_id`, set when a prompt_cache_key exists |
| `x-codex-beta-features` | `remote_compaction_v2` when the client sent none (openai_compact_body_signal.go:17,160-174) |
| `x-codex-routing-hint` | `model=<final slug>` plus `;tier=priority|flex|ultrafast` when that tier applies. Any client-supplied value is always removed first (openai_routing_hint.go:14-59) |
| `OpenAI-Beta` | the client value with `responses=experimental` **removed** (openai_gateway_passthrough.go:708-737). The usage-probe request (§7) sends `OpenAI-Beta: responses=experimental` instead |

Identity rules (openai_codex_identity.go, pkg/openai/request.go):
- `originator` must match the UA product token (the text before the first `/`). A mismatch, such
  as originator `codex_cli_rs` with a `codex-tui/...` UA, gets **404** from `/backend-api/codex`
  (request.go:191-201).
- The `version` header must equal the UA version segment and be at least 0.144.0.
- Enforcement is on by default: whatever the client claims, UA, originator and version are
  rewritten to the canonical CLI identity. The stated reason is that the upstream risk-tiers client
  identities, and stale or third-party identities get HTTP 200 followed by an in-stream
  `server_is_overloaded` error. An admin-set per-account UA only contributes OS/arch/terminal text;
  the version is always rebuilt (openai_codex_identity.go:44-53,140-165,211-237).
  `codex-tui` identities are normalized to `codex_cli_rs` (request.go:273-298).
- The canonical version is chosen in this order: admin setting, then auto-sync from GitHub
  `openai/codex` releases (tag prefix `rust-v`, every 6 h), then the built-in constant
  (service/openai_codex_version_sync_service.go:12-25).
- Known official originators: `codex_cli_rs`, `codex-tui`, `codex_vscode`, `codex_vscode_copilot`,
  `codex_app`, `codex_chatgpt_desktop`, `codex_atlas`, `codex_exec`, `codex_sdk_ts`, plus any name
  starting with `Codex ` (request.go:50-60). This list is used to detect official clients.

Client headers: the gateway uses an **allow-list**. Only these are copied from the client
(openai_gateway_service.go:74-88): `accept-language`, `content-type`, `conversation_id`,
`openai-beta`, `user-agent`, `originator`, `session_id`, `x-codex-beta-features`,
`x-codex-installation-id`, `x-codex-turn-state`, `x-codex-turn-metadata`, `x-codex-window-id`,
`x-openai-internal-codex-responses-lite`. Everything else is dropped, including the client
Authorization/x-api-key, cookies and x-forwarded-*. Afterwards the client's `session_id` and
`conversation_id` are deleted and rebuilt, and UA, originator and version are overwritten as
described above. In optional "passthrough" mode the allow-list also includes `accept`
(openai_gateway_service.go:92-107).

session_id / conversation_id derivation: the client's `prompt_cache_key` is hashed together with
the downstream API-key id and a credential namespace (`chatgpt:<account_id>[:user:<user_id>]`), as
`hex(sha256("u<apiKeyID>:a<namespace>:<raw>")[:8])`. This keeps two downstream users, or two
upstream accounts after a failover, from sharing an upstream session
(service/openai_codex_account_identity.go:55-91). The Chat Completions path instead sets
`session_id` to a UUIDv4-shaped value built from sha256(seed)
(openai_gateway_chat_completions.go:390-397). For a clean-room design, a stable per-conversation
id that is scoped to the account and tenant is enough. The exact hash does not matter to the upstream.

Additional identity scoping (optional): ids inside `client_metadata` (`installation_id`,
`x-codex-installation-id`, `session_id`, `thread_id`, `turn_id`, `window_id`,
`x-client-request-id`) and `prompt_cache_key` are rewritten into account-scoped UUIDs. If the
account has a stored device id, `client_metadata["x-codex-installation-id"]` is added
(openai_codex_transform.go:1468-1503). An optional "fingerprint convergence" mode collapses
installation, session and thread ids per account (openai_codex_fingerprint.go). It is off by default.

## 5. Payload rules for `/backend-api/codex/responses`

Applied to every OAuth request after the client protocol has been converted to Responses
(service/openai_codex_transform.go:154-361; openai_gateway_forward.go:398-760).

Forced fields:
- `stream: true` and `store: false`. Both are overwritten even when the client set them; the
  upstream otherwise replies "Store must be set to false". On `/responses/compact`, `store` and
  `stream` are deleted instead (transform.go:198-218).
- `instructions` must be a non-empty string. When it is missing or blank, the gateway inserts the
  model-specific Codex base prompt that ships with the Codex CLI (pkg/openai/constants.go:63-183;
  `instructions*.txt`). Choice: names containing `codex` get the GPT-5-Codex prompt; gpt-5.1,
  gpt-5.2 and gpt-5.5 each get their own prompt; anything else gets the newest (5.5); the
  hard fallback is "You are a helpful coding assistant." On the Chat-Completions bridge,
  instructions may be the empty string `""`, but the field must exist
  (openai_gateway_messages.go:566-577). A 400 saying "instructions are required" is recognized
  (openai_gateway_upstream_errors.go:67-119).
- Any `role:"system"` input message is rejected by the upstream. Its text is moved into
  `instructions` (prepended, joined with blank lines). The message is either removed (when the
  text could be moved without loss) or turned into `role:"developer"`. In JSON-object mode it is
  kept as developer (transform.go:1318-1373).
- When a `reasoning` object exists, `include` gets `"reasoning.encrypted_content"` appended
  (transform.go:1435-1466). Chat→Responses conversion always adds it.
- A string `input` becomes `[{"type":"message","role":"user","content":"<s>"}]`. An empty string
  becomes `[]` (transform.go:334-350).
- `parallel_tool_calls` is **not** forced. It passes through when the client set it.

Removed fields (OAuth only; transform.go:154-172):
`max_output_tokens`, `max_completion_tokens`, `temperature`, `top_p`, `frequency_penalty`,
`presence_penalty`, `chat_template_kwargs`, `user`, `metadata`, `prompt_cache_retention`,
`safety_identifier`, `stream_options`, `truncation`, `stop_sequences`.
Also removed or changed:
- `previous_response_id` is deleted on HTTP (non-WebSocket) OAuth requests (forward.go:665-669).
  The upstream keeps no server-side state.
- `prompt` becomes `input` when input is absent. `commands` and
  `input[].internal_chat_message_metadata_passthrough` are removed (request_body.go:1415-1446).
- `text.verbosity` is removed for models below gpt-5.3. Bare `gpt-5` and gpt-6 or later keep it
  (transform.go:1258-1279; forward.go:610-612).
- `reasoning.effort: "minimal"` becomes `"none"` (forward.go:431-434). `max` collapses to
  `xhigh` except on families that support `max` (request_body.go:2697-2733).
- `service_tier`: `fast` becomes `priority`. Allowed values are priority, flex, auto, default,
  scale and ultrafast; unknown values are removed (request_body.go:1964-1982).
- Legacy `functions`/`function_call` become `tools`/`tool_choice`. Chat-style
  `{type:function,function:{name,...}}` tools are flattened to top-level `name`, `description`,
  `parameters` and `strict`, and invalid function tools are dropped (transform.go:224-256,1802-1878).
  A `tool_choice` that names an absent function or tool type becomes `"auto"`; `allowed_tools`
  is left as is (transform.go:363-413).
- The tool name `python` is reserved upstream. It is aliased to `python__sub2api` and restored in
  responses (openai_codex_tool_names.go:13-58).

Input item rules (transform.go:476-535,1610-1757):
- `role:"tool"` messages become `{type:function_call_output, call_id, output}`. Without a call id
  they become a user message.
- `reasoning` items: the `id` (rs_*) is **always removed**, because with store=false a replayed id
  returns 404 "Item with id 'rs_…' not found". `encrypted_content` is kept. When `summary` is
  missing it is set to `[]` (the upstream returns 400 otherwise).
- `compaction`/`compaction_summary` items require `encrypted_content`.
- `item_reference` items are dropped unless the request is a tool continuation.
- Item ids are removed (except in continuation cases). `call_id` is removed from non-tool items.
  Tool-call ids are normalized to the `fc_` prefix ("Expected an ID that begins with 'fc'"),
  `call_x` becomes `fc_x`, and ids longer than 64 characters are hashed down to 64.
- `function_call`, `custom_tool_call` and `mcp_tool_call` must have a `name` (fallback "tool").
- Non-string `text` in content parts is JSON-stringified.
- Tool outputs with no matching call are removed (openai_responses_input_compat.go:9-11).
- When the history replays a `web_search_call` but the request declares no web_search tool, the
  upstream fails with "response protection is unavailable". The fix is to add the tool
  `{"type":"web_search","external_web_access":false}` and, when the caller declared no tools, set
  `tool_choice:"none"` (openai_oauth_web_search_history.go:12-34).

Model name normalization (transform.go:13-81,619-664; openai_model_alias.go): provider prefixes
(`openai/…`), case, `_` and spaces are normalized, `gpt5…` becomes `gpt-5…`, and the suffixes
`-none|-minimal|-low|-medium|-high|-xhigh|-YYYY-MM-DD|-openai-compact` are stripped. Examples from
the map: `gpt-5`, `gpt-5-mini`, `gpt-5-nano` and `gpt-5.1` map to `gpt-5.4`; `gpt-5-codex`,
`gpt-5.1-codex*`, `codex-mini-latest` and `gpt-5.3` map to `gpt-5.3-codex`; `gpt-5.2-codex` maps
to `gpt-5.2`; `gpt-5.4-high` maps to `gpt-5.4`. An empty model maps to `gpt-5.4`. Unknown names
pass through unchanged. The effort implied by a suffix is used only for billing and logging. No
code was found that copies it into `reasoning.effort` (UNCONFIRMED whether that is intentional).
Responses sent to the client show the client's original model name (model fields are rewritten
back).

Special model rules: `gpt-5.3-codex-spark` rejects image input and the `image_generation` tool
(400). Those are stripped or rejected, and an instruction block explains the limitation
(transform.go:145-151,871-907). `gpt-6.1-sol` rejects effort `none`/`minimal`
(pkg/openai/constants.go:243-253).

Non-stream clients: the upstream is always asked to stream. For `stream:false` the gateway reads
the whole SSE body and returns one JSON response (response_handling.go:1625-1838):
1. A terminal `response.failed` or `error` event becomes an error (or a failover).
2. Otherwise the `response` object of the first `response.completed` or `response.done` event is
   the result. If there is none, `response.incomplete`/`cancelled` is used.
3. If that response has an empty `output`, output is rebuilt from the raw `item` objects of
   `response.output_item.done` events, in order, deduplicated by id. If there are none, it is
   rebuilt from `output_text.delta`, function-call arguments and reasoning-summary deltas.
4. Usage comes from the final response. `created_at` is filled in if missing. Content-Type is
   `application/json`.
For OAuth accounts, a body that looks like SSE is treated as SSE even without an
`text/event-stream` Content-Type.

Chat Completions clients: **yes, converted chat→responses** (openai_gateway_chat_completions.go:150-300;
pkg/apicompat/chatcompletions_to_responses.go:20-113):
- `messages` become `input`: system/developer become message items; user content parts become
  input_text/input_image; assistant text becomes `output_text` (reasoning_content is wrapped as
  `<thinking>…</thinking>`); `tool_calls` become `function_call{call_id,name,arguments}`
  (arguments default to `{}`); `tool`/`function` messages become `function_call_output`
  (empty output becomes "(empty)").
- `stream:true` and `store:false` are always set, plus `include:["reasoning.encrypted_content"]`.
  `reasoning_effort` becomes `reasoning{effort, summary:"auto"}`. `response_format` becomes
  `text.format`. `max_tokens`/`max_completion_tokens` become `max_output_tokens` (then stripped by
  the OAuth rules). temperature/top_p are dropped for reasoning models.
- Clients that post a Responses-shaped body (with `input`) to /chat/completions (for example
  Cursor) have it forwarded as Responses.
- Back-conversion of the stream: `output_text.delta` becomes `delta.content`;
  `reasoning_summary_text.delta` and `reasoning_text.delta` become `delta.reasoning_content`;
  `output_item.added` (function_call) plus `function_call_arguments.delta` and
  `custom_tool_call_input.delta` become `delta.tool_calls[i]`. finish_reason is `tool_calls` when
  tools were called, `length` for an incomplete response with reason max_output_tokens,
  `content_filter`, or `stop` (pkg/apicompat/responses_to_chatcompletions.go:89-179). Usage maps
  `prompt_tokens=input_tokens`, `completion_tokens=output_tokens`, total = sum, and the token
  details carry the cached tokens.
- A prompt_cache_key can be added automatically for compat clients, derived from the messages, so
  that caching works (`deriveCompatPromptCacheKey`; the exact derivation is UNCONFIRMED and not needed).

Anthropic Messages clients are bridged in a similar way (openai_gateway_messages*.go). Not detailed here.

## 6. SSE and response handling

Upstream frames are standard SSE (`event: <type>` and `data: <json>`). Event types handled:
`response.created`, `response.in_progress`, `response.output_item.added`,
`response.output_item.done`, `response.output_text.delta`, `response.refusal.delta`,
`response.function_call_arguments.delta|done`, `response.custom_tool_call_input.delta|done`,
`response.reasoning_summary_text.delta|done`, `response.reasoning_text.delta`,
`response.image_generation_call.partial_image`, and the terminals `response.completed`,
`response.done` (alias), `response.incomplete`, `response.failed`, `response.cancelled`, `error`
(response_handling.go:1839-1853,2209-2219). A bare `error` event is often followed by
`response.failed`; the pair is handled as one failure.

Streaming to the client: `Content-Type: text/event-stream`, `Cache-Control: no-cache`,
`X-Accel-Buffering: no`. Events are relayed with the model name rewritten back. Optional keepalive
comments can be enabled. The gateway stops reading as soon as a terminal event has been written. An
error or failure **before any client-visible output** triggers a failover to another account,
unless it is a policy, context-length or invalid-request error. After output has started, the
failure event is passed to the client as is (response_handling.go:535-640;
openai_gateway_passthrough.go:1529-1600). A `response.completed` with no output, no usage and no
error is treated as a silent refusal (response_handling.go:1400-1419).

Usage extraction, first match among `usage`, `response.usage`, `data.usage`, `data.response.usage`
(response_handling.go:1369-1393,1541-1623):
- input = `input_tokens` (fallback `prompt_tokens`)
- output = `output_tokens` (fallback `completion_tokens`). OpenAI already includes reasoning
  tokens in output_tokens (`output_tokens_details.reasoning_tokens`).
- cached read = `input_tokens_details.cached_tokens` (fallback `prompt_tokens_details.cached_tokens`)
- cache write = `input_tokens_details.cache_write_tokens` or `cache_creation_tokens` (rare)
- image tokens: `output_tokens_details.image_tokens`, `input_tokens_details.image_tokens`,
  `tool_usage.image_gen.*`
- Usage on a terminal event replaces earlier values unless it is all zeros. Non-terminal usage is
  merged field by field, keeping non-zero values.

Response headers: `x-request-id` is recorded. `x-codex-turn-state` from the upstream response is
passed back to the client, including on compact JSON (response_handling.go:1708-1710). A
turn-state that another account minted is removed before sending (forward.go:1474).

## 7. Rate limits and quota

**Response headers on every /codex/responses reply, success or 429**
(openai_gateway_usage.go:991-1055):
```
x-codex-primary-used-percent            float
x-codex-primary-reset-after-seconds     int
x-codex-primary-window-minutes          int
x-codex-secondary-used-percent          float
x-codex-secondary-reset-after-seconds   int
x-codex-secondary-window-minutes        int
x-codex-primary-over-secondary-limit-percent  float
```
Mapping windows to 5h and 7d (openai_gateway_service.go:150-225): when both window sizes are
known, the smaller window is 5h. With only one known, a window of 360 minutes or less is 5h.
With none known, the legacy rule applies: primary = 7d, secondary = 5h. The gateway saves a
snapshot (throttled to one write per 30 s) and computes `reset_at = now + reset_after`.

**429 handling**:
- Body shapes (ratelimit_service.go:1840-1897):
  `{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"plus","resets_at":<unix>,"resets_in_seconds":<n>}}`.
  `type` may also be `rate_limit_exceeded`. `resets_at` is used first, then `resets_in_seconds`.
  Both may arrive as a number or a string. `plan_type` from the body updates the stored plan.
- Classification (openai_account_runtime_block_fastpath.go:42-74): **quota exhausted** when a
  header window's used-percent is at least 100 (the 7d window is checked first), or when the body
  has resets_at or resets_in_seconds. The cooldown lasts until that reset time. Anything else is a
  **transient** 429.
- Transient 429 on an OAuth account: retry on the same account for up to 2 min. Retry delay is
  500 ms, or the `Retry-After` value when present, capped at 8 s. After that the account is
  blocked for a configurable fallback cooldown (default 5 s), and at most 3 accounts are tried
  (fastpath.go:13-23,269-312). A "429 storm" guard allows only one account switch per 10 s.
- A used-percent below 100 together with reset-after headers does not count as exhausted
  (ratelimit_service.go:1467-1470).

**Quota / usage query endpoint (optional, for display and auto-reset)**
(service/openai_quota_service.go:27-40,642-660):
- `GET https://chatgpt.com/backend-api/wham/usage`, sent through the browser-fingerprint client.
  Headers:
  ```
  authorization: Bearer <access_token>
  chatgpt-account-id: <id>
  openai-beta: codex-1
  oai-language: zh-CN          (any locale)
  originator: Codex Desktop
  accept: application/json
  sec-fetch-site: none
  sec-fetch-mode: no-cors
  sec-fetch-dest: empty
  priority: u=4, i
  x-openai-fedramp: true       (FedRAMP only)
  ```
- Response fields used: `user_id`, `account_id`, `email`, `plan_type`,
  `rate_limit{allowed, limit_reached, primary_window{used_percent, limit_window_seconds, reset_after_seconds, reset_at}, secondary_window{…}}`
  (a window can be `null`), `additional_rate_limits[]{limit_name, metered_feature, rate_limit{…}}`
  (`metered_feature:"codex_bengalfox"` is the separate Codex Spark quota),
  `rate_limit_reset_credits{available_count, credits[]}`,
  `credits{has_credits, unlimited, balance (decimal string|null)}`.
- Reset credits: `GET …/wham/rate-limit-reset-credits` lists them, and
  `POST …/wham/rate-limit-reset-credits/consume` with body
  `{"redeem_request_id":"<uuid v4>", "credit_id"?:...}` returns
  `{code, credit{…}, windows_reset}`. Optional.
- Cheaper alternative used by the admin "usage" view: send a minimal streaming request
  (`model: codex-auto-review`, input "hi", `store:false`, `stream:true`, default instructions) to
  `/codex/responses` and read only the `x-codex-*` headers (account_usage_service.go:887-1006).
  The headers sent are the same as for inference plus `OpenAI-Beta: responses=experimental`.

## 8. Model list

- Fetched from `GET https://chatgpt.com/backend-api/codex/models?client_version=<ver>`
  (openai_codex_models_service.go:31,1700-1790,2639-2660). Headers: `Authorization: Bearer`,
  `chatgpt-account-id`, `x-openai-fedramp` when applicable, `Accept: application/json`,
  `Originator`, `User-Agent`, `Version` (the client_version when valid and at least 0.144.0,
  otherwise the canonical version), and `If-None-Match` for ETag revalidation. A 401 here goes
  through the same account-error handling as inference.
- Response: `{"models":[{slug, display_name, description, default_reasoning_level,
  supported_reasoning_levels[{effort,description}], visibility, supported_in_api, priority,
  context_window, max_context_window, input_modalities, supports_parallel_tool_calls,
  support_verbosity, model_messages{instructions_template,…}, use_responses_lite, service_tiers,
  …}]}` (service/openai_codex_models_service.go:342-417).
- For OpenAI-style `/v1/models` it becomes `{"object":"list","data":[{id:slug, object:"model",
  created:0, owned_by:"openai", display_name}]}`. Duplicates and slugs containing `*` are skipped
  (openai_models_list.go:104-162). Codex clients get the manifest unchanged. Results are cached
  per account with a stale-while-revalidate pattern.
- Static fallback/default list (pkg/openai/constants.go:22-46): gpt-5.6-sol, gpt-6, gpt-5.6,
  gpt-5.6-terra, gpt-5.6-luna, gpt-6.1-terra, gpt-6.1-sol, gpt-6-sol, gpt-6-luna, gpt-6-astra,
  gpt-5.5, gpt-5.4, gpt-5.4-mini, gpt-5.3-codex-spark, codex-auto-review, gpt-5.2,
  gpt-image-1/1.5/2/2.5-*/2.6-*. Default test model `gpt-5.4`.

## 9. Error classification

Statuses that cause a failover to another account (openai_gateway_upstream_errors.go:303-345):
**401, 402, 403, 405, 429, 529 and any status of 500 or above**, plus:
- 400 with a transient message: "An error occurred while processing your request", "selected model
  is at capacity", or "you can retry your request … help.openai.com … request id"; also code
  `server_is_overloaded` or `slow_down`, or text containing "server(s) are (currently) overloaded".
  Capacity shedding is treated as request-scoped: retry, but do not penalize the account
  (upstream_errors.go:121-198; fastpath.go:104-108).
- A 400 model-not-found is a failover.
- An access-state code at `error.code`, `response.error.code`, `detail.code` or `code` with a value
  such as `deactivated_workspace` or `{workspace|account|organization|org}_{deactivated|disabled|suspended}`
  (either order) is a failover **and permanently disables** the account (upstream_errors.go:478-510).
- **Never** a failover, always passed to the client: context-window errors (`context_length_exceeded`,
  "maximum context length", ...), cyber/content-policy errors, other 4xx validation errors.

What happens to the account (service/ratelimit_service.go:333-600):
- 401 with code `token_invalidated`/`token_revoked`, or body `{"detail":"Unauthorized"}`: permanently disabled.
- Other 401: the token cache is invalidated and the account is unschedulable for
  `rate_limit.oauth_401_cooldown_minutes` (default 10), so a refresh can repair it. With no
  refresh token it is permanently disabled.
- 402 with `detail.code == deactivated_workspace`, or any other 402: disabled.
- 400 with "organization has been disabled" or "identity verification is required": disabled.
- 403: an HTML body (from a CDN or proxy) or a Cloudflare bot block (`error code: 1010`,
  `cloudflare_error:true`, or `error_name:browser_signature_banned`) causes **no** account penalty.
  Otherwise the account gets a 10-minute cooldown, and the 3rd 403 within 180 min disables it
  (ratelimit_service.go:94-96,1125-1150, handleOpenAI403).
- 429: see §7.
- 5xx: logged only, with a short per-model cooldown for API-key accounts.

Retries on the same account before failing over (forward.go:1120-1194):
- 400 `invalid_encrypted_content` (or `thinking_signature_invalid` with "encrypted content …
  could not be verified"): encrypted reasoning items are removed from the input and the request is
  retried once. The bad ciphertext digests are remembered per session and removed up front on
  later turns.
- 400 "Unknown/Unsupported parameter: X" (for example `max_output_tokens`, `truncation`,
  `input[N].namespace|status`, a null `input[N].content`, or `prompt_cache_breakpoint`): the field
  is removed and the request retried, at most 6 times per request
  (openai_responses_rejected_field_retry.go:20-34).
- Compact endpoint: retried once with a fallback model when the model is unsupported.

## 10. Other parity notes

- **Transport / TLS**: inference uses Go's standard TLS with **HTTP/2 enabled by default**. Behind
  an HTTP proxy it falls back to HTTP/1.1 after 2 errors within 60 s, for 10 min
  (config/config.go:2568-2572; repository/http_upstream.go:77-79,1074-1100). Inference does not
  impersonate a browser fingerprint. chatgpt.com **web** endpoints (accounts/check, subscriptions,
  settings, wham/usage, referrals) use Firefox TLS/header impersonation to pass Cloudflare
  (repository/req_client_pool.go:46-60,99-105). The response-header timeout for OpenAI is
  unbounded by default (`gateway.openai_response_header_timeout=0`). An optional "first output
  timeout" depends on reasoning effort.
- **Proxy**: per-account HTTP, HTTPS or SOCKS5 proxy for every call (auth, inference, web).
- **Concurrency**: a per-account `concurrency` value limits in-flight requests and the connection
  pool size. It is admin-set (no protocol-mandated value; the default is UNCONFIRMED). Sticky
  sessions bind a session hash to an account for 1 h (openai_gateway_service.go:35).
- **WebSocket mode (optional, used only when the client connects with WS)**: upstream
  `wss://chatgpt.com/backend-api/codex/responses`, with handshake header
  `OpenAI-Beta: responses_websockets=2026-02-06` (v2; v1 used `2026-02-04` and is rejected) plus
  the same identity and session headers. Each turn sends a JSON message
  `{"type":"response.create", …responses body…, "stream":true, "store":false}` with `background`
  removed. `previous_response_id` is kept on WS. Up to 5 reconnects with 120 ms to 2 s backoff.
  When `previous_response_not_found` occurs, the id is dropped and the request replayed once
  (openai_ws_forwarder.go:17-21; openai_ws_forwarder_payload.go:60-75,100-210; forward.go:772-990).
- **Compaction**: the Codex `/responses/compact` endpoint takes unary JSON (`accept:
  application/json`, a `version` header, `session_id` required; store and stream removed).
  Native "remote compaction v2" uses `x-codex-beta-features: remote_compaction_v2` and a
  `compaction_trigger` input item that must stay last.
- **Image generation (optional)**: through the hosted `image_generation` tool on
  /codex/responses. For `/v1/images/*` the gateway builds
  `{model:"gpt-5.6-luna" (main model, env SUB2API_IMAGES_MAIN_MODEL), stream:true, store:false,
  reasoning{effort:"medium",summary:"auto"}, parallel_tool_calls:true,
  include:["reasoning.encrypted_content"], tool_choice:{type:"image_generation"},
  tools:[{type:"image_generation", action:"generate"|"edit", model:<gpt-image-*>, size, quality,
  background, output_format, …}], input:[user message with input_text plus input_image data URLs]}`
  and reads `image_generation_call` output items (openai_images_responses.go:391-440). For Codex
  CLI clients it can also add the image_generation tool automatically.
- **Other chatgpt.com endpoints (optional)**: `…/codex/alpha/search` (search),
  `…/codex/realtime/calls?intent=quicksilver&architecture=avas` plus
  `wss://chatgpt.com/backend-api/codex` sideband (live/realtime), `…/referrals/invite`.
- **Service tier / fast mode**: `service_tier:"priority"` (from "fast") is passed through, and
  the routing hint carries `tier=`.
- **Cloudflare detection**: response header `cf-mitigated: challenge`, or body text "Just a
  moment", "cloudflare" or `error code: 1010`. These responses are treated as path or IP
  problems, not account problems.
- **Responses Lite (optional)**: header `X-OpenAI-Internal-Codex-Responses-Lite`. Hosted tools
  must then go inside an `input[]{type:"additional_tools"}` item.

### Items not confirmed
- Whether the upstream ever honors the effort implied by a model-name suffix (sub2api does not
  write it into the body).
- The default per-account concurrency for OAuth accounts.
- The exact `prompt_cache_key` derivation for compat clients (sub2api-specific, not needed for the
  upstream contract).
- `oai-language` on wham/usage is hard-coded to `zh-CN`; any locale presumably works.
