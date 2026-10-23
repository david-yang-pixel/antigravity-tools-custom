import { useMemo, useState, type ReactNode } from 'react';
import { AlertTriangle, Loader2, Plus, RefreshCw, Search, ShieldCheck, Shuffle } from 'lucide-react';
import type { CandidateModel, ModelRegistryView, ModelVerificationResult, UpstreamDiscoveryReport, WorkBuddyRealm } from '../../../types/modelRegistry';
import { filterCandidates, formatCheckedAt, verificationLabels } from './registryLogic';
import RegistryDialog from './RegistryDialog';

export interface UpstreamPick { source_id: string; realm: WorkBuddyRealm }

export const realmLabels: Record<WorkBuddyRealm, string> = { cn: '国内版 / China', global: '国际版 / Global' };
export const hasRealm = (view: ModelRegistryView, sourceId: string) => view.sources.find(s => s.id === sourceId)?.provider_type === 'workbuddy';

interface Props {
    view: ModelRegistryView;
    pick: UpstreamPick;
    report: UpstreamDiscoveryReport | null;
    busy: string | null;
    testing: string | null;
    tested: Record<string, ModelVerificationResult>;
    notice: ReactNode;
    onPick: (pick: UpstreamPick) => void;
    onFetch: () => void;
    onChangeSource: () => void;
    onTest: (candidate: CandidateModel) => void;
    onAdd: (candidate: CandidateModel) => void;
    onClose: () => void;
}

