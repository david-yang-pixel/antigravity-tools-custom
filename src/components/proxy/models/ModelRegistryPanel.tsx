import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { AlertTriangle, CheckCircle2, CircleHelp, Download, Loader2, RefreshCw, Search } from 'lucide-react';
import { request } from '../../../utils/request';
import type { CandidateModel, ModelRegistryView, ModelVerificationReport, ModelVerificationResult, PublishedModel, RefreshModelCandidatesRequest, SaveModelRegistryRequest, VerifyModelCandidatesRequest } from '../../../types/modelRegistry';
import { candidateFor, capabilityLabels, capabilityScope, filterPublications, formatCheckedAt, mergePublications, needsUnverifiedConfirmation, newPublicationId, publicationError, registryError, saveRequest, verificationLabels } from './registryLogic';
import RegistryDialog from './RegistryDialog';
import UpstreamModels from './UpstreamModels';
import './modelRegistry.css';

type Tab = 'published' | 'advanced';
type Action = { kind: 'publish'; drafts: PublishedModel[]; editing: boolean }
    | { kind: 'remove' | 'toggle'; model: PublishedModel };
type Feedback = { message: string; conflict?: boolean; error?: boolean; retry?: 'load' | 'sync' };

function Verification({ candidate }: { candidate?: CandidateModel }) {
    const status = candidate?.verification_status ?? 'unverified';
    return <span className={`registry-badge registry-${status}`}>{verificationLabels[status]}</span>;
}

