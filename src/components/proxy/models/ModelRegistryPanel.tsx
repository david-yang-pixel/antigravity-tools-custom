import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { AlertTriangle, CheckCircle2, ChevronRight, CircleHelp, EyeOff, Loader2, RefreshCw, Search, ShieldCheck, Upload, X } from 'lucide-react';
import { request } from '../../../utils/request';
import type { CandidateModel, ModelRegistryView, ModelVerificationReport, ModelVerificationResult, PublishedModel, RefreshModelCandidatesRequest, SaveModelRegistryRequest, VerifyModelCandidatesRequest } from '../../../types/modelRegistry';
import { candidateFor, capabilityLabels, capabilityScope, filterCandidates, filterPublications, formatCheckedAt, mergePublications, needsUnverifiedConfirmation, newPublicationId, publicationError, registryError, saveRequest, verificationError, verificationLabels } from './registryLogic';
import RegistryDialog from './RegistryDialog';
import './modelRegistry.css';

type Tab = 'published' | 'candidates' | 'advanced';
type Action = { kind: 'publish'; drafts: PublishedModel[]; editing: boolean }
    | { kind: 'verify'; ids: string[] }
    | { kind: 'ignore'; ids: string[]; ignored: boolean }
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
    const [showIgnored, setShowIgnored] = useState(false);
    const [selected, setSelected] = useState<Set<string>>(new Set());
    const [busy, setBusy] = useState<string | null>('load');
    const [feedback, setFeedback] = useState<Feedback | null>(null);
    const [action, setAction] = useState<Action | null>(null);
    const [confirmed, setConfirmed] = useState(false);
    const [actionError, setActionError] = useState<Feedback | null>(null);
    const [detail, setDetail] = useState<{ candidate?: CandidateModel; model?: PublishedModel } | null>(null);
    const [results, setResults] = useState<ModelVerificationResult[] | null>(null);
    const locked = useRef(false);
    const alive = useRef(true);
    const sequence = useRef(0);
    const id = useId();
    const selectAll = useRef<HTMLInputElement>(null);
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
            setSelected(prev => new Set([...prev].filter(key => next.candidates.some(c => c.id === key))));
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
    const visibleCandidates = useMemo(() => filterCandidates(view?.candidates ?? [], source, query, verification, showIgnored, view?.sources), [view, source, query, verification, showIgnored]);
    const visiblePublished = useMemo(() => view ? filterPublications(view, source, query, verification, publicationState) : [], [view, source, query, verification, publicationState]);
    const selectedCandidates = (view?.candidates ?? []).filter(c => selected.has(c.id));
    const allVisibleSelected = visibleCandidates.length > 0 && visibleCandidates.every(c => selected.has(c.id));
    useEffect(() => {
        if (selectAll.current) selectAll.current.indeterminate = !allVisibleSelected && visibleCandidates.some(c => selected.has(c.id));
    }, [visibleCandidates, selected, allVisibleSelected]);

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
            setAction(null); setConfirmed(false); setSelected(new Set());
            if (report) setResults(report.results);
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
        } else if (action.kind === 'verify') {
            const invalid = verificationError(action.ids);
            if (invalid) return fail(invalid);
            if (!confirmed) return fail('请确认可能消耗上游额度或费用。 / Confirm upstream usage/cost first.');
            await mutate('verify_model_candidates', { candidate_ids: action.ids, confirm_upstream_usage: true }, '基础文本检查完成，请查看逐项结果，不证明其他能力。 / Basic text checks complete; see individual results. No advanced capabilities proven.');
        } else if (action.kind === 'ignore') {
            const ignored = new Set(view.candidates.filter(c => c.ignored).map(c => c.id));
            action.ids.forEach(key => action.ignored ? ignored.add(key) : ignored.delete(key));
            await mutate('save_model_registry', saveRequest(view, undefined, [...ignored]), action.ignored ? '已保存忽略选择，上架绑定不变。 / Ignored selection saved; publications unchanged.' : '已恢复候选显示。 / Candidates restored.');
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
    const sync = () => void mutate('refresh_model_candidates', { source_id: source || null }, '候选已同步，仅元数据：无推理、无自动上架。 / Metadata synced: no inference, no automatic publication.');
    const closeAction = () => { if (!busy) { setAction(null); setActionError(null); } };
    const resetFilters = () => { setSource(''); setQuery(''); setVerification(''); setPublicationState(''); setShowIgnored(false); };
    const issue = (value: Feedback | null, inDialog = false) => value && <div role={value.error ? 'alert' : 'status'} className={`registry-notice ${value.error ? 'registry-notice-error' : ''}`}>
        {value.error ? <AlertTriangle size={16} aria-hidden /> : <CheckCircle2 size={16} aria-hidden />}
        <div className="min-w-0 flex-1 space-y-2"><p className="break-words">{value.conflict ? '版本冲突：其他管理员已修改目录，本次未覆盖服务端。 / Revision conflict: another admin changed the registry; nothing was overwritten.' : value.message}</p>
            {value.conflict && <p className="text-xs break-words">{value.message} · 重新加载后核对并再次提交，不自动重放。 / Reload, review, then resubmit manually.</p>}
            {(value.conflict || value.retry === 'load') && <button className="registry-btn" disabled={!!busy} onClick={() => void load(inDialog || !!action)}>重新加载{action ? '并保留草稿 / Reload, keep draft' : ' / Reload'}</button>}
            {value.retry === 'sync' && !value.conflict && <button className="registry-btn" disabled={!!busy} onClick={sync}>重试同步（无推理） / Retry metadata sync</button>}
        </div>
    </div>;

    return <section className="model-registry min-w-0 space-y-4" aria-label="模型管理">
        <header className="flex flex-wrap items-start justify-between gap-3">
            <div className="space-y-1"><h2 className="text-base font-semibold">模型目录 / Model registry <span className="ml-2 text-xs font-normal text-gray-500 dark:text-gray-400">{view ? `修订 / Revision ${view.revision}` : '读取中 / Loading'}</span></h2>
                <p className="registry-muted">发现 ≠ 验证 ≠ 上架。仅已上架且启用的公开 ID 可调用。 / Discovery ≠ verification ≠ publication. Only enabled publications allow inference.</p></div>
            {tab !== 'advanced' && <div className="flex flex-wrap gap-2">
                <button className="registry-btn" disabled={!!busy} onClick={() => void load()}><RefreshCw size={14} aria-hidden />重新加载 / Reload</button>
                <button className="registry-btn" disabled={!!busy || !view} onClick={sync}><RefreshCw size={14} aria-hidden />同步候选 / Sync<span className="registry-muted">· 无推理 / No inference</span></button>
            </div>}
        </header>
        <div role="tablist" aria-label="模型目录分区" className="registry-tabs">
            {([['published', `已上架 / Published${view ? ` · ${view.published_models.length}` : ''}`], ['candidates', `上游候选 / Candidates${view ? ` · ${view.candidates.length}` : ''}`], ['advanced', '高级 / Advanced']] as const).map(([key, label], index) => <button
                key={key} type="button" role="tab" id={`${id}-${key}`} aria-controls={`${id}-content`} aria-selected={tab === key} tabIndex={tab === key ? 0 : -1}
                className={tab === key ? 'registry-tab registry-tab-active' : 'registry-tab'} onClick={() => setTab(key)}
                onKeyDown={event => {
                    const tabs: Tab[] = ['published', 'candidates', 'advanced'];
                    const next = event.key === 'ArrowRight' ? (index + 1) % 3 : event.key === 'ArrowLeft' ? (index + 2) % 3 : event.key === 'Home' ? 0 : event.key === 'End' ? 2 : -1;
                    if (next >= 0) { event.preventDefault(); setTab(tabs[next]); document.getElementById(`${id}-${tabs[next]}`)?.focus(); }
                }}>{label}</button>)}
        </div>
        {!action && issue(feedback)}
        {busy && <p role="status" className="registry-muted flex items-center gap-2"><Loader2 size={14} className="motion-safe:animate-spin" aria-hidden />{busy === 'verify_model_candidates' ? '正在验证基础文本；每项最多 30 秒，不自动重试。 / Basic text checks: up to 30s each, no automatic retries.' : busy === 'load' ? '正在读取目录 / Loading registry…' : '等待服务端确认，请勿重复提交。 / Waiting for the server; do not resubmit.'}</p>}
        {view?.notices.map((notice, index) => <p key={index} className="registry-muted flex items-start gap-2"><CircleHelp size={14} className="mt-0.5 shrink-0" aria-hidden /><span className="break-words">{notice}</span></p>)}
        <div role="tabpanel" id={`${id}-content`} aria-labelledby={`${id}-${tab}`} aria-busy={!!busy}>
            <div hidden={tab !== 'advanced'} className="space-y-4">
                <div className="registry-notice"><CircleHelp size={16} aria-hidden /><p>来源配置独立保存；以下高级处理仅适用于标注的 AG 链路。WB「尚未接入」不代表原生不支持。设置不构成验证证据，也不会上架模型。 / Source settings save separately. Advanced processing applies only to the labelled AG path. WB not integrated ≠ no native support. Settings are not verification or publication.</p></div>
                {advancedContent || <p className="registry-empty">当前没有高级设置。 / No advanced settings available.</p>}
            </div>
            {tab !== 'advanced' && (view ? <div className="space-y-3">
                <div className="flex flex-wrap items-center gap-2">
                    <label className="min-w-0 flex-1 basis-52 relative"><span className="sr-only">搜索模型 / Search models</span><Search size={14} className="absolute left-3 top-3 text-gray-400" aria-hidden /><input className="registry-input pl-9" placeholder="搜索 ID、名称、来源 / Search ID, name, source" value={query} onChange={e => setQuery(e.target.value)} /></label>
                    <label><span className="sr-only">上游来源 / Source</span><select className="registry-input" value={source} onChange={e => setSource(e.target.value)}><option value="">全部来源 / All sources</option>{view.sources.map(s => <option key={s.id} value={s.id}>{s.display_name} · {s.account_count} 账号 / accounts{s.enabled ? '' : '（未启用 / Disabled）'}</option>)}</select></label>
                    <label><span className="sr-only">基础文本验证状态 / Text status</span><select className="registry-input" value={verification} onChange={e => setVerification(e.target.value)}><option value="">全部验证状态 / All text statuses</option>{Object.entries(verificationLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
                    {tab === 'published' ? <label><span className="sr-only">上架状态 / Publication state</span><select className="registry-input" value={publicationState} onChange={e => setPublicationState(e.target.value)}><option value="">全部上架状态 / All states</option><option value="enabled">已启用 / Enabled</option><option value="disabled">已停用 / Disabled</option></select></label> :
                        <label className="flex items-center gap-2 text-xs px-1 py-2"><input type="checkbox" checked={showIgnored} onChange={e => setShowIgnored(e.target.checked)} />显示已忽略 / Show ignored</label>}
                </div>
                {tab === 'published' ? <>
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
                    {!visiblePublished.length && <div className="registry-empty"><p>{view.published_models.length ? '没有匹配的上架模型。 / No matching publications.' : '暂无上架模型，发现候选不会开放调用。 / No publications; discovery does not enable inference.'}</p><button className="registry-btn mt-3" onClick={() => { resetFilters(); setTab('candidates'); }}>前往候选 / Browse candidates<ChevronRight size={14} aria-hidden /></button></div>}
                </> : <>
                    <p className="registry-muted">{visibleCandidates.length} / {view.candidates.length} 项 / items · 账号元数据不是可用性证明，验证须主动确认。 / Account metadata is not availability evidence; checks require explicit consent.</p>
                    {selected.size > 0 && <div className="registry-bulk" role="region" aria-label="候选批量操作 / Selected candidate actions"><strong className="text-xs">已选 / Selected {selectedCandidates.length}{selectedCandidates.some(c => !visibleCandidates.some(v => v.id === c.id)) ? '（含筛选外 / includes hidden）' : ''}</strong>
                        <button className="registry-btn registry-primary" disabled={!!busy || !selectedCandidates.length || selectedCandidates.some(c => c.verification_status === 'unavailable' || !view.sources.find(s => s.id === c.source_id)?.enabled)} onClick={() => openPublish(selectedCandidates)}><Upload size={14} aria-hidden />上架 / Publish</button>
                        <button className="registry-btn" disabled={!!busy || !!verificationError(selectedCandidates.map(c => c.id))} onClick={() => openAction({ kind: 'verify', ids: selectedCandidates.map(c => c.id) })}><ShieldCheck size={14} aria-hidden />验证基础文本 / Verify text</button>
                        <button className="registry-btn" disabled={!!busy} onClick={() => openAction({ kind: 'ignore', ids: selectedCandidates.map(c => c.id), ignored: true })}><EyeOff size={14} aria-hidden />忽略 / Ignore</button>
                        {selectedCandidates.some(c => c.ignored) && <button className="registry-btn" disabled={!!busy} onClick={() => openAction({ kind: 'ignore', ids: selectedCandidates.map(c => c.id), ignored: false })}>恢复 / Restore</button>}
                        <button className="registry-link ml-auto" disabled={!!busy} onClick={() => setSelected(new Set())}><X size={14} aria-hidden />清除选择 / Clear</button>
                        {selected.size > 5 && <p className="w-full text-xs">每次最多验证 5 项，请减少选择。 / At most 5 checks per action; reduce selection.</p>}
                    </div>}
                    <table className="registry-table registry-candidates"><caption className="sr-only">上游候选 / Candidates</caption><thead><tr><th scope="col" className="registry-check"><input ref={selectAll} type="checkbox" aria-label="选择当前筛选全部候选 / Select all filtered candidates" checked={allVisibleSelected} disabled={!!busy || !visibleCandidates.length} onChange={e => setSelected(prev => { const next = new Set(prev); visibleCandidates.forEach(c => e.target.checked ? next.add(c.id) : next.delete(c.id)); return next; })} /></th><th scope="col">原生模型 / Name</th><th scope="col">来源 / Source</th><th scope="col">基础文本 / Account evidence</th><th scope="col">操作 / Actions</th></tr></thead><tbody>
                        {visibleCandidates.map((c, index) => <tr key={c.id} aria-selected={selected.has(c.id)} style={{ '--registry-order': Math.min(index, 6) } as React.CSSProperties}>
                            <td className="registry-check"><input type="checkbox" aria-label={`选择 ${sourceName(c.source_id)} / ${c.native_model}`} disabled={!!busy} checked={selected.has(c.id)} onChange={e => setSelected(prev => { const next = new Set(prev); e.target.checked ? next.add(c.id) : next.delete(c.id); return next; })} /></td>
                            <td data-label="原生模型 / Native model"><code className="font-semibold">{c.native_model}</code><p className="registry-muted mt-1">{c.display_name}{c.ignored && <span className="registry-badge ml-2">已忽略 / Ignored</span>}{view.published_models.some(p => p.source_id === c.source_id && p.native_model === c.native_model) && <span className="registry-badge ml-2">已有绑定 / Published binding</span>}</p></td>
                            <td data-label="来源 / Source"><span className="text-xs font-medium">{sourceName(c.source_id)}</span><p className="registry-muted mt-1">{c.provider_type}</p></td>
                            <td data-label="基础文本 / Text"><Verification candidate={c} /><p className="registry-muted mt-1">{c.verified_account_count} / {c.account_count} 账号文本证据 / accounts with text evidence</p></td>
                            <td data-label="操作 / Actions"><div className="registry-row-actions"><button className="registry-link" disabled={!!busy} onClick={() => setDetail({ candidate: c })}>详情 / Details</button><button className="registry-link" disabled={!!busy || c.verification_status === 'unavailable' || !view.sources.find(s => s.id === c.source_id)?.enabled} onClick={() => openPublish([c])}>上架 / Publish</button><button className="registry-link" disabled={!!busy} onClick={() => openAction({ kind: 'ignore', ids: [c.id], ignored: !c.ignored })}>{c.ignored ? '恢复 / Restore' : '忽略 / Ignore'}</button></div></td>
                        </tr>)}
                    </tbody></table>
                    {!visibleCandidates.length && <div className="registry-empty"><p>{view.candidates.length ? '没有匹配候选，请调整筛选。 / No matching candidates; adjust filters.' : '尚无候选，请先配置来源账号，再同步元数据。 / No candidates; configure source accounts and sync metadata.'}</p><button className="registry-btn mt-3" onClick={resetFilters}>重置筛选 / Reset filters</button></div>}
                </>}
            </div> : !busy && <p className="registry-empty">目录不可用时不回退旧路由，请重新加载。 / Registry unavailable; no legacy fallback. Reload to retry.</p>)}
        </div>
        {action && view && <RegistryDialog title={action.kind === 'publish' ? (action.editing ? '编辑公开模型 / Edit publication' : `上架 / Publish · ${action.drafts.length}`) : action.kind === 'verify' ? `基础文本验证 / Verify text · ${action.ids.length}` : action.kind === 'ignore' ? (action.ignored ? '忽略候选 / Ignore' : '恢复候选 / Restore') : action.kind === 'remove' ? '确认下架 / Unpublish' : toggleDraft?.enabled ? '启用 / Enable' : '停用 / Disable'}
            confirmText={action.kind === 'verify' ? '确认额度消耗并验证 / Accept cost & verify' : action.kind === 'publish' ? '保存上架 / Save publication' : action.kind === 'remove' ? '确认下架 / Unpublish' : '确认保存 / Save'}
            onConfirm={() => void submit()} onCancel={closeAction} busy={!!busy} destructive={action.kind === 'remove' || (action.kind === 'toggle' && !toggleDraft?.enabled)}
            blocked={!!actionError?.conflict || !!draftError || (unverified && !confirmed) || (action.kind === 'verify' && (!confirmed || !!verificationError(action.ids)))}>
            {issue(actionError, true)}
            <fieldset disabled={!!busy} className="min-w-0 space-y-4">
                {action.kind === 'publish' && <><p className="registry-muted">公开 ID 是客户端名称，明确绑定单个来源与原生目标，记录 ID 保持稳定。更改迁移项公开 ID 会失去旧绑定的免验证资格。 / A public ID binds one source/native target; record identity stays stable. Renaming a legacy ID removes its verification exemption.</p>
                    {action.drafts.map((p, index) => <div key={p.id} className="space-y-2 border-b border-gray-200 dark:border-base-300 pb-4">
                        <p className="text-xs break-words"><strong>{sourceName(p.source_id)}</strong> → <code>{p.native_model}</code></p>
                        <label className="block text-xs font-medium">公开 ID / Alias<input className="registry-input mt-1 font-mono" value={p.public_id} onChange={e => setAction({ ...action, drafts: action.drafts.map((d, i) => i === index ? { ...d, public_id: e.target.value } : d) })} /></label>
                        <label className="block text-xs font-medium">显示名称 / Display name<input className="registry-input mt-1" value={p.display_name} onChange={e => setAction({ ...action, drafts: action.drafts.map((d, i) => i === index ? { ...d, display_name: e.target.value } : d) })} /></label>
                        <Verification candidate={candidateFor(view, p)} />
                    </div>)}{draftError && <p role="alert" className="text-xs text-red-600 dark:text-red-400">{draftError}</p>}</>}
                {action.kind === 'verify' && <><div className="registry-notice registry-notice-warning"><AlertTriangle size={16} aria-hidden /><p>将发起真实上游请求，可能消耗额度或产生费用。仅基础文本，不验证推理、视觉或工具。每次 1–5 项，每项最多 30 秒，不自动重试。 / Real upstream requests may consume quota or cost money. Basic text only, NOT reasoning, vision or tools. Select 1–5, up to 30s each, no automatic retries.</p></div>
                    <ul className="list-disc pl-5 space-y-1 text-xs">{action.ids.map(key => { const c = view.candidates.find(c => c.id === key); return <li className="break-words" key={key}>{c ? `${sourceName(c.source_id)} / ${c.native_model}` : key}</li>; })}</ul>
                    <label className="flex items-start gap-2 text-sm"><input className="mt-1" type="checkbox" checked={confirmed} onChange={e => setConfirmed(e.target.checked)} /><span>我明确同意本次 {action.ids.length} 项验证可能消耗上游额度或费用。 / I explicitly accept upstream usage/cost for these {action.ids.length} checks.</span></label>
                    {!!verificationError(action.ids) && <p role="alert">{verificationError(action.ids)}</p>}</>}
                {action.kind === 'ignore' && <p>{action.ignored ? '忽略 / Ignore' : '恢复 / Restore'} · {action.ids.length} 项。只影响候选展示，不会下架，也不调用推理。 / Only candidate visibility changes; no unpublishing or inference.</p>}
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
        {results && <RegistryDialog title="基础文本验证结果 / Basic text results" confirmText="关闭 / Close" onConfirm={() => setResults(null)} onCancel={() => setResults(null)}>
            <p className="registry-muted">仅本次基础文本，不覆盖高级能力，不自动重试。 / Basic text only; no advanced capabilities proven, no automatic retries.</p>
            {!results.length && <p>未返回逐项结果，请重新加载核对。 / No individual results; reload to review.</p>}
            <ul className="space-y-4">{results.map((r, index) => { const c = view?.candidates.find(c => c.id === r.candidate_id); return <li className="space-y-1 border-b border-gray-200 dark:border-base-300 pb-3" key={`${r.candidate_id}-${index}`}><p className="text-xs font-medium break-words">{c ? `${sourceName(c.source_id)} / ${c.native_model}` : r.candidate_id}</p><span className={`registry-badge registry-${r.status}`}>{verificationLabels[r.status]}</span><p className="text-sm break-words">{r.message}</p><p className="registry-muted break-words">{formatCheckedAt(r.checked_at)} · {r.account_id ? `本次账号 / Account ID: ${r.account_id}` : '未返回账号证据 / No account evidence returned'}</p></li>; })}</ul>
        </RegistryDialog>}
    </section>;
}
