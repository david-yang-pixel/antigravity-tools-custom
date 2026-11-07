# Multi-native-ID publications Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One published model can bind several native IDs of the same WorkBuddy or Qoder source, and every account that supports any of them is scheduled as an equal, each request carrying the ID its account supports.

**Architecture:** `PublishedModel` gains `extra_native_models`; the registry validates and gates it. The WorkBuddy and Qoder schedulers take an ordered target list, pick an account from the union pool, and record the chosen native ID on the lease; dispatchers send, cool, trip circuits and label monitoring with that leased ID. The admin UI edits the list.

**Tech Stack:** Rust (Tauri backend, axum, tokio, serde), React + TypeScript frontend, `node --test` for pure frontend logic.

**Spec:** `docs/superpowers/specs/2026-11-06-multi-native-binding-design.md`

## Global Constraints

- Field name `extra_native_models: Vec<String>` with `#[serde(default, skip_serializing_if = "Vec::is_empty")]`; `schema_version` stays `1`.
- At most 16 extra IDs per publication (`MAX_EXTRA_NATIVE_MODELS = 16`).
- Extra IDs allowed only for `workbuddy:default` and native account provider sources (`Provider::from_source(..).is_some()`); Antigravity and custom upstreams reject a non-empty list.
- Target order is `[native_model] + extra_native_models`; an account uses the first target it supports and can schedule.
- Equal pool: ranking (session affinity, in-flight, LRU) is unchanged and ignores which target an account matched.
- With an empty `extra_native_models` every code path must behave exactly as before.
- Billing keys on `public_id`; never on a native ID.
- Commit messages follow AGENTS.md (link Issue/PR; until a PR exists write `Ref: PR pending (claude/wizardly-lamport-muo52d)`) and end with the session's attribution trailers.
- Run only targeted tests for the touched module: `cd src-tauri && cargo test --lib <filter>`; frontend `node --test src/components/proxy/models/registryLogic.test.mjs`. No fmt/clippy/full build (pre-flight is release-only).

## Review Focus

1. A legacy (migrated) publication that gains an extra ID must keep its grandfathered primary binding; only the new extra ID is gated. Test in Task 1.
2. An account whose catalog contains *both* targets must use the first (primary) one, not alternate. Test in Task 2.
3. A WorkBuddy 429 on target A for account X must still let X serve target B in the same group. Test in Task 2.
4. A request bound to a session on a domestic account keeps that account when the group gains no new members; session keys for single-target publications are unchanged so existing affinity survives the upgrade. Test in Task 2.
5. Qoder: the upstream body and `x-mapped-model` carry the leased ID, not the primary ID, when a global-only account is picked. Test in Task 4.

---

### Task 1: Registry field, validation and publication gating

**Files:**
- Modify: `src-tauri/src/modules/model_registry.rs` (struct at :63, `validate_store` :341-354, `save` :1282-1311, `initialize` literal :939)
- Modify struct literals to add `extra_native_models: Vec::new()`: `src-tauri/src/modules/model_registry/tests.rs:56,382`, `src-tauri/src/modules/native_providers/tests.rs:844`, `src-tauri/src/proxy/upstream/openai_compat/tests.rs:109`, `src-tauri/src/proxy/upstream/workbuddy/tests.rs:124,1480`, `src-tauri/src/proxy/common/model_mapping.rs:1501`
- Modify: `docs/MODEL_REGISTRY_IMPLEMENTATION.md` (invariant "binds one source and one native model" → one source, ordered native IDs; add the field to the `PublishedModel` wire type)
- Test: `src-tauri/src/modules/model_registry/tests.rs`

**Interfaces:**
- Produces:
  - `PublishedModel.extra_native_models: Vec<String>`
  - `impl PublishedModel { pub fn native_models(&self) -> Vec<&str> }` — primary first, then extras in stored order.
  - `pub fn pools_native_models(source_id: &str) -> bool` in `model_registry.rs`.
  - `pub const MAX_EXTRA_NATIVE_MODELS: usize = 16;`

