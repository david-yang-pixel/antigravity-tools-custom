import { useEffect, useState } from 'react';
import { ChevronDown, Gauge, Globe, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import { showToast } from '../common/ToastContainer';

export interface EgressNode { name: string; type: string; delay: number | null; alive: boolean | null; eligible: boolean; accounts: number }
export interface EgressView {
    enabled: boolean; subscription_configured: boolean; subscription_host: string | null;
    include: string; exclude: string; auto_assign: boolean; env_locked: string[];
    running: boolean; binary: string | null; error: string | null;
    nodes: EgressNode[]; bindings: Record<string, string>;
}
interface Probe { node: string; delay_ms: number | null; error: string | null }
export const DIRECT = 'DIRECT';
const input = 'w-full min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900 px-3 py-2 text-xs outline-none focus:border-blue-500 disabled:opacity-50';
const button = 'inline-flex items-center justify-center gap-1.5 rounded-lg border border-gray-200 dark:border-gray-700 px-3 py-1.5 text-xs hover:bg-gray-50 dark:hover:bg-gray-800 disabled:opacity-50 disabled:cursor-not-allowed';

export function nodeLabel(node: EgressNode, t: (key: string, options?: Record<string, unknown>) => string) {
    if (node.delay) return `${node.name} · ${node.delay} ms`;
    if (node.alive === false) return `${node.name} · ${t('workbuddy.egress.timeout')}`;
    return node.name;
}

/** Subscription, filters and auto-assignment for per-account airport egress. */
export default function WorkBuddyEgressPanel({ view, onChange }: { view: EgressView | null; onChange: (view: EgressView) => void }) {
    const { t } = useTranslation();
    const [open, setOpen] = useState(false);
    const [link, setLink] = useState('');
    const [include, setInclude] = useState('');
    const [exclude, setExclude] = useState('');
    const [busy, setBusy] = useState('');
    const [error, setError] = useState('');
    useEffect(() => { if (view) { setInclude(view.include); setExclude(view.exclude); } }, [view?.include, view?.exclude]);
    if (!view) return null;
    const locked = (field: string) => view.env_locked.includes(field);
    const alive = view.nodes.filter(n => n.alive).length;
    async function run(name: string, command: string, args?: unknown) {
        setBusy(name); setError('');
        try { onChange(await request<EgressView>(command, args)); return true; }
        catch (e) { setError(String(e)); return false; }
        finally { setBusy(''); }
    }
    async function save(changes: Record<string, unknown>) {
        if (await run('save', 'save_workbuddy_egress', { request: changes })) { setLink(''); showToast(t('workbuddy.egress.saved'), 'success'); }
    }
    const status = !view.enabled ? 'off' : view.error ? 'error' : view.running ? 'running' : 'stopped';
    return <div className="rounded-2xl border border-gray-200 bg-white dark:border-gray-800 dark:bg-gray-900">
        <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
            <button type="button" className="flex min-w-0 items-center gap-2 text-left" aria-expanded={open} onClick={() => setOpen(!open)}>
                <Globe size={16} className="shrink-0 text-blue-600" />
                <span className="font-medium">{t('workbuddy.egress.title')}</span>
                <span className={`whitespace-nowrap rounded px-1.5 py-0.5 text-[11px] ${status === 'running' ? 'bg-green-50 text-green-700 dark:bg-green-900/30 dark:text-green-300' : status === 'error' ? 'bg-red-50 text-red-700 dark:bg-red-900/30 dark:text-red-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`workbuddy.egress.status_${status}`)}</span>
                {view.enabled && view.nodes.length > 0 && <span className="truncate text-xs text-gray-500">{t('workbuddy.egress.node_summary', { total: view.nodes.length, alive })}</span>}
                <ChevronDown size={16} className={`shrink-0 text-gray-400 transition-transform ${open ? 'rotate-180' : ''}`} />
            </button>
            <label className="flex items-center gap-2 text-xs">
                <span>{t('workbuddy.enable')}</span>
                <input type="checkbox" className="toggle toggle-sm bg-gray-200 border-gray-300 checked:bg-blue-600 checked:border-blue-600" checked={view.enabled}
                    disabled={!!busy || locked('enabled') || (!view.enabled && !view.subscription_configured && !link.trim())}
                    title={!view.subscription_configured ? t('workbuddy.egress.link_required') : undefined}
                    onChange={e => void save({ enabled: e.target.checked, ...(link.trim() ? { subscription_url: link.trim() } : {}) })} />
            </label>
        </div>
        {(open || (view.enabled && view.error)) && <div className="space-y-3 border-t border-gray-100 px-4 py-4 text-xs dark:border-gray-800">
            {open && <>
                <p className="leading-relaxed text-gray-500 dark:text-gray-400">{t('workbuddy.egress.description')}</p>
                <label className="block space-y-1"><span>{t('workbuddy.egress.subscription')}</span>
                    <input className={`${input} font-mono`} type="password" autoComplete="off" spellCheck={false} value={link} disabled={!!busy || locked('subscription_url')}
                        placeholder={view.subscription_configured ? t('workbuddy.egress.subscription_kept', { host: view.subscription_host || '' }) : 'https://…'}
                        onChange={e => setLink(e.target.value)} /></label>
                <div className="grid gap-3 sm:grid-cols-2">
                    <label className="block space-y-1"><span>{t('workbuddy.egress.include')}</span>
                        <input className={`${input} font-mono`} value={include} disabled={!!busy} placeholder={t('workbuddy.egress.include_placeholder')} onChange={e => setInclude(e.target.value)} /></label>
                    <label className="block space-y-1"><span>{t('workbuddy.egress.exclude')}</span>
                        <input className={`${input} font-mono`} value={exclude} disabled={!!busy} onChange={e => setExclude(e.target.value)} /></label>
                </div>
                <label className="flex items-center gap-2"><input type="checkbox" className="accent-blue-600" checked={view.auto_assign} disabled={!!busy}
                    onChange={e => void save({ auto_assign: e.target.checked })} />{t('workbuddy.egress.auto_assign')}</label>
                <p className="leading-relaxed text-gray-500">{t('workbuddy.egress.fail_closed_hint')}</p>
                {view.binary && <p className="break-all text-gray-500">{t('workbuddy.egress.binary', { path: view.binary })}</p>}
                {view.env_locked.length > 0 && <p className="text-amber-600">{t('workbuddy.env_locked')}</p>}
            </>}
            {(error || (view.enabled && view.error)) && <p role="alert" className="break-words text-red-600">{error || view.error}</p>}
            {open && <div className="flex flex-wrap justify-end gap-2">
                {view.subscription_configured && !locked('subscription_url') && <button type="button" className={`${button} mr-auto text-red-600`} disabled={!!busy} onClick={() => void save({ subscription_url: '', enabled: false })}>{t('workbuddy.egress.clear_link')}</button>}
                <button type="button" className={button} disabled={!!busy || !view.enabled} onClick={() => void run('refresh', 'refresh_workbuddy_egress')}><RefreshCw size={13} className={busy === 'refresh' ? 'animate-spin' : ''} />{t('workbuddy.egress.refresh')}</button>
                <button type="button" className={button} disabled={!!busy || !view.enabled} onClick={() => void run('test', 'test_workbuddy_egress_nodes')}><Gauge size={13} className={busy === 'test' ? 'animate-pulse' : ''} />{t('workbuddy.egress.test_nodes')}</button>
                <button type="button" className="rounded-lg bg-blue-600 px-4 py-1.5 font-medium text-white hover:bg-blue-700 disabled:opacity-50" disabled={!!busy}
                    onClick={() => void save({ include, exclude, ...(link.trim() ? { subscription_url: link.trim() } : {}) })}>{busy === 'save' ? t('common.saving') : t('common.save')}</button>
            </div>}
        </div>}
    </div>;
}

/** One account's node: auto, direct or a fixed node, plus a latency probe on its real path. */
export function WorkBuddyEgressCell({ accountId, view, disabled, onChange }: {
    accountId: string; view: EgressView | null; disabled: boolean; onChange: (view: EgressView) => void;
}) {
    const { t } = useTranslation();
    const [busy, setBusy] = useState(false);
    const [probe, setProbe] = useState<Probe | null>(null);
    const [error, setError] = useState('');
    if (!view?.enabled) return <span className="text-xs text-gray-500">{t('workbuddy.egress.off')}</span>;
    const bound = view.bindings[accountId] ?? '';
    const missing = !!bound && bound !== DIRECT && !view.nodes.some(n => n.name === bound);
    async function select(node: string) {
        setBusy(true); setError(''); setProbe(null);
        try { onChange(await request<EgressView>('set_workbuddy_account_egress', { request: { id: accountId, node: node || null } })); }
        catch (e) { setError(String(e)); }
        finally { setBusy(false); }
    }
    async function test() {
        setBusy(true); setError(''); setProbe(null);
        try { setProbe(await request<Probe>('test_workbuddy_account_egress', { request: { id: accountId } })); }
        catch (e) { setError(String(e)); }
        finally { setBusy(false); }
    }
    return <div className="min-w-48 max-w-64">
        <div className="flex items-center gap-1">
            <select className={`${input} !py-1.5`} aria-label={t('workbuddy.egress.column')} value={bound} disabled={disabled || busy} onChange={e => void select(e.target.value)}>
                <option value="">{t('workbuddy.egress.auto')}</option>
                <option value={DIRECT}>{t('workbuddy.egress.direct')}</option>
                {missing && <option value={bound}>{t('workbuddy.egress.missing', { name: bound })}</option>}
                {view.nodes.map(node => <option key={node.name} value={node.name}>{nodeLabel(node, t)}</option>)}
            </select>
            <button type="button" className="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-gray-500 hover:bg-gray-100 hover:text-gray-900 disabled:opacity-40 dark:hover:bg-gray-800 dark:hover:text-white"
                disabled={disabled || busy} title={t('workbuddy.egress.test_account')} aria-label={t('workbuddy.egress.test_account')} onClick={() => void test()}>
                <Gauge size={15} className={busy ? 'animate-pulse' : ''} /></button>
        </div>
        {probe && <div role="status" className={`mt-1 truncate text-xs ${probe.delay_ms ? 'text-green-700 dark:text-green-400' : 'text-red-600'}`} title={probe.error || probe.node}>
            {probe.delay_ms ? t('workbuddy.egress.latency', { ms: probe.delay_ms }) : probe.error}</div>}
        {error && <div role="alert" className="mt-1 break-words text-xs text-red-600">{error}</div>}
    </div>;
}
