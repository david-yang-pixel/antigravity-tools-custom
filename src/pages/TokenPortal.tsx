import { useCallback, useEffect, useRef, useState } from 'react';
import { Activity, AlertTriangle, LogOut, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { toQuotaMillions } from '../components/user-token/tokenQuota';
import TokenConnectionDetails from '../components/user-token/TokenConnectionDetails';
import TokenUsageDetails, { LogTimeRange, UsageLogPage, UsagePeriod } from '../components/user-token/TokenUsageDetails';
import PortalAuthForms from '../components/user-token/PortalAuthForms';
import PortalApiKeyPanel from '../components/user-token/PortalApiKeyPanel';
import { PortalConfig, portalCall } from '../components/user-token/portalApi';
import { formatYuan, Price } from '../components/user-token/money';
import ModelPriceCard from '../components/user-token/ModelPriceCard';
import PortalTopup, { takeReturnedOrder, TopupOrder } from '../components/user-token/PortalTopup';

interface ModelUsage {
    model: string;
    requests: number;
    failed_requests: number;
    input_tokens: number;
    output_tokens: number;
    last_used_at: number;
    cached_tokens: number | null;
    cache_recorded_requests: number;
    cost_micros: number | null;
}
interface Overview {
    username: string;
    token_hint: string;
    account_email: string | null;
    pending_activation: boolean;
    expired: boolean;
    expires_at: number | null;
    max_concurrency: number;
    token_quota: number;
    total_tokens_used: number;
    remaining_tokens: number | null;
    total_requests: number;
    /** Prepaid balance in micro-yuan; null when the key is not billed. */
    balance_micros: number | null;
    allowed_models: string[];
    available_models: string[];
    model_prices: (Price & { model: string })[];
    topup_orders: TopupOrder[];
    logs: UsageLogPage;
    daily: UsagePeriod[];
    weekly: UsagePeriod[];
    models: ModelUsage[];
}

const portalMenus = ['overview', 'apikey', 'models', 'statistics', 'logs'] as const;
type PortalMenu = typeof portalMenus[number];
/** Key sign-in sends the key as Bearer; account sign-in relies on the session cookie. */
type Auth = { kind: 'key'; token: string } | { kind: 'account' };

export default function TokenPortal() {
    const { t, i18n } = useTranslation();
    const [menu, setMenu] = useState<PortalMenu>('overview');
    const [overview, setOverview] = useState<Overview | null>(null);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [keyError, setKeyError] = useState('');
    const [notice, setNotice] = useState('');
    const [config, setConfig] = useState<PortalConfig | null>(null);
    const [accountKey, setAccountKey] = useState('');
    const [search, setSearch] = useState('');
    const [logRange, setLogRange] = useState<LogTimeRange>({});
    const [updatedAt, setUpdatedAt] = useState<Date | null>(null);
    const auth = useRef<Auth | null>(null);
    const pending = useRef<AbortController | null>(null);
    // Read once on arrival: the payment gateway returns users here with the order number.
    const [returnedOrder, setReturnedOrder] = useState(takeReturnedOrder);
    const portalRequest = useCallback((path: string, init?: RequestInit) => {
        const current = auth.current;
        return fetch(`/api/token-portal/${path}`, {
            ...init,
            headers: { ...init?.headers, ...(current?.kind === 'key' ? { Authorization: `Bearer ${current.token}` } : {}) },
            cache: 'no-store', credentials: current?.kind === 'key' ? 'omit' : 'same-origin', referrerPolicy: 'no-referrer',
        });
    }, []);
    useEffect(() => () => pending.current?.abort(), []);
    // Restore an existing account session and learn which account features are on.
    useEffect(() => {
        void portalCall<PortalConfig>('config').then(result => { if (result.ok) setConfig(result.data); }).catch(() => undefined);
        // Back from the payment page: key sign-in lived only in memory, so say why to sign in
        // again. A restored account session clears this notice when it loads.
        if (returnedOrder) setNotice(t('topup.return_login'));
        void load({ kind: 'account' }, 1, {}, true);
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, []);

    function reset() {
        pending.current?.abort();
        pending.current = null;
        auth.current = null;
        setMenu('overview');
        setLogRange({});
        setOverview(null); setAccountKey(''); setSearch(''); setUpdatedAt(null); setError(''); setBusy(false);
    }

    async function logout() {
        const wasAccount = auth.current?.kind === 'account';
        reset(); setKeyError(''); setNotice('');
        if (wasAccount) await portalCall('auth/logout', {}).catch(() => undefined);
    }

    async function loadAccountKey() {
        const result = await portalCall<{ token?: string }>('key');
        if (result.ok && result.data.token) setAccountKey(result.data.token);
    }

    async function regenerateKey(): Promise<boolean> {
        const result = await portalCall<{ token?: string }>('key/regenerate', {});
        if (!result.ok || !result.data.token) return false;
        setAccountKey(result.data.token);
        if (overview) setOverview({ ...overview, token_hint: `••••${result.data.token.slice(-4)}` });
        return true;
    }

    async function load(next: Auth, page = 1, range: LogTimeRange = logRange, silent = false) {
        pending.current?.abort();
        const controller = new AbortController();
        pending.current = controller;
        setBusy(true); setError('');
        try {
            const query = new URLSearchParams({ page: String(page) });
            if (range.start_time !== undefined) query.set('start_time', String(range.start_time));
            if (range.end_time !== undefined) query.set('end_time', String(range.end_time));
            const response = await fetch(`/api/token-portal/me?${query}`, {
                headers: next.kind === 'key' ? { Authorization: `Bearer ${next.token}` } : undefined,
                cache: 'no-store', credentials: next.kind === 'key' ? 'omit' : 'same-origin', referrerPolicy: 'no-referrer', signal: controller.signal,
            });
            if (response.status === 401) {
                if (controller.signal.aborted) return;
                const wasSignedIn = auth.current !== null;
                reset();
                if (next.kind === 'key') setKeyError(t('token_portal.invalid_token'));
                else if (wasSignedIn && !silent) setNotice(t('token_portal.session_expired'));
                return;
            }
            if (!response.ok) throw new Error('unavailable');
            const result: Overview = await response.json();
            if (controller.signal.aborted) return;
            const firstLoad = auth.current === null;
            auth.current = next;
            setLogRange(range);
            setKeyError(''); setNotice('');
            setOverview(result); setUpdatedAt(new Date());
            if (next.kind === 'account' && firstLoad) void loadAccountKey();
        } catch {
            if (!controller.signal.aborted && !silent) setError(t('token_portal.load_error'));
        } finally {
            if (pending.current === controller) { setBusy(false); pending.current = null; }
        }
    }

    const reload = (page = overview?.logs.page ?? 1, range: LogTimeRange = logRange) => { if (auth.current) void load(auth.current, page, range); };
    const isAccount = auth.current?.kind === 'account';
    const currentKey = auth.current?.kind === 'key' ? auth.current.token : accountKey;
    const menus = portalMenus.filter(item => item !== 'apikey' || isAccount);

    const unlimited = t('user_token.unlimited');
    const million = (n: number) => `${toQuotaMillions(n)} M tokens`;
    const billed = overview !== null && overview.balance_micros !== null;
    const exhausted = overview?.remaining_tokens === 0 || (billed && (overview?.balance_micros ?? 0) <= 0);
    const quotaUnlimited = overview?.remaining_tokens === null;
    const status = overview?.pending_activation ? t('token_portal.pending_activation') : overview?.expired ? t('token_portal.expired') : exhausted ? t(billed ? 'billing.insufficient' : 'user_token.quota_exhausted') : t('token_portal.active');
    const priceOf = (model: string) => overview?.model_prices.find(price => price.model === model);
    // The price list is public to every key once any price is set; only billed keys are charged by it.
    const showPrices = billed || (overview?.model_prices.length ?? 0) > 0;
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
                        {overview?.account_email && <span className="hidden md:inline text-slate-500 max-w-56 truncate" title={overview.account_email}>{overview.account_email}</span>}
                        {overview && <button onClick={() => void logout()} className="flex items-center gap-2 rounded-lg px-3 py-2 hover:bg-slate-100"><LogOut size={16} />{t('token_portal.logout')}</button>}
                    </div>
                </div>
                {overview && <nav aria-label={t('token_portal.navigation')} className="max-w-6xl mx-auto px-5 sm:px-8 flex gap-1 sm:gap-6 overflow-x-auto">
                    {menus.map(item => <button key={item} type="button" aria-current={menu === item ? 'page' : undefined}
                        aria-controls={`portal-panel-${item}`} onClick={() => setMenu(item)}
                        className={`shrink-0 border-b-2 px-3 sm:px-4 py-3 text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500 ${menu === item ? 'border-blue-600 text-blue-600' : 'border-transparent text-slate-500 hover:text-slate-900 hover:border-slate-300'}`}>
                        {t(`token_portal.menu_${item}`)}
                    </button>)}
                </nav>}
            </header>
            {!overview ? (
                <PortalAuthForms config={config} busy={busy} keyError={keyError} accountError={error} notice={notice}
                    onKeyLogin={token => void load({ kind: 'key', token }, 1, {})}
                    onAccountReady={() => void load({ kind: 'account' }, 1, {})} />
            ) : (
                <main className="max-w-6xl mx-auto px-5 sm:px-8 py-10 space-y-8">
                    <section className="flex flex-wrap items-start justify-between gap-4">
                        <div><p className="text-sm text-slate-500 mb-2">{t('token_portal.kicker')} · <span className="font-mono">{overview.token_hint}</span></p>
                            <h1 className="text-3xl font-semibold tracking-tight break-all">{overview.username}</h1>
                            <div className="flex flex-wrap items-center gap-3 mt-3 text-xs text-slate-500"><span className={`rounded-full px-2.5 py-1 font-medium ${overview.expired || exhausted ? 'bg-amber-100 text-amber-800' : 'bg-emerald-50 text-emerald-700'}`}>{status}</span>
                                <span>{t('token_portal.expires')}: {overview.expires_at ? date(overview.expires_at) : t('user_token.never')}</span></div>
                        </div>
                        <div className="text-right"><button onClick={() => reload()} disabled={busy} className="inline-flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-4 py-2.5 text-sm disabled:opacity-50"><RefreshCw size={16} className={busy ? 'animate-spin' : ''} />{t('common.refresh')}</button>
                            {updatedAt && <p className="mt-2 text-xs text-slate-400">{t('token_portal.updated')}: {updatedAt.toLocaleTimeString(i18n.language)}</p>}</div>
                    </section>
                    {error && <p role="alert" className="text-sm text-red-600 rounded-lg bg-red-50 px-4 py-3">{error}</p>}
                    {overview.pending_activation && <p className="flex gap-2 rounded-lg bg-amber-50 border border-amber-200 px-4 py-3 text-sm text-amber-800"><AlertTriangle size={16} className="shrink-0 mt-0.5" />{billed && config?.topup?.enabled
                        ? t('topup.pending_banner', { min: formatYuan(config.topup.min_amount_cents * 10_000) })
                        : t(billed ? 'billing.pending_banner' : 'token_portal.pending_banner')}</p>}
                    <div id="portal-panel-overview" hidden={menu !== 'overview'} className="space-y-8">
                    <section aria-label={t('token_portal.summary')} className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
                        {[
                            ...(billed ? [[t('billing.balance'), formatYuan(overview.balance_micros ?? 0)]] : []),
                            [t('token_portal.used'), million(overview.total_tokens_used)],
                            ...(billed && quotaUnlimited ? [] : [[t('token_portal.remaining'), quotaUnlimited ? unlimited : million(overview.remaining_tokens ?? 0)]]),
                            [t('token_portal.requests'), overview.total_requests.toLocaleString()],
                            [t('user_token.max_concurrency'), overview.max_concurrency === 0 ? unlimited : String(overview.max_concurrency)],
                        ].map(([label, value]) => <div key={label} className="bg-white border border-slate-200 rounded-xl p-5"><p className="text-sm text-slate-500 mb-3">{label}</p><p className="text-xl font-semibold tracking-tight break-words">{value}</p></div>)}
                    </section>
                    {billed && <section className="bg-white border border-slate-200 rounded-xl px-5 py-5 space-y-2">
                        <div className="flex flex-wrap justify-between gap-2 text-sm"><span className="font-medium">{t('billing.balance')}</span><span className={`font-semibold ${exhausted ? 'text-amber-700' : ''}`}>{formatYuan(overview.balance_micros ?? 0)}</span></div>
                        <p className="text-xs text-slate-500">{t('billing.portal_balance_hint')}</p>
                        {/* An expired key's balance cannot be spent, so it is not offered a top-up. */}
                        <PortalTopup config={overview.expired ? undefined : config?.topup} orders={overview.topup_orders ?? []} request={portalRequest}
                            returnedOrder={returnedOrder} onReturnHandled={() => setReturnedOrder(null)} onSettled={() => reload()} />
                    </section>}
                    {!(billed && quotaUnlimited) && <section className="bg-white border border-slate-200 rounded-xl px-5 py-5 space-y-3">
                        <div className="flex flex-wrap justify-between gap-2 text-sm"><span className="font-medium">{t('token_portal.quota')}</span><span>{quotaUnlimited ? unlimited : million(overview.token_quota)}</span></div>
                        {overview.token_quota > 0 && <div role="progressbar" aria-label={t('token_portal.used')} aria-valuenow={Math.round(usedPercent)} aria-valuemin={0} aria-valuemax={100} className="h-2 rounded-full bg-slate-100 overflow-hidden"><div style={{ width: `${usedPercent}%` }} className={`h-full rounded-full ${exhausted ? 'bg-amber-500' : 'bg-blue-500'}`} /></div>}
                        <p className="text-xs text-slate-500">{t('user_token.quota_settlement_hint')}</p>
                    </section>}
                    {currentKey && <TokenConnectionDetails token={currentKey} />}
                    </div>
                    {isAccount && <div id="portal-panel-apikey" hidden={menu !== 'apikey'}>
                        <PortalApiKeyPanel token={accountKey} pending={overview.pending_activation} onRegenerate={regenerateKey} />
                    </div>}
                    <section id="portal-panel-models" hidden={menu !== 'models'} className="space-y-3">
                        <h2 className="text-lg font-semibold">{t('user_token.allowed_models')}</h2>
                        {overview.allowed_models.length === 0 && <p className="text-sm text-slate-500">{t('token_portal.all_models')}</p>}
                        {showPrices && <p className="text-sm text-slate-500">{t(billed ? 'billing.portal_prices_hint' : 'billing.portal_prices_hint_unbilled')}</p>}
                        {overview.available_models.length ? <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4 pt-1">
                            {overview.available_models.map(model => <ModelPriceCard key={model} model={model} price={priceOf(model)} showPrice={showPrices} billed={billed} />)}
                        </div> : <p className="text-sm text-slate-500">{t('token_portal.empty_catalog')}</p>}
                    </section>
                    <div id="portal-panel-statistics" hidden={menu !== 'statistics'} className="space-y-8">
                    <section className="space-y-4">
                        <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="text-lg font-semibold">{t('token_portal.by_model')}</h2><label className="sr-only" htmlFor="usage-model-filter">{t('token_portal.search')}</label><input id="usage-model-filter" type="search" value={search} onChange={e => setSearch(e.target.value)} placeholder={t('token_portal.search')} className="rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm w-full sm:w-64" /></div>
                        <div className="overflow-x-auto bg-white border border-slate-200 rounded-xl">
                            <table className="w-full text-sm text-left whitespace-nowrap">
                                <thead className="bg-slate-50 text-xs text-slate-500"><tr>{['model','requests','input','output','cached','total',...(billed ? ['cost'] : []),'last_used'].map(k => <th key={k} className="px-5 py-4 font-medium" scope="col">{t(k === 'cost' ? 'billing.cost' : `token_portal.${k}`)}</th>)}</tr></thead>
                                <tbody className="divide-y divide-slate-100">{rows.map(row => <tr key={row.model}>
                                    <th scope="row" className="px-5 py-4 font-mono text-xs font-medium">{row.model}</th>
                                    <td className="px-5 py-4">{row.requests.toLocaleString()}{row.failed_requests > 0 && <span className="block text-xs text-slate-400 mt-1">{t('token_portal.failures', { count: row.failed_requests })}</span>}</td>
                                    <td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.input_tokens)}</td><td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.output_tokens)}</td><td className="px-5 py-4 tabular-nums">{row.cached_tokens === null ? t('token_portal.not_recorded') : toQuotaMillions(row.cached_tokens)}{row.cached_tokens !== null && row.cache_recorded_requests < row.requests && <span className="block text-xs text-slate-400">{t('token_portal.partial_cache')}</span>}</td><td className="px-5 py-4 font-medium tabular-nums">{toQuotaMillions(row.input_tokens + row.output_tokens)}</td>{billed && <td className="px-5 py-4 tabular-nums">{row.cost_micros === null ? '—' : formatYuan(row.cost_micros)}</td>}<td className="px-5 py-4 text-xs text-slate-500">{date(row.last_used_at)}</td>
                                </tr>)}</tbody>
                            </table>
                            {rows.length === 0 && <p className="text-center text-sm text-slate-500 py-12">{t(overview.models.length ? 'token_portal.no_matches' : 'token_portal.empty')}</p>}
                        </div>
                        <p className="text-xs text-slate-500 leading-5">{t('token_portal.statistics_note')}</p>
                    </section>
                    <TokenUsageDetails view="statistics" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} onPage={page => reload(page)} />
                    </div>
                    <div id="portal-panel-logs" hidden={menu !== 'logs'}>
                        <TokenUsageDetails view="logs" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} showCost={billed} onPage={page => reload(page)} onFilter={range => reload(1, range)} />
                    </div>
                </main>
            )}
        </div>
    );
}
