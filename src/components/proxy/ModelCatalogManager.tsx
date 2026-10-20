import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Download, Loader2, Play, Plus, RefreshCw, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import ModelPricingModal from '../user-token/ModelPricingModal';

type Provider = 'antigravity' | 'workbuddy';
interface Candidate {
    id: string;
    status: 'untested' | 'verified' | 'unsupported' | 'inconclusive';
    checked_at: number | null;
    added: boolean;
}
interface Progress {
    running: boolean; total: number; completed: number;
    verified: number; unsupported: number; inconclusive: number;
    current_model: string | null; started_at: number; errors: string[];
}
const button = 'inline-flex items-center justify-center gap-1.5 rounded-lg border border-gray-200 px-3 py-2 text-xs font-medium hover:bg-gray-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-gray-700 dark:hover:bg-gray-800';
const statusClass = {
    untested: 'bg-gray-100 text-gray-500 dark:bg-gray-800',
    verified: 'bg-green-50 text-green-700 dark:bg-green-900/20 dark:text-green-400',
    unsupported: 'bg-red-50 text-red-600 dark:bg-red-900/20 dark:text-red-400',
    inconclusive: 'bg-amber-50 text-amber-700 dark:bg-amber-900/20 dark:text-amber-400',
};

export default function ModelCatalogManager({ models, onChange }: { models: string[]; onChange: () => void }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    const [pricing, setPricing] = useState(false);
    const [provider, setProvider] = useState<Provider | null>(null);
    const [rows, setRows] = useState<Candidate[]>([]);
    const [selected, setSelected] = useState<Set<string>>(new Set());
    const [progress, setProgress] = useState<Progress | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [query, setQuery] = useState('');
    const dialog = useRef<HTMLDialogElement>(null);
    const generation = useRef(0);
    useEffect(() => {
        if (open) dialog.current?.showModal();
        else dialog.current?.close();
    }, [open]);
    useEffect(() => () => { generation.current += 1; }, []);
    const visible = rows.filter(row => row.id.toLowerCase().includes(query.toLowerCase()));
    const addable = rows.filter(row => selected.has(row.id) && row.status === 'verified' && !row.added);
    const running = !!progress?.running;

    async function load(source: Provider, refresh: boolean) {
        const version = ++generation.current;
        setProvider(source); setBusy(true); setError(''); setSelected(new Set()); setProgress(null);
        if (provider !== source) { setRows([]); setQuery(''); }
        try {
            const [list, status] = await Promise.all([
                request<Candidate[]>('get_model_candidates', { request: { provider: source, refresh } }),
                request<Progress>('get_model_test_status', { provider: source }),
            ]);
            if (generation.current !== version) return;
            setRows(list); setProgress(status);
        } catch (failure) { if (generation.current === version) setError(String(failure)); }
        finally { if (generation.current === version) setBusy(false); }
    }

    useEffect(() => {
        if (!open || !provider || !running) return;
        let cancelled = false;
        let timer: ReturnType<typeof setTimeout>;
        const poll = async () => {
            try {
                const status = await request<Progress>('get_model_test_status', { provider });
                const list = await request<Candidate[]>('get_model_candidates', { request: { provider } });
                if (cancelled) return;
                setProgress(status); setRows(list); setError('');
                if (status.running) timer = setTimeout(poll, 2000);
            } catch (failure) {
                if (!cancelled) { setError(String(failure)); timer = setTimeout(poll, 5000); }
            }
        };
        timer = setTimeout(poll, 1000);
        return () => { cancelled = true; clearTimeout(timer); };
    }, [open, provider, running]);

    async function test(ids: string[]) {
        if (!provider || !ids.length) return;
        setBusy(true); setError('');
        try { setProgress(await request<Progress>('start_model_tests', { request: { provider, model_ids: ids } })); }
        catch (failure) { setError(String(failure)); }
        finally { setBusy(false); }
    }
    async function edit(add: string[], remove: string[]) {
        if (!provider) return;
        setBusy(true); setError('');
        try {
            setRows(await request<Candidate[]>('edit_published_models', { request: { provider, add, remove } }));
            setSelected(new Set()); onChange();
        } catch (failure) { setError(String(failure)); }
        finally { setBusy(false); }
    }
    function close() {
        generation.current += 1;
        setBusy(false); setOpen(false);
    }
    function toggle(id: string) {
        setSelected(previous => { const next = new Set(previous); if (!next.delete(id)) next.add(id); return next; });
    }
    function show() {
        setOpen(true); setError('');
        if (provider) void load(provider, false);
    }

    return <>
        <div className="flex flex-wrap items-center gap-2">
            <button type="button" className={button} onClick={() => setPricing(true)}>{t('proxy.model_catalog.pricing')}</button>
            <button type="button" className={`${button} border-blue-200 text-blue-600 dark:border-blue-800 dark:text-blue-300`} onClick={show}>
                <Download size={14} />{t('proxy.model_catalog.fetch')}
            </button>
        </div>
        <ModelPricingModal open={pricing} models={models} onClose={() => { setPricing(false); onChange(); }} />
        {createPortal(<dialog ref={dialog} onCancel={close} onClose={() => setOpen(false)}
            className="m-auto w-[calc(100%_-_2rem)] max-w-4xl rounded-2xl bg-white p-0 text-gray-900 shadow-2xl backdrop:bg-black/40 dark:bg-gray-900 dark:text-gray-100"
            aria-labelledby="model-catalog-title">
            <div className="flex max-h-[85vh] flex-col">
                <div className="flex items-start justify-between gap-4 border-b border-gray-100 p-5 dark:border-gray-800">
                    <div><h2 id="model-catalog-title" className="text-lg font-semibold">{t('proxy.model_catalog.title')}</h2>
                        <p className="mt-1 text-xs leading-relaxed text-gray-500">{t('proxy.model_catalog.flow')}</p></div>
                    <button type="button" className="rounded-lg p-1.5 hover:bg-gray-100 dark:hover:bg-gray-800" onClick={close} aria-label={t('common.close')}><X size={18} /></button>
                </div>
                <div className="flex flex-wrap items-center gap-2 px-5 py-4">
                    <span className="mr-1 text-xs text-gray-500">{t('proxy.model_catalog.source')}</span>
                    {(['antigravity', 'workbuddy'] as const).map(source => <button key={source} type="button"
                        disabled={busy || running} aria-pressed={source === provider} onClick={() => void load(source, true)}
                        className={`${button} ${source === provider ? '!border-blue-500 bg-blue-50 text-blue-600 dark:bg-blue-900/20' : ''}`}>
                        {source === 'antigravity' ? 'Antigravity' : 'WorkBuddy'}
                    </button>)}
                    {provider && <button type="button" className={button} disabled={busy || running} onClick={() => void load(provider, true)}>
                        <RefreshCw size={13} className={busy ? 'animate-spin' : ''} />{t('proxy.model_catalog.refresh')}
                    </button>}
                    <input className="input input-bordered input-sm ml-auto w-full sm:w-52" value={query} onChange={event => setQuery(event.target.value)}
                        placeholder={t('proxy.model_catalog.search')} aria-label={t('proxy.model_catalog.search')} />
                </div>
                <div className="px-5 pb-3 text-xs leading-relaxed text-gray-500">
                    {t('proxy.model_catalog.test_hint')}
                    {progress && progress.started_at > 0 && <p className="mt-2 break-words text-blue-600 dark:text-blue-300" aria-live="polite">
                        {t('proxy.model_catalog.progress', { completed: progress.completed, total: progress.total, verified: progress.verified, unsupported: progress.unsupported, inconclusive: progress.inconclusive })}
                        {progress.current_model && ` · ${progress.current_model}`}
                    </p>}
                    {(error || progress?.errors.length) ? <p role="alert" className="mt-2 break-words text-red-600">{error || progress?.errors.join('; ')}</p> : null}
                </div>
                <div className="min-h-40 flex-1 overflow-auto border-y border-gray-100 dark:border-gray-800">
                    <table className="w-full text-left text-xs">
                        <thead className="sticky top-0 bg-gray-50 text-gray-500 dark:bg-gray-800"><tr>
                            <th className="w-10 px-5 py-3"><input type="checkbox" className="checkbox checkbox-xs" aria-label={t('proxy.model_catalog.select_all')}
                                checked={visible.length > 0 && visible.every(row => selected.has(row.id))}
                                onChange={event => setSelected(previous => { const next = new Set(previous); for (const row of visible) { if (event.target.checked) next.add(row.id); else next.delete(row.id); } return next; })} /></th>
                            <th className="py-3 font-medium">{t('proxy.supported_models.model_id')}</th>
                            <th className="px-3 py-3 font-medium">{t('proxy.model_catalog.test_result')}</th>
                            <th className="px-5 py-3 text-right font-medium">{t('proxy.supported_models.action')}</th>
                        </tr></thead>
                        <tbody>{visible.map(row => <tr key={row.id} className="border-t border-gray-100 dark:border-gray-800">
                            <td className="px-5 py-3"><input type="checkbox" className="checkbox checkbox-xs" checked={selected.has(row.id)} onChange={() => toggle(row.id)} aria-label={row.id} /></td>
                            <td className="py-3"><span className="break-all font-mono">{row.id}</span>{row.added && <span className="ml-2 text-[10px] text-blue-600">{t('proxy.model_catalog.added')}</span>}</td>
                            <td className="px-3 py-3"><span className={`whitespace-nowrap rounded-md px-2 py-1 ${statusClass[row.status]}`}>{t(`proxy.model_catalog.${row.status}`)}</span>
                                {row.checked_at && <div className="mt-1 whitespace-nowrap text-[10px] text-gray-400">{new Date(row.checked_at).toLocaleString()}</div>}</td>
                            <td className="px-5 py-3 text-right"><div className="flex justify-end gap-3">
                                <button type="button" className="whitespace-nowrap text-blue-600 disabled:opacity-40" disabled={busy || running} onClick={() => void test([row.id])}>{t('proxy.model_catalog.test')}</button>
                                {row.added && <button type="button" className="whitespace-nowrap text-gray-500 disabled:opacity-40" disabled={busy || running} onClick={() => void edit([], [row.id])}>{t('proxy.model_catalog.remove')}</button>}
                            </div></td>
                        </tr>)}</tbody>
                    </table>
                    {!rows.length && <div className="flex items-center justify-center gap-2 px-5 py-12 text-sm text-gray-400">
                        {busy && <Loader2 size={16} className="animate-spin" />}
                        {busy ? t('common.loading') : provider ? t('proxy.model_catalog.empty') : t('proxy.model_catalog.choose')}
                    </div>}
                </div>
                <div className="flex flex-wrap items-center justify-between gap-3 p-5">
                    <span className="text-xs text-gray-500">{t('proxy.model_catalog.selected', { count: selected.size })}</span>
                    <div className="flex flex-wrap gap-2">
                        <button type="button" className={button} disabled={busy || running || !selected.size} onClick={() => void test([...selected])}>
                            {running ? <Loader2 size={13} className="animate-spin" /> : <Play size={13} />}{t('proxy.model_catalog.test_selected')}
                        </button>
                        <button type="button" className={`${button} !border-blue-600 !bg-blue-600 text-white hover:!bg-blue-700`}
                            disabled={busy || running || !addable.length} onClick={() => void edit(addable.map(row => row.id), [])}>
                            <Plus size={14} />{t('proxy.model_catalog.add_selected', { count: addable.length })}
                        </button>
                    </div>
                </div>
            </div>
        </dialog>, document.body)}
    </>;
}
