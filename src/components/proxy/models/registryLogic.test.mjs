// Offline pure-logic checks. No bridge, browser, account or inference requests.
// Run: node --test src/components/proxy/models/registryLogic.test.mjs
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { test } from 'node:test';
import ts from 'typescript';

const source = await readFile(new URL('./registryLogic.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext } });
const logic = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
const ag = { id: 'antigravity:default', provider_type: 'antigravity', display_name: 'AG source', enabled: true, account_count: 2, pools_native_models: false };
const wb = { id: 'workbuddy:default', provider_type: 'workbuddy', display_name: 'WB source', enabled: true, account_count: 1, pools_native_models: true };
const candidate = (id, source_id, native_model, patch = {}) => ({ id, source_id, native_model, provider_type: source_id === ag.id ? ag.provider_type : wb.provider_type, display_name: native_model, account_count: 1, verified_account_count: 0, verification_status: 'unverified', ignored: false, last_verified_at: null, capabilities: [], ...patch });
const publication = (id, source_id, native_model, public_id, patch = {}) => ({ id, source_id, native_model, public_id, display_name: public_id, enabled: true, legacy: false, ...patch });
const view = (patch = {}) => ({ schema_version: 1, revision: 7, sources: [ag, wb], candidates: [candidate('ag-c', ag.id, 'same-name'), candidate('wb-c', wb.id, 'same-name')], published_models: [], notices: [], ...patch });

test('candidate lookup binds explicit source/native pairs, never public aliases', () => {
    const registry = view();
    assert.equal(logic.candidateFor(registry, publication('p', wb.id, 'same-name', 'antigravity-custom')).id, 'wb-c');
    assert.equal(logic.candidateFor(registry, publication('p', ag.id, 'different', 'same-name')), undefined);
});

test('selectors contain only enabled publications with enabled, known sources', () => {
    assert.deepEqual(logic.registryCatalog(view()), []);
    const registry = view({ published_models: [publication('a', ag.id, 'same-name', 'a'), publication('b', wb.id, 'same-name', 'b', { enabled: false }), publication('u', 'missing', 'same-name', 'u')] });
    assert.deepEqual(logic.registryCatalog(registry).map(p => p.public_id), ['a']);
    assert.deepEqual(logic.registryCatalog({ ...registry, sources: [{ ...ag, enabled: false }, wb] }), []);
});

test('protocol selection follows native target and source, not image-like public alias', () => {
    const registry = view({ published_models: [publication('a', ag.id, 'text-target', 'custom-image'), publication('b', wb.id, 'text-target', 'gemini-sounding'), publication('c', ag.id, 'gemini-future-image', 'pictures')] });
    const models = logic.registryCatalog(registry);
    assert.deepEqual(models[0].protocols, ['openai', 'anthropic', 'gemini']);
    assert.equal(models[0].image_generation, false);
    assert.deepEqual(models[1].protocols, ['openai', 'anthropic']);
    assert.equal(models[1].source_name, 'WB source');
    assert.deepEqual(models[2].protocols, ['openai', 'gemini']);
    assert.equal(models[2].image_generation, true);
});

test('OpenAI-compatible upstreams serve OpenAI and Anthropic endpoints only', () => {
    const custom = { ...ag, id: 'openai-compatible:relay', provider_type: 'openai_compatible', display_name: 'Relay' };
    const registry = view({ sources: [custom], candidates: [candidate('relay-c', custom.id, 'gpt-x')], published_models: [publication('relay-p', custom.id, 'gpt-x', 'relay-gpt')] });
    const [model] = logic.registryCatalog(registry);
    assert.deepEqual(model.protocols, ['openai', 'anthropic']);
    assert.equal(model.source_name, 'Relay');
    assert.equal(model.image_generation, false);
});

test('native Qoder publications retain their sources and supported client protocols', () => {
    for (const provider_type of ['qoder']) {
        const source = { ...ag, id: `${provider_type}:default`, provider_type, display_name: provider_type, pools_native_models: true };
        const registry = view({ sources: [source], candidates: [candidate('native-c', source.id, 'same-native')], published_models: [publication('native-p', source.id, 'same-native', `${provider_type}-chat`)] });
        const [model] = logic.registryCatalog(registry);
        assert.deepEqual(model.protocols, ['openai', 'anthropic']);
        assert.equal(model.source_id, source.id);
        assert.equal(model.image_generation, false);
    }
});

test('future providers remain visible without guessing protocol compatibility; explicit capabilities win', () => {
    const extra = { ...ag, id: 'future:one', provider_type: 'future', display_name: 'Future' };
    const c = candidate('future-c', extra.id, 'm');
    const registry = view({ sources: [extra], candidates: [c], published_models: [publication('future-p', extra.id, 'm', 'custom')] });
    assert.deepEqual(logic.registryCatalog(registry)[0].protocols, []);
    c.capabilities = [{ key: 'gemini_protocol', status: 'conditional' }, { key: 'protocol_openai', status: 'supported' }];
    assert.deepEqual(logic.registryCatalog(registry)[0].protocols, ['openai', 'gemini']);
    const known = view({ candidates: [candidate('ag-c', ag.id, 'same-name', { capabilities: [{ key: 'gemini_protocol', status: 'unsupported' }] })], published_models: [publication('a', ag.id, 'same-name', 'a')] });
    assert.equal(logic.registryCatalog(known)[0].protocols.includes('gemini'), false);
});

test('search, source, ignored and text status filters compose and search display names', () => {
    const registry = view();
    registry.candidates[1].ignored = true;
    registry.candidates[1].verification_status = 'failed';
    assert.equal(logic.filterCandidates(registry.candidates, '', '', '', false).length, 1);
    assert.deepEqual(logic.filterCandidates(registry.candidates, wb.id, ' WB SOURCE ', 'failed', true, registry.sources).map(c => c.id), ['wb-c']);
    assert.equal(logic.filterCandidates(registry.candidates, ag.id, '', 'verified', true).length, 0);
    const publications = view({ published_models: [publication('a', ag.id, 'same-name', 'alias'), publication('b', wb.id, 'same-name', 'alias-b', { enabled: false })] });
    assert.deepEqual(logic.filterPublications(publications, '', 'ag source', 'unverified', 'enabled').map(p => p.id), ['a']);
    assert.deepEqual(logic.filterPublications(publications, wb.id, '', '', 'disabled').map(p => p.id), ['b']);
});

test('publication validates exact case-sensitive public names and a single models/ prefix', () => {
    const previous = publication('old', ag.id, 'same-name', 'Alias');
    const registry = view({ published_models: [previous] });
    assert.equal(logic.publicIdKey('models/Alias'), 'Alias');
    assert.equal(logic.publicIdKey('Models/Alias'), 'Models/Alias');
    assert.ok(logic.publicationError(registry, [publication('new', wb.id, 'same-name', 'models/Alias')]));
    assert.equal(logic.publicationError(registry, [publication('new', wb.id, 'same-name', 'alias')]), null);
    for (const name of ['', 'models/', 'a b', 'a\nb', 'a*', 'a?', 'a\u0000b']) {
        assert.ok(logic.publicationError(view(), [publication('new', ag.id, 'same-name', name)]), JSON.stringify(name));
    }
    assert.ok(logic.publicationError(view(), [publication('new', ag.id, 'same-name', 'x'), publication('other', wb.id, 'same-name', 'x')]));
});

test('a published binding can take another alias, never a used name', () => {
    const registry = view({ published_models: [publication('old', ag.id, 'same-name', 'same-name')] });
    assert.ok(logic.publicationError(registry, [publication('new', ag.id, 'same-name', 'same-name')]));
    assert.equal(logic.publicationError(registry, [publication('new', ag.id, 'same-name', 'my-alias')]), null);
});

test('unavailable/new disabled-source candidates cannot be published', () => {
    const unavailable = view({ candidates: [candidate('ag-c', ag.id, 'same-name', { verification_status: 'unavailable' })] });
    assert.ok(logic.publicationError(unavailable, [publication('new', ag.id, 'same-name', 'new')]));
    assert.ok(logic.publicationError(view({ sources: [{ ...ag, enabled: false }] }), [publication('new', ag.id, 'same-name', 'new')]));
    assert.ok(logic.publicationError(view(), [publication('new', ag.id, 'not-discovered', 'new')]));
});

test('unverified publication always requires explicit consent; ordinary edits do not create exposure', () => {
    const fresh = publication('new', ag.id, 'same-name', 'new');
    assert.equal(logic.needsUnverifiedConfirmation(view(), [fresh]), true);
    assert.equal(logic.saveRequest(view(), [fresh]).allow_unverified, false);
    assert.equal(logic.saveRequest(view(), [fresh], undefined, true).allow_unverified, true);
    const registry = view({ published_models: [fresh] });
    assert.equal(logic.needsUnverifiedConfirmation(registry, [{ ...fresh, display_name: 'Renamed display' }]), false);
    const disabled = { ...fresh, enabled: false };
    assert.equal(logic.needsUnverifiedConfirmation(view({ published_models: [disabled] }), [fresh]), true);
    assert.equal(logic.needsUnverifiedConfirmation(view({ candidates: [candidate('ag-c', ag.id, 'same-name', { verification_status: 'verified' })] }), [fresh]), false);
});

test('legacy exemption stays with the same normalized binding and never transfers to an alias', () => {
    const old = publication('stable-id', ag.id, 'same-name', 'Legacy', { legacy: true });
    const registry = view({ published_models: [old] });
    assert.equal(logic.needsUnverifiedConfirmation(registry, [{ ...old, public_id: 'models/Legacy' }]), false);
    assert.equal(logic.mergePublications(registry, [{ ...old, display_name: 'Display' }])[0].legacy, true);
    const renamed = { ...old, public_id: 'new-alias' };
    assert.equal(logic.needsUnverifiedConfirmation(registry, [renamed]), true);
    const merged = logic.mergePublications(registry, [renamed]);
    assert.equal(merged[0].legacy, false);
    assert.equal(merged[0].id, 'stable-id');
    assert.equal(old.public_id, 'Legacy');
    assert.equal(logic.needsUnverifiedConfirmation(view(), [{ ...old, id: 'forged' }]), true);
    assert.equal(logic.mergePublications(view(), [old])[0].legacy, false);
});

test('save payload preserves revision, untouched publications and persisted ignored IDs', () => {
    const original = publication('a', ag.id, 'same-name', 'a');
    const registry = view({ candidates: [candidate('c', ag.id, 'same-name', { ignored: true })], published_models: [original] });
    const added = publication('b', wb.id, 'same-name', 'b');
    const payload = logic.saveRequest(registry, logic.mergePublications(registry, [added]));
    assert.equal(payload.revision, 7);
    assert.deepEqual(payload.ignored_candidate_ids, ['c']);
    assert.deepEqual(payload.published_models.map(p => p.id), ['a', 'b']);
    assert.equal(registry.published_models.length, 1);
});

test('text checks are explicitly bounded to 1–5 unique candidates', () => {
    assert.ok(logic.verificationError([]));
    assert.equal(logic.verificationError(['a']), null);
    assert.equal(logic.verificationError(['a', 'b', 'c', 'd', 'e']), null);
    assert.ok(logic.verificationError(['a', 'b', 'c', 'd', 'e', 'f']));
    assert.ok(logic.verificationError(['a', 'a']));
});

test('evidence, native declarations and gateway capabilities remain separate', () => {
    for (const key of ['reasoning', 'vision', 'tools', 'native_future']) assert.equal(logic.capabilityScope({ key, status: 'unverified' }), 'native');
    for (const key of ['thinking_budget', 'thinking_backfill', 'agent_compression', 'aliases', 'protocol_adaptation', 'gemini_protocol', 'image_generation']) assert.equal(logic.capabilityScope({ key }), 'gateway');
    assert.equal(logic.capabilityScope({ key: 'text', status: 'supported' }), 'evidence');
    assert.equal(logic.capabilityScope({ key: 'unknown' }), 'other');
});

test('conflicts are explicit and generated publication IDs are stable draft-safe strings', () => {
    for (const error of [new Error('revision conflict'), 'HTTP 409', { message: 'revision changed' }, '版本冲突']) assert.equal(logic.registryError(error).conflict, true);
    assert.equal(logic.registryError(new Error('network unavailable')).conflict, false);
    const circular = {}; circular.self = circular;
    assert.doesNotThrow(() => logic.registryError(circular));
    const ids = Array.from({ length: 100 }, () => logic.newPublicationId());
    assert.equal(new Set(ids).size, 100);
    assert.equal(ids.every(id => /^[a-z0-9-]+$/i.test(id)), true);
    assert.match(logic.formatCheckedAt(null), /Never checked/);
});

test('non-secure headless Web origins can create publication drafts without randomUUID', () => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'crypto');
    Object.defineProperty(globalThis, 'crypto', { configurable: true, value: undefined });
    try {
        const ids = Array.from({ length: 100 }, () => logic.newPublicationId());
        assert.equal(new Set(ids).size, 100);
        assert.equal(ids.every(id => /^publication-[a-z0-9-]+$/.test(id)), true);
    } finally {
        if (descriptor) Object.defineProperty(globalThis, 'crypto', descriptor);
        else delete globalThis.crypto;
    }
});