- [ ] **Step 1: Write failing tests** in `model_registry/tests.rs` (use existing `Sandbox`, `seed_wb`, `request`, `publication`, `view`, `save`, `resolve_existing`):
  - `extra_native_ids_round_trip_and_resolve_in_order`: seed WB candidates `Flash` and `Flash-sg` (both verified), publish `Flash` with `extra_native_models = ["Flash-sg"]`; `resolve_existing("Flash").unwrap().unwrap().native_models() == vec!["Flash", "Flash-sg"]`.
  - `publications_without_extra_ids_serialize_unchanged`: after saving a plain publication, `model_registry.json` text does not contain `"extra_native_models"`.
  - `extra_native_ids_reject_invalid_bindings`: each of these `save` calls is `Err`: extra equal to primary; duplicate extra; extra not a candidate; extra that is a candidate of another source (AG `same-name` while publication is WB); any extra on an `ANTIGRAVITY_SOURCE` publication; 17 extras.
  - `newly_added_extra_ids_are_gated_like_targets`: extra that is `unavailable` → `Err` containing `"unavailable"` even with `allow_unverified`; extra that is unverified → `Err` containing `"allow_unverified"` without confirmation, `Ok` with it; re-saving the same publication with the same extras and `allow_unverified = false` → `Ok`.
  - `legacy_publication_keeps_exemption_when_an_extra_id_is_added` (Review Focus 1): a legacy publication of an unverified target gains a verified extra with `allow_unverified = false` → `Ok`, and `legacy` stays `true`.

- [ ] **Step 2: Run and confirm they fail**

Run: `cd src-tauri && cargo test --lib model_registry::tests::extra_native -- --nocapture` (and the other two names)
Expected: compile error / FAIL on the missing field.

