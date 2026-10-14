import { useEffect, useRef, useState } from 'react';
import { Activity, ArrowRight, KeyRound, LogOut, RefreshCw, ShieldCheck } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { toQuotaMillions } from '../components/user-token/tokenQuota';
import TokenConnectionDetails from '../components/user-token/TokenConnectionDetails';
import TokenUsageDetails, { LogTimeRange, UsageLogPage, UsagePeriod } from '../components/user-token/TokenUsageDetails';

interface ModelUsage {
    model: string;
    requests: number;
    failed_requests: number;
    input_tokens: number;
    output_tokens: number;
    last_used_at: number;
    cached_tokens: number | null;
    cache_recorded_requests: number;
}
interface Overview {
    username: string;
    token_hint: string;
    expired: boolean;
    expires_at: number | null;
    max_concurrency: number;
    token_quota: number;
    total_tokens_used: number;
    remaining_tokens: number | null;
    total_requests: number;
    allowed_models: string[];
    available_models: string[];
    logs: UsageLogPage;
    daily: UsagePeriod[];
    weekly: UsagePeriod[];
    models: ModelUsage[];
}

const portalMenus = ['overview', 'models', 'statistics', 'logs'] as const;
type PortalMenu = typeof portalMenus[number];

export default function TokenPortal() {
    const { t, i18n } = useTranslation();
    const [input, setInput] = useState('');
    const [menu, setMenu] = useState<PortalMenu>('overview');
    const [overview, setOverview] = useState<Overview | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [search, setSearch] = useState('');
    const [logRange, setLogRange] = useState<LogTimeRange>({});
    const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
    const key = useRef('');
    const pending = useRef<AbortController | null>(null);
    useEffect(() => () => pending.current?.abort(), []);

    function logout() {
        pending.current?.abort();
        pending.current = null;
        key.current = '';
        setMenu('overview');
        setLogRange({});
        setInput(''); setOverview(null); setSearch(''); setUpdatedAt(null); setError(''); setBusy(false);
    }

    async function load(token: string, page = 1, range: LogTimeRange = logRange) {
        pending.current?.abort();
        const controller = new AbortController();
        pending.current = controller;
        setBusy(true); setError('');
        try {
            const query = new URLSearchParams({ page: String(page) });
            if (range.start_time !== undefined) query.set('start_time', String(range.start_time));
            if (range.end_time !== undefined) query.set('end_time', String(range.end_time));
            const response = await fetch(`/api/token-portal/me?${query}`, {
                headers: { Authorization: `Bearer ${token}` },
                cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer', signal: controller.signal,
            });
            if (response.status === 401) {
                if (controller.signal.aborted) return;
                logout(); setError(t('token_portal.invalid_token')); return;
            }
            if (!response.ok) throw new Error('unavailable');
            const result: Overview = await response.json();
            if (controller.signal.aborted) return;
            key.current = token;
            setLogRange(range);
            setInput(''); setOverview(result); setUpdatedAt(new Date());
        } catch {
            if (!controller.signal.aborted) setError(t('token_portal.load_error'));
        } finally {
            if (pending.current === controller) { setBusy(false); pending.current = null; }
        }
    }

    const unlimited = t('user_token.unlimited');
    const million = (n: number) => `${toQuotaMillions(n)} M tokens`;
    const exhausted = overview?.remaining_tokens === 0;
    const status = overview?.expired ? t('token_portal.expired') : exhausted ? t('user_token.quota_exhausted') : t('token_portal.active');
    const rows = overview?.models.filter(m => m.model.toLowerCase().includes(search.toLowerCase())) ?? [];
    const usedPercent = overview && overview.token_quota > 0 ? Math.min(100, overview.total_tokens_used / overview.token_quota * 100) : 0;
    const date = (ts: number) => new Date(ts * 1000).toLocaleString(i18n.language, { timeZone: 'Asia/Shanghai' });

    return (
        <div className="min-h-screen bg-slate-50 text-slate-900">
            <header className="border-b border-slate-200 bg-white">
                <div className="max-w-6xl mx-auto px-5 sm:px-8 h-20 flex items-center justify-between gap-4">
                    <div className="flex items-center gap-3"><Activity size={24} className="text-blue-600" /><span className="font-semibold tracking-tight">Stellar AI <span className="text-slate-400 font-normal hidden sm:inline">/ {t('token_portal.title')}</span></span></div>
                    <div className="flex items-center gap-3 text-sm">
                        <button className="rounded-lg px-3 py-2 hover:bg-slate-100 focus-visible:ring-2 focus-visible:ring-blue-500" onClick={() => i18n.changeLanguage(i18n.language.startsWith('zh') ? 'en' : 'zh')} aria-label={t('token_portal.language')}>{i18n.language.startsWith('zh') ? 'EN' : '中文'}</button>
                        {overview && <button onClick={logout} className="flex items-center gap-2 rounded-lg px-3 py-2 hover:bg-slate-100"><LogOut size={16} />{t('token_portal.logout')}</button>}
                    </div>
                </div>
                {overview && <nav aria-label={t('token_portal.navigation')} className="max-w-6xl mx-auto px-5 sm:px-8 flex gap-1 sm:gap-6 overflow-x-auto">
                    {portalMenus.map(item => <button key={item} type="button" aria-current={menu === item ? 'page' : undefined}
                        aria-controls={`portal-panel-${item}`} onClick={() => setMenu(item)}
                        className={`shrink-0 border-b-2 px-3 sm:px-4 py-3 text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500 ${menu === item ? 'border-blue-600 text-blue-600' : 'border-transparent text-slate-500 hover:text-slate-900 hover:border-slate-300'}`}>
                        {t(`token_portal.menu_${item}`)}
                    </button>)}
                </nav>}
            </header>
            {!overview ? (
                <main className="max-w-md mx-auto px-5 py-16 sm:py-24">
                    <div className="mb-8"><div className="w-12 h-12 flex items-center justify-center rounded-xl bg-blue-50 text-blue-600 mb-6"><KeyRound size={24} /></div>
                        <p className="text-sm text-blue-600 font-medium mb-3">{t('token_portal.kicker')}</p>
                        <h1 className="text-3xl font-semibold tracking-tight mb-3">{t('token_portal.login_title')}</h1>
                        <p className="text-sm leading-6 text-slate-500">{t('token_portal.login_description')}</p>
                    </div>
                    <form onSubmit={e => { e.preventDefault(); const token = input.trim(); if (token) void load(token); }} className="bg-white border border-slate-200 rounded-2xl p-6 space-y-4 shadow-sm">
                        <label htmlFor="portal-token" className="block text-sm font-medium">API Token</label>
                        <input id="portal-token" type="password" autoComplete="off" spellCheck={false} value={input} onChange={e => setInput(e.target.value)} maxLength={256}
                            placeholder="sk-…" disabled={busy} required className="w-full rounded-lg border border-slate-300 bg-white px-3 py-3 text-sm outline-none focus:border-blue-500 focus:ring-2 focus:ring-blue-100" />
                        {error && <p role="alert" className="text-sm text-red-600">{error}</p>}
                        <button type="submit" disabled={busy || !input.trim()} className="w-full flex justify-center items-center gap-2 rounded-lg bg-blue-600 text-white text-sm font-medium py-3 hover:bg-blue-700 disabled:opacity-50">
                            {busy ? <RefreshCw size={16} className="animate-spin" /> : <ArrowRight size={16} />}{t('token_portal.login')}
                        </button>
                        <p className="text-xs leading-5 text-slate-500 flex gap-2"><ShieldCheck size={16} className="shrink-0 mt-0.5" />{t('token_portal.memory_only')}</p>
                    </form>
                </main>
            ) : (
                <main className="max-w-6xl mx-auto px-5 sm:px-8 py-10 space-y-8">
                    <section className="flex flex-wrap items-start justify-between gap-4">
                        <div><p className="text-sm text-slate-500 mb-2">{t('token_portal.kicker')} · <span className="font-mono">{overview.token_hint}</span></p>
                            <h1 className="text-3xl font-semibold tracking-tight break-all">{overview.username}</h1>
                            <div className="flex flex-wrap items-center gap-3 mt-3 text-xs text-slate-500"><span className={`rounded-full px-2.5 py-1 font-medium ${overview.expired || exhausted ? 'bg-amber-100 text-amber-800' : 'bg-emerald-50 text-emerald-700'}`}>{status}</span>
                                <span>{t('token_portal.expires')}: {overview.expires_at ? date(overview.expires_at) : t('user_token.never')}</span></div>
                        </div>
                        <div className="text-right"><button onClick={() => void load(key.current, overview.logs.page)} disabled={busy} className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-4 py-2.5 text-sm disabled:opacity-50"><RefreshCw size={16} className={busy ? 'animate-spin' : ''} />{t('common.refresh')}</button>
                            {updatedAt && <p className="mt-2 text-xs text-slate-400">{t('token_portal.updated')}: {updatedAt.toLocaleTimeString(i18n.language)}</p>}</div>
                    </section>
                    {error && <p role="alert" className="text-sm text-red-600 rounded-lg bg-red-50 px-4 py-3">{error}</p>}
                    <div id="portal-panel-overview" hidden={menu !== 'overview'} className="space-y-8">
                    <section aria-label={t('token_portal.summary')} className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
                        {[
                            [t('token_portal.used'), million(overview.total_tokens_used)],
                            [t('token_portal.remaining'), overview.remaining_tokens === null ? unlimited : million(overview.remaining_tokens)],
                            [t('token_portal.requests'), overview.total_requests.toLocaleString()],
                            [t('user_token.max_concurrency'), overview.max_concurrency === 0 ? unlimited : String(overview.max_concurrency)],
                        ].map(([label, value]) => <div key={label} className="bg-white border border-slate-200 rounded-xl p-5"><p className="text-sm text-slate-500 mb-3">{label}</p><p className="text-xl font-semibold tracking-tight break-words">{value}</p></div>)}
                    </section>
                    <section className="bg-white border border-slate-200 rounded-xl px-5 py-5 space-y-3">
                        <div className="flex flex-wrap justify-between gap-2 text-sm"><span className="font-medium">{t('token_portal.quota')}</span><span>{overview.token_quota === 0 ? unlimited : million(overview.token_quota)}</span></div>
                        {overview.token_quota > 0 && <div role="progressbar" aria-label={t('token_portal.used')} aria-valuenow={Math.round(usedPercent)} aria-valuemin={0} aria-valuemax={100} className="h-2 rounded-full bg-slate-100 overflow-hidden"><div style={{ width: `${usedPercent}%` }} className={`h-full rounded-full ${exhausted ? 'bg-amber-500' : 'bg-blue-500'}`} /></div>}
                        <p className="text-xs text-slate-500">{t('user_token.quota_settlement_hint')}</p>
                    </section>
                    <TokenConnectionDetails token={key.current} />
                    </div>
                    <section id="portal-panel-models" hidden={menu !== 'models'} className="space-y-3">
                        <h2 className="text-lg font-semibold">{t('user_token.allowed_models')}</h2>
                        {overview.allowed_models.length === 0 && <p className="text-sm text-slate-500">{t('token_portal.all_models')}</p>}
                        {overview.available_models.length ? <div className="flex flex-wrap gap-2">{overview.available_models.map(model => <span key={model} className="font-mono text-xs break-all px-3 py-2 bg-white border border-slate-200 rounded-lg">{model}</span>)}</div> : <p className="text-sm text-slate-500">{t('token_portal.empty_catalog')}</p>}
                    </section>
                    <div id="portal-panel-statistics" hidden={menu !== 'statistics'} className="space-y-8">
                    <section className="space-y-4">
                        <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="text-lg font-semibold">{t('token_portal.by_model')}</h2><label className="sr-only" htmlFor="usage-model-filter">{t('token_portal.search')}</label><input id="usage-model-filter" type="search" value={search} onChange={e => setSearch(e.target.value)} placeholder={t('token_portal.search')} className="rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm w-full sm:w-64" /></div>
                        <div className="overflow-x-auto bg-white border border-slate-200 rounded-xl">
                            <table className="w-full text-sm text-left whitespace-nowrap">
                                <thead className="bg-slate-50 text-xs text-slate-500"><tr>{['model','requests','input','output','cached','total','last_used'].map(k => <th key={k} className="px-5 py-4 font-medium" scope="col">{t(`token_portal.${k}`)}</th>)}</tr></thead>
                                <tbody className="divide-y divide-slate-100">{rows.map(row => <tr key={row.model}>
                                    <th scope="row" className="px-5 py-4 font-mono text-xs font-medium">{row.model}</th>
                                    <td className="px-5 py-4">{row.requests.toLocaleString()}{row.failed_requests > 0 && <span className="block text-xs text-slate-400 mt-1">{t('token_portal.failures', { count: row.failed_requests })}</span>}</td>
                                    <td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.input_tokens)}</td><td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.output_tokens)}</td><td className="px-5 py-4 tabular-nums">{row.cached_tokens === null ? t('token_portal.not_recorded') : toQuotaMillions(row.cached_tokens)}{row.cached_tokens !== null && row.cache_recorded_requests < row.requests && <span className="block text-xs text-slate-400">{t('token_portal.partial_cache')}</span>}</td><td className="px-5 py-4 font-medium tabular-nums">{toQuotaMillions(row.input_tokens + row.output_tokens)}</td><td className="px-5 py-4 text-xs text-slate-500">{date(row.last_used_at)}</td>
                                </tr>)}</tbody>
                            </table>
                            {rows.length === 0 && <p className="text-center text-sm text-slate-500 py-12">{t(overview.models.length ? 'token_portal.no_matches' : 'token_portal.empty')}</p>}
                        </div>
                        <p className="text-xs text-slate-500 leading-5">{t('token_portal.statistics_note')}</p>
                    </section>
                    <TokenUsageDetails view="statistics" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} onPage={page => void load(key.current, page)} />
                    </div>
                    <div id="portal-panel-logs" hidden={menu !== 'logs'}>
                        <TokenUsageDetails view="logs" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} onPage={page => void load(key.current, page)} onFilter={range => void load(key.current, 1, range)} />
                    </div>
                </main>
            )}
        </div>
    );
}
