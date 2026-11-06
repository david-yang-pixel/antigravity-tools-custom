# One public model, several native IDs of the same source

## Problem

A WorkBuddy domestic (cn) account lists `deepseek-v4.1-flash`, a global account lists
`deepseek-v4.1-flash-sg`. A publication binds exactly one source and one native ID, and
WorkBuddy/Qoder dispatch admits only accounts whose catalog contains that exact ID. So one
public name can use either the domestic or the global pool, never both.

## Goal

An administrator can bind additional native IDs of the **same source** to one publication.
Every account that supports any bound ID joins **one pool and is scheduled as an equal**
(no primary/fallback ordering between pools). Each request is sent with the native ID the
chosen account actually supports.

Non-goals: cross-source bindings, primary/fallback semantics, model groups as a separate
entity, multi-binding for Antigravity or custom OpenAI-compatible upstreams.

## Data model

`PublishedModel` gains `extra_native_models: Vec<String>`:

- `#[serde(default, skip_serializing_if = "Vec::is_empty")]` — files and API payloads that
  do not use the feature are byte-for-byte unchanged; `schema_version` stays 1.
- `native_model` remains the primary ID (first in order, used by legacy/AG code paths).
- Validation (`validate_store` and `save`):
  - each extra ID passes `validate_native` for the publication's source and is an existing
    candidate of that source;
  - no duplicates, none equal to `native_model`;
  - at most 16 extra IDs;
  - non-empty only for sources whose dispatcher pools accounts per model: `workbuddy` and
    native account providers (`qoder`). Antigravity and custom upstreams reject it.
- Publication gating applies per newly bound extra ID exactly as for the primary target:
  a definitively `unavailable` candidate cannot be added; an unverified one requires
  `allow_unverified`. Unchanged bindings are not re-gated.
- `PublishedModel::native_models()` returns `[native_model] + extra_native_models` in order.

## Dispatch

Dispatchers receive the ordered target list instead of a single target.

### Account selection (WorkBuddy and Qoder)

- Eligible accounts: enabled accounts (plus the existing fixed-account, credential and quota
  filters) that support **any** target.
- Ranking is unchanged (session affinity, in-flight load, least recently used), so domestic
  and global accounts compete as equals.
- For the chosen account the scheduler picks the first target, in binding order, that the
  account supports and that is schedulable on it (WorkBuddy per-model 429 cooldown). An
  account with no schedulable target is not a candidate.
- The picked native ID is carried on the lease (`Lease.model` for WorkBuddy, a new field for
  Qoder) and used for the upstream request, 429 cooldown, circuit accounting, logs and the
  `x-mapped-model` header.
- Session affinity keys use the joined target list, so a session keeps its account.

### WorkBuddy model circuit

Circuits stay per native ID.

- Admission: every target is checked with `admit_model`. Targets that are held are removed
  from the request's target list. If any target is admitted normally, half-open probe
  targets are dropped too (no probe is spent while healthy targets exist). If only probe
  targets remain, the first one is admitted as the probe with the probe policy. If every
  target is held, the request gets 429 with the smallest `Retry-After`.
- When no account can be leased, `hold_cooling_model` runs per target; only if every target
  is held does the request return 429 (smallest `Retry-After`), otherwise the existing 503.
- `record_model_limit` / `model_accepted` use the leased native ID.

## Billing and observability

Billing, prices and free-model rules key on the public ID and do not change. Monitor and
token stats see the actual native ID through `x-mapped-model`.

## UI and headless parity

- Publish/edit dialog: for supporting sources, a checkbox list "同时调用 / Also route to"
  with the other candidates of the same source (ignored and unavailable candidates hidden,
  already-bound ones shown checked).
- Published table: primary native ID plus a `+N` badge; details dialog lists all IDs.
- Headless/Docker: `save_model_registry` accepts the same field; documented in
  `docs/WORKBUDDY_UPSTREAM.md` and `docs/proxy/native-providers.md`.

## Risks and rollback

- Downgrading to a build without the field while a publication uses it makes the old build
  reject `model_registry.json` (it denies unknown fields) and deny all inference. Clear the
  extra IDs before downgrading. Registries that never used the feature are unaffected.
- Behavior with no extra IDs is identical to today; every new path is gated on a non-empty
  list.

## Testing

- Registry: round-trip without the field, validation failures (cross-source, duplicate,
  unknown candidate, unsupported source), gating of newly added extra IDs.
- WorkBuddy scheduler: mixed pool picks per-account native IDs, per-ID cooldown excludes
  only that ID on that account, circuit admission across targets.
- Qoder scheduler: mixed pool picks per-account native IDs.
- Frontend: `registryLogic` helpers for extra-ID options and validation.