- [ ] **Step 3: Implement**
  - Add the field with the serde attributes from Global Constraints; add `native_models()`, `pools_native_models()`, the constant.
  - `validate_store`: for each publication, if `extra_native_models` is non-empty require `pools_native_models(source_id)`, length ≤ 16, each passes `validate_native`, is in `candidates` for the same source, is unique and ≠ `native_model`. Same error string family ("Invalid or duplicate publication/source/target").
  - `save`: keep `same_binding`/`grandfathered` on the primary only. Gate each extra ID not present in the old record's extras (or every extra when `!same_binding`, or when the record goes disabled→enabled) with the same two checks as the primary, looking up its candidate via `candidate_id(source_id, extra)`.
  - Add `extra_native_models: Vec::new()` to every listed struct literal.
  - Update the doc file.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo test --lib model_registry`
Expected: all PASS.

- [ ] **Step 5: Commit** — `feat(registry): bind extra native IDs of the same source to a publication`

---

### Task 2: WorkBuddy scheduler over a target list

**Files:**
- Modify: `src-tauri/src/modules/workbuddy/accounts_scheduler.rs`
- Modify: `src-tauri/src/modules/workbuddy/accounts.rs` (`Lease` :976, test helper `acquire` :1082)
- Modify callers of the changed signatures: `src-tauri/src/modules/workbuddy/verification.rs:79` only if `acquire_probe` changes (it must not)
- Test: `accounts_scheduler.rs` `mod tests`

**Interfaces:**
- Consumes: nothing from Task 1 (plain `&[&str]`).
- Produces (all in `workbuddy::accounts::scheduling`):
  - `pub fn try_acquire(targets: &[&str], session: &str, policy: &Policy) -> Result<Option<Lease>, String>` (stays `pub(super)`)
  - `pub fn try_acquire_excluding(targets: &[&str], session: &str, policy: &Policy, excluded: &[String]) -> Result<Option<Lease>, String>`
  - `pub async fn acquire(targets: &[&str], session: &str, policy: &Policy, timeout: u64) -> Result<Lease, String>`
  - `pub async fn acquire_probe(account: &Account, model: &str) -> Result<Lease, String>` — unchanged signature.
  - `pub struct Admission { pub targets: Vec<String>, pub probe: Option<ModelProbe> }`
  - `pub fn admit_models(targets: &[&str], policy: &Policy) -> Result<Admission, ModelLimited>` (replaces public `admit_model`, which becomes private)
  - `pub fn hold_cooling_models(targets: &[&str], policy: &Policy) -> Option<ModelLimited>` (replaces public `hold_cooling_model`, which becomes private)
  - `record_model_limit(model: &str, ..)` and `model_accepted(model: &str)` unchanged.
  - `impl Lease { pub fn model(&self) -> &str }` in `accounts.rs`.

- [ ] **Step 1: Write failing tests** (add a `seed_with(uid, models: &[&str])` helper beside `seed`; existing tests switch `acquire("model", ..)` to `acquire(&["model"], ..)`):
  - `workbuddy_group_pools_accounts_with_different_native_ids`: seed `cn` with `["flash"]`, `global` with `["flash-sg"]`; PerformanceFirst; two concurrent leases on `&["flash", "flash-sg"]` cover both accounts, and each `lease.model()` equals the ID that account lists.
  - `workbuddy_group_prefers_first_target_on_accounts_listing_both` (Review Focus 2): one account with `["flash", "flash-sg"]`; three sequential leases all have `model() == "flash"`.
  - `workbuddy_group_rate_limit_on_one_target_keeps_the_other` (Review Focus 3): account with both IDs; `model_failure` on a `flash` lease; next lease on the group returns the same account with `model() == "flash-sg"`; a lease on `&["flash"]` alone returns `Ok(None)` from `try_acquire`.
  - `workbuddy_single_target_session_key_is_unchanged` (Review Focus 4): bind a session with `&["model"]`, then `acquire(&["model"], same session)` returns the same account (guards the key format `"{path}:{model}:{session}"` for one target).
  - `workbuddy_group_admission_skips_held_targets_and_reports_min_retry`: open the circuit for `flash` (two accounts record agreeing 429s via `record_model_limit`); `admit_models(&["flash","flash-sg"])` returns `targets == ["flash-sg"]`, `probe.is_none()`; open `flash-sg` too and `admit_models` returns `Err` whose `retry_after` is the smaller hold.
  - `workbuddy_group_half_open_probe_only_when_no_healthy_target`: after `expire_model_circuits()` both targets are half-open-or-healthy; with `flash-sg` healthy the admission has no probe; with both half-open it admits exactly `["flash"]` with a probe.
  - `workbuddy_group_hold_requires_every_target_cooling`: cool `flash` on all its accounts; `hold_cooling_models(&["flash","flash-sg"])` is `None` while a `flash-sg` account is free, `Some` once every account for both is cooling.

- [ ] **Step 2: Run and confirm failure**

Run: `cd src-tauri && cargo test --lib workbuddy::accounts`
Expected: compile errors on the new signatures.

- [ ] **Step 3: Implement**
  - `eligible(targets, ..)`: account supports any target (`supports_model` for real requests, catalog membership for probes).
  - `try_acquire_inner`: session key `format!("{}:{}:{session}", path, targets.join(","))` (identical to today for one target). For each account compute `pick = targets.iter().find(|t| account supports t && (!breaker || entry.schedulable(t, now)))`; an account with no `pick` is unavailable; keep the CacheFirst bound-account early return; store `pick` in `Lease.model`.
  - `admit_models`: call private `admit_model` per target; collect healthy (`Ok(None)`), probes (`Ok(Some)`), and held (`Err`). Healthy non-empty → those targets, drop probe guards. Else first probe target with its guard. Else `Err` with the minimum `retry_after` (its `accounts`/`reason` from that same target).
  - `hold_cooling_models`: call private `hold_cooling_model` for every target (do not short-circuit, so each target's circuit is updated); return `None` if any is `None`, else the one with the smallest `retry_after`.
  - `Lease::model()` accessor; update the `#[cfg(test)] acquire(model)` helper in `accounts.rs` to pass `&[model]`.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo test --lib workbuddy::accounts && cargo test --lib workbuddy::verification`
Expected: PASS.

- [ ] **Step 5: Commit** — `feat(workbuddy): schedule one pool across a publication's native IDs`

---

### Task 3: WorkBuddy dispatch uses the leased native ID

**Files:**
- Modify: `src-tauri/src/proxy/upstream/workbuddy.rs` (`dispatch` :72-158, `forward` :235-313, `attempt` :319-…, `decorate` call :601)
- Modify: `docs/WORKBUDDY_UPSTREAM.md` (new short section under「使用」: binding cn and global IDs to one public model, equal scheduling, headless JSON example, downgrade note)
- Test: `src-tauri/src/proxy/upstream/workbuddy/tests.rs`

**Interfaces:**
- Consumes: Task 1 `PublishedModel::native_models()`; Task 2 `admit_models`, `acquire`, `try_acquire_excluding`, `hold_cooling_models`, `Lease::model()`.
- Produces: none for later tasks.

