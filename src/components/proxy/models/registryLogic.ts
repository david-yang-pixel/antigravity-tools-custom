import type { CandidateModel, ModelCapability, ModelRegistryView, ModelSource, PublishedModel, RegistryCatalogModel, RegistryProtocol, SaveModelRegistryRequest } from '../../../types/modelRegistry';

export const verificationLabels = {
    unverified: '未验证 / Unverified', verified: '基础文本已验证 / Text verified', unavailable: '不可用 / Unavailable', failed: '验证失败 / Failed',
};
export const capabilityLabels = {
    supported: '已支持 / Supported', conditional: '有条件支持 / Conditional', not_integrated: '尚未接入 / Not integrated', unsupported: '不支持 / Unsupported', unverified: '未验证 / Unverified',
};
export const publicIdKey = (id: string) => id.replace(/^models\//, '');
export const candidateFor = (view: ModelRegistryView, model: PublishedModel) =>
    view.candidates.find(c => c.source_id === model.source_id && c.native_model === model.native_model);

export function newPublicationId(): string {
    // randomUUID is absent on non-secure remote Web origins. IDs are identities,
    // not secrets; retain the generated value in the draft across retries.
    return globalThis.crypto?.randomUUID?.() ?? `publication-${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}-${Math.random().toString(36).slice(2)}`;
}

export function filterCandidates(candidates: CandidateModel[], source: string, query: string, verification: string, showIgnored: boolean, sources: ModelSource[] = []) {
    const text = query.trim().toLocaleLowerCase();
    return candidates.filter(c => (!source || c.source_id === source)
        && (showIgnored || !c.ignored)
        && (!verification || c.verification_status === verification)
        && (!text || [c.native_model, c.display_name, c.source_id, c.provider_type, sources.find(s => s.id === c.source_id)?.display_name ?? ''].some(value => value.toLocaleLowerCase().includes(text))));
}

export function filterPublications(view: ModelRegistryView, source: string, query: string, verification: string, state: string) {
    const text = query.trim().toLocaleLowerCase();
    return view.published_models.filter(p => (!source || p.source_id === source)
        && (!state || p.enabled === (state === 'enabled'))
        && (!verification || (candidateFor(view, p)?.verification_status ?? 'unverified') === verification)
        && (!text || [p.public_id, p.display_name, p.native_model, p.source_id, view.sources.find(s => s.id === p.source_id)?.display_name ?? ''].some(value => value.toLocaleLowerCase().includes(text))));
}

// Only published/enabled bindings enter client selectors. Unknown providers need
// explicit protocol declarations; a public alias never determines compatibility.
export function registryCatalog(view: ModelRegistryView): RegistryCatalogModel[] {
    return view.published_models.filter(p => p.enabled).flatMap(model => {
        const source = view.sources.find(s => s.id === model.source_id);
        if (!source?.enabled) return [];
        const candidate = candidateFor(view, model);
        const image = source.provider_type === 'antigravity' && /image/i.test(model.native_model);
        const protocols = (['openai', 'anthropic', 'gemini'] as RegistryProtocol[]).filter(protocol => {
            const declared = candidate?.capabilities.find(c => c.key === `protocol_${protocol}` || c.key === `${protocol}_protocol`);
            if (declared) return declared.status === 'supported' || declared.status === 'conditional';
            if (source.provider_type === 'antigravity') return protocol !== 'anthropic' || !image;
            if (source.provider_type === 'workbuddy') return protocol !== 'gemini';
            return false;
        });
        return [{ ...model, provider_type: source.provider_type, source_name: source.display_name, protocols, image_generation: image }];
    });
}

export function capabilityScope(capability: ModelCapability): 'evidence' | 'native' | 'gateway' | 'other' {
    if (capability.key === 'text') return 'evidence';
    if (/^(native_|declared_|reasoning$|vision$|tools$)/.test(capability.key)) return 'native';
    if (/^(protocol_|gateway_)|_protocol$/.test(capability.key) || ['thinking_budget', 'thinking_backfill', 'image_cropping', 'agent_compression', 'aliases', 'image_generation', 'global_system_prompt', 'multimodal', 'multimodal_limits'].includes(capability.key)) return 'gateway';
    return 'other';
}

const sameBinding = (previous: PublishedModel | undefined, draft: PublishedModel) => !!previous
    && publicIdKey(previous.public_id) === publicIdKey(draft.public_id)
    && previous.source_id === draft.source_id && previous.native_model === draft.native_model;
const newlyExposed = (previous: PublishedModel | undefined, draft: PublishedModel) => !sameBinding(previous, draft)
    || (!!previous && !previous.enabled && draft.enabled);

export function publicationError(view: ModelRegistryView, drafts: PublishedModel[]): string | null {
    const ids = new Set(drafts.map(d => d.id));
    const names = new Set(view.published_models.filter(p => !ids.has(p.id)).map(p => publicIdKey(p.public_id)));
    for (const model of drafts) {
        const name = publicIdKey(model.public_id);
        if (!name || /[\s\x00-\x1f\x7f*?]/u.test(name)) return '公开 ID 不能为空或含空白、控制字符、通配符。 / Public ID must be nonempty, without whitespace, controls or wildcards.';
        if (names.has(name)) return `公开 ID「${name}」已存在（区分大小写）。 / Public ID already exists (case-sensitive). Choose another alias.`;
        names.add(name);
        if (!model.display_name.trim()) return '请填写显示名称。 / Display name is required.';
        const existing = view.published_models.find(p => p.id === model.id);
        const candidate = candidateFor(view, model);
        if (!existing && !candidate) return '候选已变化或移除，草稿保留。 / Candidate changed or removed; draft retained.';
        if (!(existing?.legacy && sameBinding(existing, model)) && newlyExposed(existing, model) && candidate?.verification_status === 'unavailable') return '明确不可用的候选不能上架。 / Unavailable candidates cannot be published.';
        if (!existing && !view.sources.some(s => s.id === model.source_id && s.enabled)) return '来源未启用，不能新增上架。 / Enable the source before publishing.';
    }
    return null;
}

export function needsUnverifiedConfirmation(view: ModelRegistryView, drafts: PublishedModel[]) {
    return drafts.some(draft => {
        const previous = view.published_models.find(p => p.id === draft.id);
        const grandfathered = previous?.legacy && sameBinding(previous, draft);
        return newlyExposed(previous, draft) && !grandfathered && candidateFor(view, draft)?.verification_status !== 'verified';
    });
}

export function saveRequest(view: ModelRegistryView, published = view.published_models, ignored = view.candidates.filter(c => c.ignored).map(c => c.id), allowUnverified = false): SaveModelRegistryRequest {
    return { revision: view.revision, published_models: published, ignored_candidate_ids: ignored, allow_unverified: allowUnverified };
}

export function mergePublications(view: ModelRegistryView, drafts: PublishedModel[]) {
    const ids = new Set(drafts.map(d => d.id));
    return [...view.published_models.filter(p => !ids.has(p.id)), ...drafts.map(draft => {
        const previous = view.published_models.find(p => p.id === draft.id);
        // Renaming a migrated public ID must not transfer its legacy exemption.
        return { ...draft, legacy: !!previous?.legacy && sameBinding(previous, draft) };
    })];
}

export function verificationError(ids: string[]) {
    return !ids.length ? '请先选择候选。 / Select at least one candidate.' : ids.length > 5 ? '每次最多验证 5 项。 / At most 5 checks per action.'
        : new Set(ids).size !== ids.length ? '选择包含重复候选。 / Duplicate candidate selection.' : null;
}

export function registryError(error: unknown): { message: string; conflict: boolean } {
    let message: string;
    try { message = error instanceof Error ? error.message : typeof error === 'string' ? error : JSON.stringify(error) || '未知错误 / Unknown error'; }
    catch { message = String(error); }
    return { message, conflict: /conflict|revision|冲突|版本|\b409\b/i.test(message) };
}

export function formatCheckedAt(value: number | null) {
    if (!value) return '尚无验证记录 / Never checked';
    const date = new Date(value * 1000);
    return Number.isNaN(date.getTime()) ? '时间不可用 / Invalid timestamp' : date.toLocaleString(undefined, { hour12: false });
}