test('extra target options stay within the source and hide ignored or unavailable candidates', () => {
    const registry = view({ candidates: [candidate('c1', wb.id, 'a'), candidate('c2', wb.id, 'a-sg'), candidate('c3', wb.id, 'a-ignored', { ignored: true }), candidate('c4', wb.id, 'a-denied', { verification_status: 'unavailable' }), candidate('c5', ag.id, 'a-sg')] });
    const draft = publication('p', wb.id, 'a', 'alias');
    assert.deepEqual(logic.extraTargetOptions(registry, draft).map(c => c.native_model), ['a-sg']);
    assert.deepEqual(logic.extraTargetOptions(registry, { ...draft, extra_native_models: ['a-denied'] }).map(c => c.native_model), ['a-denied', 'a-sg']);
});

test('pooled sources follow the backend flag, not the provider type', () => {
    const qoder = { ...ag, id: 'qoder:default', provider_type: 'qoder', pools_native_models: true };
    // A future pooled provider needs no frontend change; a known type without the flag is not pooled.
    const future = { ...ag, id: 'future:one', provider_type: 'future', pools_native_models: true };
    const unflagged = { ...wb, id: 'workbuddy:other', pools_native_models: false };
    const registry = view({ sources: [ag, wb, qoder, future, unflagged] });
    assert.equal(logic.pooledSource(registry, wb.id), true);
    assert.equal(logic.pooledSource(registry, ag.id), false);
    assert.equal(logic.pooledSource(registry, qoder.id), true);
    assert.equal(logic.pooledSource(registry, future.id), true);
    assert.equal(logic.pooledSource(registry, unflagged.id), false);
    assert.equal(logic.pooledSource(registry, 'missing'), false);
    assert.deepEqual(logic.nativeTargets(publication('p', wb.id, 'a', 'x', { extra_native_models: ['b'] })), ['a', 'b']);
    assert.deepEqual(logic.nativeTargets(publication('p', wb.id, 'a', 'x')), ['a']);
});