- [ ] **Step 1: Write failing test** `workbuddy_group_binding_sends_each_account_its_own_native_id`: use the existing mock upstream (its handler records request bodies; extend it to record `body["model"]` if it does not already) and seed two accounts, one listing `claude-sonnet-4-6`, the other `claude-sonnet-4-6-sg`. Insert a `PublishedModel` extension for `wb-sonnet` with `native_model = "claude-sonnet-4-6"`, `extra_native_models = ["claude-sonnet-4-6-sg"]`; send two concurrent non-stream requests with `Policy` in PerformanceFirst. Assert both succeed, the upstream saw both model IDs once each, and each response's `x-mapped-model` matches the ID that account received.

- [ ] **Step 2: Run and confirm failure**

Run: `cd src-tauri && cargo test --lib proxy::upstream::workbuddy::tests::workbuddy_group_binding`
Expected: FAIL (only the primary ID is ever sent).

- [ ] **Step 3: Implement**
  - `dispatch` passes `published.native_models()` (owned `Vec<String>`) to `forward` instead of `target`.
  - `forward(targets: &[String], ..)`: `admit_models` → on `Err` return `model_limited` naming `targets.join(", ")`; use `admission.targets` for `acquire`/`try_acquire_excluding`; on acquire failure use `hold_cooling_models(&all targets)`. Use the probe policy only when `admission.probe.is_some()`.
  - `attempt` drops its `target` parameter and uses `lease.model()` (clone to a local `String` before the lease moves) for `client::chat`, logs, 429 messages, `record_model_limit`, `model_accepted` and `decorate`.
  - Update the doc.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo test --lib proxy::upstream::workbuddy`
Expected: PASS (all existing WorkBuddy dispatch tests too).

- [ ] **Step 5: Commit** — `feat(workbuddy): dispatch grouped publications with each account's native ID`

---

### Task 4: Qoder scheduler and dispatch over a target list

**Files:**
- Modify: `src-tauri/src/modules/native_providers/accounts_scheduler.rs`
- Modify: `src-tauri/src/modules/native_providers/accounts.rs` (`Lease` :194, caller :287)
- Modify: `src-tauri/src/proxy/upstream/native.rs` (`dispatch` :40-125, `open` :298-…)
- Modify: `docs/proxy/native-providers.md` (same feature note as WorkBuddy, Qoder wording)
- Test: `src-tauri/src/modules/native_providers/tests.rs`

**Interfaces:**
- Consumes: Task 1 `PublishedModel::native_models()`.
- Produces:
  - `pub fn try_acquire(provider: Provider, targets: &[&str], session: &str, policy: &Policy, exclude: &HashSet<String>) -> Result<Option<Lease>, Failure>`
  - `pub async fn acquire(provider: Provider, targets: &[&str], session: &str, policy: &Policy, exclude: &HashSet<String>, request_deadline: tokio::time::Instant) -> Result<Lease, Failure>`
  - `Lease.model: String` (pub) — the picked native ID.
  - `open(provider, targets: &[String], payload, timeout, session, policy) -> Result<(Events, Account, String), Failure>` — third element is the native ID actually sent.

- [ ] **Step 1: Write failing tests** (existing calls change `"shared-model"` to `&["shared-model"]`):
  - `native_group_pools_accounts_with_different_native_ids`: Qoder accounts `cn` with models `["shared-model"]` and `global` with `["shared-model-sg"]`; two PerformanceFirst `try_acquire` calls on `&["shared-model", "shared-model-sg"]` lease both accounts with matching `lease.model`.
  - `native_group_quota_filter_applies_per_target`: an account whose only matching target is non-free while its quota is exhausted is not eligible; the same account is eligible when the matching target `is_free`.
  - `native_group_prefers_first_target_on_accounts_listing_both`: one account with both models → `lease.model == "shared-model"`.
  - Review Focus 5, in `native.rs` tests or the existing native dispatch test module: with only the `global` account enabled, a dispatch for a publication `native_model = "shared-model"`, extras `["shared-model-sg"]` sends `shared-model-sg` to `qoder::chat` (assert via the existing mocked Qoder endpoint's captured body, or, if no Qoder HTTP mock exists, assert on `open`'s returned model) and sets `x-mapped-model: shared-model-sg`.

- [ ] **Step 2: Run and confirm failure**

Run: `cd src-tauri && cargo test --lib native_providers`
Expected: compile errors on the new signatures.