export default function ModelRegistryPanel({ advancedContent, initialTab = 'published' }: { advancedContent?: ReactNode; initialTab?: Tab }) {
    const [view, setView] = useState<ModelRegistryView | null>(null);
    const [tab, setTab] = useState<Tab>(initialTab);
    useEffect(() => { if (initialTab) setTab(initialTab); }, [initialTab]);
    const [source, setSource] = useState('');
    const [query, setQuery] = useState('');
    const [verification, setVerification] = useState('');
    const [publicationState, setPublicationState] = useState('');
    const [upstreamOpen, setUpstreamOpen] = useState(false);
    const [testing, setTesting] = useState<string | null>(null);
    const [tested, setTested] = useState<Record<string, ModelVerificationResult>>({});
    const [busy, setBusy] = useState<string | null>('load');
    const [feedback, setFeedback] = useState<Feedback | null>(null);
    const [action, setAction] = useState<Action | null>(null);
    const [confirmed, setConfirmed] = useState(false);
    const [actionError, setActionError] = useState<Feedback | null>(null);
    const [detail, setDetail] = useState<{ candidate?: CandidateModel; model?: PublishedModel } | null>(null);
    const locked = useRef(false);
    const alive = useRef(true);
    const sequence = useRef(0);
    const id = useId();
    const actionRef = useRef(action);
    actionRef.current = action;

    async function load(preserveDraft = false) {
        if (locked.current) return;
        const seq = ++sequence.current;
        locked.current = true;
        setBusy('load');
        try {
            const next = await request<ModelRegistryView>('get_model_registry');
            if (!alive.current || seq !== sequence.current) return;
            setView(next);
            setFeedback(preserveDraft ? { message: '已读取最新版本，草稿保留，请核对后再次确认。 / Latest revision loaded; draft retained. Review before resubmitting.' } : null);
            setActionError(null);
            setConfirmed(false);
        } catch (error) {
            if (alive.current && seq === sequence.current) {
                const message = { ...registryError(error), error: true, retry: 'load' as const };
                setFeedback(message);
                if (preserveDraft) setActionError(message);
            }
        } finally {
            if (seq === sequence.current) { locked.current = false; if (alive.current) setBusy(null); }
        }
    }
    useEffect(() => {
        alive.current = true;
        void load();
        const changed = () => void load(!!actionRef.current);
        window.addEventListener('model-registry-changed', changed);
        return () => { alive.current = false; window.removeEventListener('model-registry-changed', changed); };
    }, []);

    const sourceName = (sourceId: string) => view?.sources.find(s => s.id === sourceId)?.display_name || sourceId;
    const visiblePublished = useMemo(() => view ? filterPublications(view, source, query, verification, publicationState) : [], [view, source, query, verification, publicationState]);

    function openAction(next: Action) {
        setAction(next); setConfirmed(false); setActionError(null);
    }
    function openPublish(candidates: CandidateModel[]) {
        openAction({ kind: 'publish', editing: false, drafts: candidates.map(c => ({
            id: newPublicationId(), public_id: c.native_model, display_name: c.display_name || c.native_model,
            source_id: c.source_id, native_model: c.native_model, enabled: true, legacy: false,
        })) });
    }
    const latestModel = action && (action.kind === 'toggle' || action.kind === 'remove') ? view?.published_models.find(p => p.id === action.model.id) : undefined;
    const toggleDraft = latestModel ? { ...latestModel, enabled: !action || action.kind !== 'toggle' ? latestModel.enabled : !action.model.enabled } : undefined;
    const unverified = !!view && !!action && (action.kind === 'publish' ? needsUnverifiedConfirmation(view, action.drafts)
        : action.kind === 'toggle' && !!toggleDraft && needsUnverifiedConfirmation(view, [toggleDraft]));
    const draftError = view && action?.kind === 'publish' ? (action.editing && !view.published_models.some(p => p.id === action.drafts[0]?.id)
        ? '上架记录已被其他管理员删除，草稿仍保留，请重新选择。 / Publication was removed by another admin; draft retained. Select again.' : publicationError(view, action.drafts)) : null;

    async function mutate(command: 'save_model_registry' | 'refresh_model_candidates' | 'verify_model_candidates', payload: SaveModelRegistryRequest | RefreshModelCandidatesRequest | VerifyModelCandidatesRequest, label: string) {
        if (locked.current) return;
        locked.current = true;
        setBusy(command); setFeedback(null); setActionError(null);
        try {
            const response = await request<ModelRegistryView | ModelVerificationReport>(command, { request: payload });
            const report = 'registry' in response ? response : null;
            const next = report ? report.registry : response as ModelRegistryView;
            window.dispatchEvent(new Event('model-registry-changed'));
            if (!alive.current) return;
            setView(next);
            setAction(null); setConfirmed(false);
            if (report) setTested(prev => ({ ...prev, ...Object.fromEntries(report.results.map(r => [r.candidate_id, r])) }));
            setFeedback({ message: label });
        } catch (error) {
            if (!alive.current) return;
            const reason = registryError(error);
            const message = command === 'verify_model_candidates'
                ? `${reason.message} · 验证失败或结果未知；不自动重试，请重新读取核对。再次发起可能重复消耗额度。 / Check failed or outcome unknown; reload first. Retrying may consume quota again.`
                : `${reason.message} · 未收到成功确认，草稿保留。 / No success confirmation; draft retained.`;
            const failure: Feedback = { ...reason, message, error: true, retry: command === 'refresh_model_candidates' ? 'sync' : command === 'verify_model_candidates' ? 'load' : undefined };
            setFeedback(failure); setActionError(failure); setConfirmed(false);
        } finally {
            locked.current = false;
            if (alive.current) setBusy(null);
        }
    }
    async function submit() {
        if (!view || !action || busy || actionError?.conflict) return;
        const fail = (message: string) => setActionError({ message, error: true });
        if (action.kind === 'publish') {
            if (draftError) return fail(draftError);
            if (unverified && !confirmed) return fail('请明确确认未验证上架风险。 / Explicit unverified publication consent is required.');
            await mutate('save_model_registry', saveRequest(view, mergePublications(view, action.drafts), undefined, unverified && confirmed), '上架信息已保存，来源绑定明确。 / Publication saved with an explicit source binding.');
        } else if (action.kind === 'remove') {
            if (!latestModel) return fail('该记录已移除，请重新加载。 / Publication removed; reload first.');
            await mutate('save_model_registry', saveRequest(view, view.published_models.filter(p => p.id !== latestModel.id)), '已下架，此公开 ID 不再允许推理。 / Unpublished; inference with this public ID is no longer allowed.');
        } else if (toggleDraft) {
            const invalid = publicationError(view, [toggleDraft]);
            if (invalid) return fail(invalid);
            if (unverified && !confirmed) return fail('请明确确认未验证上架风险。 / Explicit unverified publication consent is required.');
            await mutate('save_model_registry', saveRequest(view, mergePublications(view, [toggleDraft]), undefined, unverified && confirmed), toggleDraft.enabled ? '已启用公开模型。 / Publication enabled.' : '已停用，绑定保留。 / Disabled; binding retained.');
        } else fail('记录已变化，请重新加载。 / Publication changed; reload first.');
    }
    const sync = () => void mutate('refresh_model_candidates', { source_id: null }, '已获取上游模型，仅元数据：无推理、无自动上架。 / Upstream models fetched: metadata only, no inference or publication.');
    const fetchUpstream = () => { setUpstreamOpen(true); sync(); };
    // Clicking Test on one model is the explicit consent for that single upstream request.
    async function test(candidate: CandidateModel) {
        setTesting(candidate.id);
        try {
            await mutate('verify_model_candidates', { candidate_ids: [candidate.id], confirm_upstream_usage: true }, `${candidate.native_model} 测试完成，仅基础文本。 / Test finished: basic text only.`);
        } finally { if (alive.current) setTesting(null); }
    }
    const closeAction = () => { if (!busy) { setAction(null); setActionError(null); } };
    const resetFilters = () => { setSource(''); setQuery(''); setVerification(''); setPublicationState(''); };
    const issue = (value: Feedback | null, inDialog = false) => value && <div role={value.error ? 'alert' : 'status'} className={`registry-notice ${value.error ? 'registry-notice-error' : ''}`}>
        {value.error ? <AlertTriangle size={16} aria-hidden /> : <CheckCircle2 size={16} aria-hidden />}
        <div className="min-w-0 flex-1 space-y-2"><p className="break-words">{value.conflict ? '版本冲突：其他管理员已修改目录，本次未覆盖服务端。 / Revision conflict: another admin changed the registry; nothing was overwritten.' : value.message}</p>
            {value.conflict && <p className="text-xs break-words">{value.message} · 重新加载后核对并再次提交，不自动重放。 / Reload, review, then resubmit manually.</p>}
            {(value.conflict || value.retry === 'load') && <button className="registry-btn" disabled={!!busy} onClick={() => void load(inDialog || !!action)}>重新加载{action ? '并保留草稿 / Reload, keep draft' : ' / Reload'}</button>}
            {value.retry === 'sync' && !value.conflict && <button className="registry-btn" disabled={!!busy} onClick={sync}>重试获取（无推理） / Retry fetch</button>}
        </div>
    </div>;

    return <section className="model-registry min-w-0 space-y-4" aria-label="模型管理">
        <header className="flex flex-wrap items-start justify-between gap-3">
            <div className="space-y-1"><h2 className="text-base font-semibold">模型目录 / Model registry <span className="ml-2 text-xs font-normal text-gray-500 dark:text-gray-400">{view ? `修订 / Revision ${view.revision}` : '读取中 / Loading'}</span></h2>
                <p className="registry-muted">发现 ≠ 验证 ≠ 上架。仅已上架且启用的公开 ID 可调用。 / Discovery ≠ verification ≠ publication. Only enabled publications allow inference.</p></div>
            {tab !== 'advanced' && <div className="flex flex-wrap gap-2">
                <button className="registry-btn" disabled={!!busy} onClick={() => void load()}><RefreshCw size={14} aria-hidden />重新加载 / Reload</button>
                <button className="registry-btn registry-primary" disabled={!!busy || !view} onClick={fetchUpstream}><Download size={14} aria-hidden />获取上游模型 / Fetch upstream models</button>
            </div>}
        </header>
        <div role="tablist" aria-label="模型目录分区" className="registry-tabs">
            {([['published', `已上架 / Published${view ? ` · ${view.published_models.length}` : ''}`], ['advanced', '高级 / Advanced']] as const).map(([key, label], index) => <button
                key={key} type="button" role="tab" id={`${id}-${key}`} aria-controls={`${id}-content`} aria-selected={tab === key} tabIndex={tab === key ? 0 : -1}
                className={tab === key ? 'registry-tab registry-tab-active' : 'registry-tab'} onClick={() => setTab(key)}
                onKeyDown={event => {
                    const tabs: Tab[] = ['published', 'advanced'];
                    const next = event.key === 'ArrowRight' || event.key === 'ArrowLeft' ? (index + 1) % 2 : event.key === 'Home' ? 0 : event.key === 'End' ? 1 : -1;
                    if (next >= 0) { event.preventDefault(); setTab(tabs[next]); document.getElementById(`${id}-${tabs[next]}`)?.focus(); }
                }}>{label}</button>)}
        </div>
        {!action && issue(feedback)}
        {busy && <p role="status" className="registry-muted flex items-center gap-2"><Loader2 size={14} className="motion-safe:animate-spin" aria-hidden />{busy === 'verify_model_candidates' ? '正在测试基础文本，最多 30 秒，不自动重试。 / Testing basic text: up to 30s, no automatic retries.' : busy === 'load' ? '正在读取目录 / Loading registry…' : busy === 'refresh_model_candidates' ? '正在获取上游模型（无推理） / Fetching upstream models (no inference)…' : '等待服务端确认，请勿重复提交。 / Waiting for the server; do not resubmit.'}</p>}
        {view?.notices.map((notice, index) => <p key={index} className="registry-muted flex items-start gap-2"><CircleHelp size={14} className="mt-0.5 shrink-0" aria-hidden /><span className="break-words">{notice}</span></p>)}
        <div role="tabpanel" id={`${id}-content`} aria-labelledby={`${id}-${tab}`} aria-busy={!!busy}>
            <div hidden={tab !== 'advanced'} className="space-y-4">
                <div className="registry-notice"><CircleHelp size={16} aria-hidden /><p>来源配置独立保存；以下高级处理仅适用于标注的 AG 链路。WB「尚未接入」不代表原生不支持。设置不构成验证证据，也不会上架模型。 / Source settings save separately. Advanced processing applies only to the labelled AG path. WB not integrated ≠ no native support. Settings are not verification or publication.</p></div>
                {advancedContent || <p className="registry-empty">当前没有高级设置。 / No advanced settings available.</p>}
            </div>
            {tab !== 'advanced' && (view ? <div className="space-y-3">
                {upstreamOpen && <UpstreamModels view={view} busy={busy} testing={testing} tested={tested} sourceName={sourceName} onTest={c => void test(c)} onAdd={c => openPublish([c])} onClose={() => setUpstreamOpen(false)} />}
                <div className="flex flex-wrap items-center gap-2">
                    <label className="min-w-0 flex-1 basis-52 relative"><span className="sr-only">搜索模型 / Search models</span><Search size={14} className="absolute left-3 top-3 text-gray-400" aria-hidden /><input className="registry-input pl-9" placeholder="搜索 ID、名称、来源 / Search ID, name, source" value={query} onChange={e => setQuery(e.target.value)} /></label>
                    <label><span className="sr-only">上游来源 / Source</span><select className="registry-input" value={source} onChange={e => setSource(e.target.value)}><option value="">全部来源 / All sources</option>{view.sources.map(s => <option key={s.id} value={s.id}>{s.display_name} · {s.account_count} 账号 / accounts{s.enabled ? '' : '（未启用 / Disabled）'}</option>)}</select></label>
                    <label><span className="sr-only">基础文本验证状态 / Text status</span><select className="registry-input" value={verification} onChange={e => setVerification(e.target.value)}><option value="">全部验证状态 / All text statuses</option>{Object.entries(verificationLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
                    <label><span className="sr-only">上架状态 / Publication state</span><select className="registry-input" value={publicationState} onChange={e => setPublicationState(e.target.value)}><option value="">全部上架状态 / All states</option><option value="enabled">已启用 / Enabled</option><option value="disabled">已停用 / Disabled</option></select></label>
                </div>
                <p className="registry-muted">{visiblePublished.length} 项 / items · 下架或停用后所有密钥均不可调用；迁移不等于验证。 / Unpublish or disable blocks all keys; migration is not verification.</p>
                <table className="registry-table registry-published"><caption className="sr-only">已上架模型 / Publications</caption><thead><tr><th scope="col">公开 ID / Name</th><th scope="col">来源 / Native target</th><th scope="col">基础文本 / Text</th><th scope="col">状态 / State</th><th scope="col">操作 / Actions</th></tr></thead><tbody>
                    {visiblePublished.map((p, index) => <tr key={p.id} style={{ '--registry-order': Math.min(index, 6) } as React.CSSProperties}>
                        <td data-label="公开 ID / Public ID"><code className="font-semibold">{p.public_id}</code><p className="registry-muted mt-1">{p.display_name}{p.legacy && <span className="registry-badge ml-2">迁移 / Legacy</span>}</p></td>
                        <td data-label="来源 / Target"><span className="text-xs font-medium">{sourceName(p.source_id)}</span><code className="block registry-muted mt-1">{p.native_model}</code></td>
                        <td data-label="基础文本 / Text"><Verification candidate={candidateFor(view, p)} /></td>
                        <td data-label="状态 / State"><button className={`registry-badge registry-state ${p.enabled ? 'registry-verified' : ''}`} disabled={!!busy} aria-label={`${p.public_id}: ${p.enabled ? '停用 / Disable' : '启用 / Enable'}`} onClick={() => openAction({ kind: 'toggle', model: p })}>{p.enabled ? '已启用 / Enabled' : '已停用 / Disabled'}</button></td>
                        <td data-label="操作 / Actions"><div className="registry-row-actions"><button className="registry-link" disabled={!!busy} onClick={() => setDetail({ candidate: candidateFor(view, p), model: p })}>详情 / Details</button><button className="registry-link" disabled={!!busy} onClick={() => openAction({ kind: 'publish', editing: true, drafts: [{ ...p }] })}>编辑 / Edit</button><button className="registry-link registry-danger" disabled={!!busy} onClick={() => openAction({ kind: 'remove', model: p })}>下架 / Unpublish</button></div></td>
                    </tr>)}
                </tbody></table>
                {!visiblePublished.length && <div className="registry-empty"><p>{view.published_models.length ? '没有匹配的上架模型。 / No matching publications.' : '暂无上架模型，发现候选不会开放调用。 / No publications; discovery does not enable inference.'}</p>{!upstreamOpen && <button className="registry-btn mt-3" disabled={!!busy} onClick={() => { resetFilters(); fetchUpstream(); }}><Download size={14} aria-hidden />获取上游模型 / Fetch upstream models</button>}</div>}
            </div> : !busy && <p className="registry-empty">目录不可用时不回退旧路由，请重新加载。 / Registry unavailable; no legacy fallback. Reload to retry.</p>)}
        </div>
        {action && view && <RegistryDialog title={action.kind === 'publish' ? (action.editing ? '编辑公开模型 / Edit publication' : '添加到已上架 / Add to published') : action.kind === 'remove' ? '确认下架 / Unpublish' : toggleDraft?.enabled ? '启用 / Enable' : '停用 / Disable'}
            confirmText={action.kind === 'publish' ? (action.editing ? '保存 / Save' : '添加 / Add') : action.kind === 'remove' ? '确认下架 / Unpublish' : '确认保存 / Save'}
            onConfirm={() => void submit()} onCancel={closeAction} busy={!!busy} destructive={action.kind === 'remove' || (action.kind === 'toggle' && !toggleDraft?.enabled)}
            blocked={!!actionError?.conflict || !!draftError || (unverified && !confirmed)}>
            {issue(actionError, true)}
            <fieldset disabled={!!busy} className="min-w-0 space-y-4">
                {action.kind === 'publish' && <><p className="registry-muted">公开 ID 是客户端名称，明确绑定单个来源与原生目标，记录 ID 保持稳定。更改迁移项公开 ID 会失去旧绑定的免验证资格。 / A public ID binds one source/native target; record identity stays stable. Renaming a legacy ID removes its verification exemption.</p>
                    {action.drafts.map((p, index) => <div key={p.id} className="space-y-2 border-b border-gray-200 dark:border-base-300 pb-4">
                        <p className="text-xs break-words"><strong>{sourceName(p.source_id)}</strong> → <code>{p.native_model}</code></p>
                        <label className="block text-xs font-medium">公开 ID / Alias<input className="registry-input mt-1 font-mono" value={p.public_id} onChange={e => setAction({ ...action, drafts: action.drafts.map((d, i) => i === index ? { ...d, public_id: e.target.value } : d) })} /></label>
                        <label className="block text-xs font-medium">显示名称 / Display name<input className="registry-input mt-1" value={p.display_name} onChange={e => setAction({ ...action, drafts: action.drafts.map((d, i) => i === index ? { ...d, display_name: e.target.value } : d) })} /></label>
                        <Verification candidate={candidateFor(view, p)} />
                    </div>)}{draftError && <p role="alert" className="text-xs text-red-600 dark:text-red-400">{draftError}</p>}</>}
                {(action.kind === 'remove' || action.kind === 'toggle') && <><p className="break-words"><code className="font-semibold">{latestModel?.public_id || action.model.public_id}</code> · {sourceName(latestModel?.source_id || action.model.source_id)}</p><p>{action.kind === 'remove' ? '所有密钥（含主密钥）均不能再调用此 ID，候选与证据保留。 / All keys including master lose access; candidates and evidence retained.' : toggleDraft?.enabled ? '启用后可调用此 ID；不构成其他能力验证。 / Enables this public ID, not verification of any capabilities.' : '所有密钥不能再调用此 ID；来源绑定和别名保留。 / Blocks all keys for this ID; source binding and alias retained.'}</p></>}
                {unverified && <div className="registry-notice registry-notice-warning"><AlertTriangle size={16} aria-hidden /><div className="space-y-3"><p className="font-semibold">未验证上架 / Unverified publication</p><p>尚无基础文本成功证据或最近检查失败；调用可能失败或产生费用。 / No successful basic text evidence, or latest check failed. Calls may fail or incur costs.</p><label className="flex items-start gap-2"><input className="mt-1" type="checkbox" checked={confirmed} onChange={e => setConfirmed(e.target.checked)} /><span>我明确接受风险，允许未验证上架（allow_unverified）。 / I explicitly allow unverified publication.</span></label></div></div>}
            </fieldset>
        </RegistryDialog>}
        {detail && <RegistryDialog title="模型详情与能力范围 / Model capabilities" confirmText="关闭 / Close" onConfirm={() => setDetail(null)} onCancel={() => setDetail(null)}>
            <div className="space-y-2"><code className="font-semibold break-words">{detail.model?.public_id || detail.candidate?.native_model}</code><p className="registry-muted break-words">{sourceName(detail.model?.source_id || detail.candidate?.source_id || '')} → {detail.model?.native_model || detail.candidate?.native_model}</p>{detail.model?.legacy && <p className="registry-muted">迁移保留绑定，不代表已有验证证据。 / Migrated binding, not verification evidence.</p>}</div>
            <section className="space-y-2"><h4 className="font-semibold">实际账号 · 基础文本证据 / Account text evidence</h4><Verification candidate={detail.candidate} />
                {detail.candidate ? <div className="registry-muted space-y-1"><p>{detail.candidate.verified_account_count} / {detail.candidate.account_count} 账号有基础文本成功证据；不保证其他账号、当前额度或高级能力。 / Accounts with basic text evidence only; no guarantee of other accounts, current quota or advanced capabilities.</p><p>最近验证 / Last check: {formatCheckedAt(detail.candidate.last_verified_at)}</p><p>仅有账号汇总，无逐账号明细。 / Aggregate counts only; no per-account history in this response.</p></div> : <p className="registry-muted">无对应候选元数据，能力及账号证据未知。 / No candidate metadata; evidence unknown.</p>}
                {detail.candidate?.capabilities.filter(cap => capabilityScope(cap) === 'evidence').map(cap => <p className="registry-muted" key={cap.key}>{cap.reason}</p>)}
            </section>
            <div className="registry-notice"><CircleHelp size={16} aria-hidden /><p>基础文本验证不证明推理、视觉或工具。「尚未接入」仅描述网关，不代表上游原生不支持。 / Text verification does NOT prove reasoning, vision or tools. Not integrated describes the gateway, not native absence.</p></div>
            {([['native', '原生能力声明（非实测） / Native declarations (not probes)'], ['gateway', '网关实现与适用条件 / Gateway implementation'], ['other', '其他声明（范围未分类） / Other declarations']] as const).map(([scope, label]) => {
                const capabilities = detail.candidate?.capabilities.filter(cap => capabilityScope(cap) === scope) ?? [];
                if (!capabilities.length && scope === 'other') return null;
                return <section key={scope} className="space-y-2"><h4 className="font-semibold">{label}</h4>{!capabilities.length ? <p className="registry-muted">没有可用声明，不推断支持情况。 / No declaration; support unknown.</p> : <dl className="divide-y divide-gray-200 dark:divide-base-300">{capabilities.map(cap => <div className="py-3 space-y-2" key={cap.key}><dt className="flex flex-wrap items-center justify-between gap-2 font-medium">{cap.label}<span className={`registry-badge registry-${cap.status}`}>{capabilityLabels[cap.status] || cap.status}</span></dt><dd className="registry-muted break-words">{cap.reason || '未提供说明 / No further explanation.'}</dd></div>)}</dl>}</section>;
            })}
        </RegistryDialog>}
    </section>;
}