test('newly added extra ids are gated like the primary', () => {
    const base = publication('p', wb.id, 'a', 'alias');
    const registry = view({ candidates: [candidate('c1', wb.id, 'a', { verification_status: 'verified' }), candidate('c2', wb.id, 'b'), candidate('c3', wb.id, 'd', { verification_status: 'unavailable' })], published_models: [base] });
    assert.equal(logic.needsUnverifiedConfirmation(registry, [{ ...base }]), false);
    assert.equal(logic.needsUnverifiedConfirmation(registry, [{ ...base, extra_native_models: ['b'] }]), true);
    assert.equal(logic.publicationError(registry, [{ ...base, extra_native_models: ['d'] }]), '明确不可用的候选不能上架。 / Unavailable candidates cannot be published.');
    // Existing extras stay exempt while binding is unchanged and record stays enabled...
    const withExtra = { ...base, extra_native_models: ['b'] };
    const held = view({ candidates: registry.candidates, published_models: [withExtra] });
    assert.equal(logic.needsUnverifiedConfirmation(held, [{ ...withExtra }]), false);
    // ...but re-enabling or changing the primary gates every extra again.
    const off = view({ candidates: registry.candidates, published_models: [{ ...withExtra, enabled: false }] });
    assert.equal(logic.needsUnverifiedConfirmation(off, [{ ...withExtra, enabled: true }]), true);
    assert.equal(logic.needsUnverifiedConfirmation(held, [{ ...withExtra, native_model: 'a', extra_native_models: ['b'], public_id: 'other' }]), true);
});