- [ ] **Step 3: Implement**
  - Scheduler: an account is eligible if any target satisfies the existing per-model predicate (`m.id == t && (m.is_free || !quota exhausted)`); `pick` is the first such target. Session key `format!("{}:{}:{}:{session}", dir, provider.key(), targets.join(","))` (unchanged for one target). Put `pick` in `Lease.model`.
  - `accounts.rs:287` passes `&[model]`.
  - `native.rs`: `dispatch` passes `published.native_models()`; `open` sets `payload["model"] = json!(lease.model)` on the per-attempt clone before `send(&lease.account, &lease.model, ..)` and returns the model; `x-mapped-model` uses it.
  - Update the doc.

- [ ] **Step 4: Run tests**

Run: `cd src-tauri && cargo test --lib native_providers && cargo test --lib proxy::upstream::native`
Expected: PASS.

- [ ] **Step 5: Commit** — `feat(qoder): schedule one pool across a publication's native IDs`

---

### Task 5: Admin UI for extra native IDs

**Files:**
- Modify: `src/types/modelRegistry.ts` (`PublishedModel` gains `extra_native_models?: string[]`)
- Modify: `src/components/proxy/models/registryLogic.ts`
- Modify: `src/components/proxy/models/ModelRegistryPanel.tsx` (publish/edit dialog :219-226, published table :204, details :233)
- Test: `src/components/proxy/models/registryLogic.test.mjs`

**Interfaces:**
- Consumes: the backend field from Task 1.
- Produces (in `registryLogic.ts`):
  - `export const pooledSource = (view: ModelRegistryView, sourceId: string) => boolean` — true when the source's `provider_type` is `workbuddy` or `qoder`.
  - `export function extraTargetOptions(view: ModelRegistryView, draft: PublishedModel): CandidateModel[]` — same-source candidates, excluding the primary, ignored and `unavailable` ones, but always including IDs already in `draft.extra_native_models`; sorted by `native_model`.
  - `export const nativeTargets = (model: PublishedModel) => string[]` — primary then extras.

- [ ] **Step 1: Write failing tests** in `registryLogic.test.mjs`:
  - `extra target options stay within the source and hide ignored or unavailable candidates`: view with WB candidates `a`, `a-sg`, `a-ignored` (ignored), `a-denied` (unavailable) and AG `a-sg`; options for a WB draft of `a` are exactly `['a-sg']`; a draft already holding `a-denied` lists it too.
  - `pooled sources are workbuddy and qoder only`: `pooledSource(view, wb.id) === true`, `pooledSource(view, ag.id) === false`, a `qoder` source → true.
  - `newly added extra ids are gated like the primary`: `needsUnverifiedConfirmation` is true when an existing verified publication adds an unverified extra, false when the extras are unchanged; `publicationError` returns the unavailable message when an `unavailable` extra is newly added.

- [ ] **Step 2: Run and confirm failure**

Run: `node --test src/components/proxy/models/registryLogic.test.mjs`
Expected: FAIL (`extraTargetOptions is not a function`).

- [ ] **Step 3: Implement**
  - Logic: the three exports; in `publicationError` and `needsUnverifiedConfirmation` treat each extra not present in the previous record's extras (all of them when the primary binding changed or the record is re-enabled) like a target: unavailable → error, not verified → needs confirmation. Leave `sameBinding` primary-only so legacy grandfathering matches the backend.
  - Panel: in the publish/edit dialog, when `pooledSource(view, p.source_id)`, render a fieldset legend `同时调用 / Also route to` with one checkbox per `extraTargetOptions` entry (label: `native_model` plus the existing `<Verification>` badge) and helper text `所有勾选 ID 的账号与主 ID 一起轮换调度，按公开 ID 计费。 / Accounts for every checked ID are scheduled together with the primary; billed by public ID.`. Toggling updates `drafts[index].extra_native_models` (omit the key when empty). Published table target cell shows `+N` as a `registry-badge` after the primary code; details dialog lists `nativeTargets(model)`. Update the dialog intro copy from "binds one source/native target" to "binds one source and its native target(s)".

- [ ] **Step 4: Run tests and typecheck**

Run: `node --test src/components/proxy/models/registryLogic.test.mjs && npx tsc --noEmit`
Expected: all tests PASS; tsc exits 0.

- [ ] **Step 5: Commit** — `feat(ui): edit extra native IDs on WorkBuddy and Qoder publications`
