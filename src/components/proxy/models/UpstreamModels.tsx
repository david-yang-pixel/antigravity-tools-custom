import { useMemo, useState } from 'react';
import { AlertTriangle, Loader2, Plus, Search, ShieldCheck, X } from 'lucide-react';
import type { CandidateModel, ModelRegistryView, ModelVerificationResult } from '../../../types/modelRegistry';
import { filterCandidates, formatCheckedAt, verificationLabels } from './registryLogic';

interface Props {
    view: ModelRegistryView;
    busy: string | null;
    testing: string | null;
    tested: Record<string, ModelVerificationResult>;
    sourceName: (sourceId: string) => string;
    onTest: (candidate: CandidateModel) => void;
    onAdd: (candidate: CandidateModel) => void;
    onClose: () => void;
}

// Upstream models that are not yet published. Fetching is metadata only; a test is
// one explicit, real upstream request; adding goes through the publication dialog.
export default function UpstreamModels({ view, busy, testing, tested, sourceName, onTest, onAdd, onClose }: Props) {
    const [source, setSource] = useState('');
    const [query, setQuery] = useState('');
    const unpublished = useMemo(() => view.candidates.filter(c => !view.published_models.some(p => p.source_id === c.source_id && p.native_model === c.native_model)), [view]);
    const visible = useMemo(() => filterCandidates(unpublished, source, query, '', true, view.sources), [unpublished, source, query, view.sources]);
    const sourceEnabled = (c: CandidateModel) => !!view.sources.find(s => s.id === c.source_id)?.enabled;

    return <section className="registry-upstream space-y-3" aria-label="上游模型 / Upstream models">
        <div className="flex flex-wrap items-center justify-between gap-2">
            <h3 className="text-sm font-semibold">上游模型 / Upstream models <span className="registry-muted font-normal">· 未上架 {unpublished.length} / Not published</span></h3>
            <button className="registry-link" disabled={!!busy} onClick={onClose}><X size={14} aria-hidden />收起 / Close</button>
        </div>
        <div className="registry-notice registry-notice-warning"><AlertTriangle size={16} aria-hidden /><p>「测试」会向上游发起一次真实请求，可能消耗额度或产生费用，仅验证基础文本，最多 30 秒，不自动重试。「添加」后模型才进入已上架并允许调用。 / Test sends one real upstream request (may use quota or cost money), basic text only, up to 30s, no retries. Only Add publishes a model.</p></div>
        <div className="flex flex-wrap items-center gap-2">
            <label className="min-w-0 flex-1 basis-52 relative"><span className="sr-only">搜索上游模型 / Search upstream models</span><Search size={14} className="absolute left-3 top-3 text-gray-400" aria-hidden /><input className="registry-input pl-9" placeholder="搜索模型、来源 / Search model, source" value={query} onChange={e => setQuery(e.target.value)} /></label>
            <label><span className="sr-only">上游来源 / Source</span><select className="registry-input" value={source} onChange={e => setSource(e.target.value)}><option value="">全部来源 / All sources</option>{view.sources.map(s => <option key={s.id} value={s.id}>{s.display_name}{s.enabled ? '' : '（未启用 / Disabled）'}</option>)}</select></label>
        </div>
        <table className="registry-table registry-upstream-table"><caption className="sr-only">未上架的上游模型 / Unpublished upstream models</caption><thead><tr><th scope="col">上游模型 / Model</th><th scope="col">来源 / Source</th><th scope="col">测试结果 / Test</th><th scope="col">操作 / Actions</th></tr></thead><tbody>
            {visible.map((c, index) => {
                const result = tested[c.id];
                const unavailable = c.verification_status === 'unavailable';
                return <tr key={c.id} style={{ '--registry-order': Math.min(index, 6) } as React.CSSProperties}>
                    <td data-label="上游模型 / Model"><code className="font-semibold">{c.native_model}</code>{c.display_name && c.display_name !== c.native_model && <p className="registry-muted mt-1">{c.display_name}</p>}</td>
                    <td data-label="来源 / Source"><span className="text-xs font-medium">{sourceName(c.source_id)}</span><p className="registry-muted mt-1">{c.verified_account_count} / {c.account_count} 账号 / accounts</p></td>
                    <td data-label="测试结果 / Test"><span className={`registry-badge registry-${c.verification_status}`}>{verificationLabels[c.verification_status]}</span>
                        {result ? <p className="registry-muted mt-1 break-words">{result.message} · {formatCheckedAt(result.checked_at)}</p> : c.last_verified_at && <p className="registry-muted mt-1">{formatCheckedAt(c.last_verified_at)}</p>}</td>
                    <td data-label="操作 / Actions"><div className="registry-row-actions">
                        <button className="registry-btn" disabled={!!busy} onClick={() => onTest(c)}>{testing === c.id ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <ShieldCheck size={14} aria-hidden />}{testing === c.id ? '测试中 / Testing' : '测试 / Test'}</button>
                        <button className="registry-btn registry-primary" disabled={!!busy || unavailable || !sourceEnabled(c)} title={unavailable ? '测试确认不可用，不能添加 / Unavailable models cannot be added' : !sourceEnabled(c) ? '来源未启用 / Source disabled' : undefined} onClick={() => onAdd(c)}><Plus size={14} aria-hidden />添加 / Add</button>
                    </div></td>
                </tr>;
            })}
        </tbody></table>
        {!visible.length && <p className="registry-empty">{unpublished.length ? '没有匹配的上游模型。 / No matching upstream models.' : view.candidates.length ? '上游模型均已上架。 / Every upstream model is already published.' : '未获取到上游模型，请先配置来源账号。 / No upstream models; configure source accounts first.'}</p>}
    </section>;
}