// Pick a source (and WorkBuddy region), list one randomly sampled account's models, then test
// or add them. Fetching is metadata only; a test is one explicit, real upstream request through
// the sampled account; adding goes through the publication dialog.
export default function UpstreamModels({ view, pick, report, busy, testing, tested, notice, onPick, onFetch, onChangeSource, onTest, onAdd, onClose }: Props) {
    const [query, setQuery] = useState('');
    const fetching = busy === 'discover_upstream_models';
    const sourceName = (id: string) => view.sources.find(s => s.id === id)?.display_name || id;
    const models = useMemo(() => report ? report.candidate_ids.flatMap(id => view.candidates.filter(c => c.id === id)) : [], [report, view.candidates]);
    const visible = useMemo(() => filterCandidates(models, '', query, '', true, view.sources), [models, query, view.sources]);
    const published = (c: CandidateModel) => view.published_models.some(p => p.source_id === c.source_id && p.native_model === c.native_model);
    const sourceEnabled = !!view.sources.find(s => s.id === report?.source_id)?.enabled;

    if (!report) return <RegistryDialog title="获取上游模型 / Fetch upstream models" confirmText={fetching ? '获取中 / Fetching' : '获取 / Fetch'} onConfirm={onFetch} onCancel={onClose} busy={!!busy} blocked={!pick.source_id}>
        {notice}
        <fieldset disabled={!!busy} className="min-w-0 space-y-4">
            <div role="radiogroup" aria-label="上游 / Upstream" className="space-y-2">
                <p className="text-xs font-medium">上游 / Upstream</p>
                <div className="registry-choices">{view.sources.map(s => <label key={s.id} className="registry-choice">
                    <input type="radio" name="upstream-source" className="sr-only" checked={pick.source_id === s.id} onChange={() => onPick({ ...pick, source_id: s.id })} />
                    <span className="font-semibold">{s.display_name}</span>
                    <span className="registry-muted">{s.account_count} 账号 / accounts{s.enabled ? '' : ' · 未启用 / Disabled'}</span>
                </label>)}</div>
            </div>
            {hasRealm(view, pick.source_id) && <div role="radiogroup" aria-label="区域 / Region" className="space-y-2">
                <p className="text-xs font-medium">区域 / Region</p>
                <div className="registry-choices">{(Object.keys(realmLabels) as WorkBuddyRealm[]).map(realm => <label key={realm} className="registry-choice">
                    <input type="radio" name="upstream-realm" className="sr-only" checked={pick.realm === realm} onChange={() => onPick({ ...pick, realm })} />
                    <span className="font-semibold">{realmLabels[realm]}</span>
                </label>)}</div>
            </div>}
            <p className="registry-muted flex items-start gap-2"><Shuffle size={14} className="mt-0.5 shrink-0" aria-hidden />随机抽取一个启用账号读取模型列表（仅元数据，无推理）；该账号失败时最多换 3 个账号。 / Lists models through one random enabled account (metadata only, no inference); falls back to up to 3 accounts.</p>
            {fetching && <p role="status" className="registry-muted flex items-center gap-2"><Loader2 size={14} className="motion-safe:animate-spin" aria-hidden />正在获取上游模型 / Fetching upstream models…</p>}
        </fieldset>
    </RegistryDialog>;

    return <RegistryDialog title="上游模型 / Upstream models" confirmText="完成 / Done" onConfirm={onClose} onCancel={onClose} busy={!!busy}>
        {notice}
        <div className="flex flex-wrap items-start justify-between gap-2">
            <div className="min-w-0 space-y-1">
                <p className="text-xs break-words"><strong>{sourceName(report.source_id)}</strong>{report.realm && ` · ${realmLabels[report.realm]}`} · {models.length} 个模型 / models</p>
                <p className="registry-muted break-words">随机账号 / Random account: <code>{report.account_label}</code>{report.attempts > 1 && ` · 尝试 ${report.attempts} 个账号 / ${report.attempts} tried`}</p>
            </div>
            <div className="flex flex-wrap gap-1">
                <button className="registry-link" disabled={!!busy} onClick={onFetch}>{fetching ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <RefreshCw size={14} aria-hidden />}换一个账号 / Another account</button>
                <button className="registry-link" disabled={!!busy} onClick={onChangeSource}>更改上游 / Change source</button>
            </div>
        </div>
        <div className="registry-notice registry-notice-warning"><AlertTriangle size={16} aria-hidden /><p>「测试」通过上述账号向上游发起一次真实请求，可能消耗额度或产生费用，仅验证基础文本，最多 30 秒，不自动重试。「添加」后模型才进入已上架并允许调用。 / Test sends one real request through this account (may use quota or cost money), basic text only, up to 30s, no retries. Only Add publishes a model.</p></div>
        <label className="relative block"><span className="sr-only">搜索上游模型 / Search upstream models</span><Search size={14} className="absolute left-3 top-3 text-gray-400" aria-hidden /><input className="registry-input pl-9" placeholder="搜索模型 / Search model" value={query} onChange={e => setQuery(e.target.value)} /></label>
        <table className="registry-table registry-upstream-table"><caption className="sr-only">上游模型 / Upstream models</caption><thead><tr><th scope="col">上游模型 / Model</th><th scope="col">测试结果 / Test</th><th scope="col">操作 / Actions</th></tr></thead><tbody>
            {visible.map((c, index) => {
                const result = tested[c.id];
                const unavailable = c.verification_status === 'unavailable';
                return <tr key={c.id} style={{ '--registry-order': Math.min(index, 6) } as React.CSSProperties}>
                    <td data-label="上游模型 / Model"><code className="font-semibold">{c.native_model}</code>{c.display_name && c.display_name !== c.native_model && <p className="registry-muted mt-1">{c.display_name}</p>}</td>
                    <td data-label="测试结果 / Test"><span className={`registry-badge registry-${result?.status ?? c.verification_status}`}>{verificationLabels[result?.status ?? c.verification_status]}</span>
                        {result ? <p className="registry-muted mt-1 break-words">{result.message} · {formatCheckedAt(result.checked_at)}</p> : c.last_verified_at && <p className="registry-muted mt-1">{formatCheckedAt(c.last_verified_at)}</p>}</td>
                    <td data-label="操作 / Actions"><div className="registry-row-actions">
                        <button className="registry-btn" disabled={!!busy} onClick={() => onTest(c)}>{testing === c.id ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <ShieldCheck size={14} aria-hidden />}{testing === c.id ? '测试中 / Testing' : '测试 / Test'}</button>
                        {published(c) ? <span className="registry-badge registry-verified self-center">已上架 / Published</span>
                            : <button className="registry-btn registry-primary" disabled={!!busy || unavailable || !sourceEnabled} title={unavailable ? '测试确认不可用，不能添加 / Unavailable models cannot be added' : !sourceEnabled ? '来源未启用 / Source disabled' : undefined} onClick={() => onAdd(c)}><Plus size={14} aria-hidden />添加 / Add</button>}
                    </div></td>
                </tr>;
            })}
        </tbody></table>
        {!visible.length && <p className="registry-empty">{models.length ? '没有匹配的上游模型。 / No matching upstream models.' : '该账号未返回可用模型，请换一个账号。 / This account listed no models; try another account.'}</p>}
    </RegistryDialog>;
}
