import { useCallback, useEffect, useRef, useState } from 'react';
import { Activity, ArrowRight, LogOut, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { toQuotaMillions } from '../components/user-token/tokenQuota';
import TokenConnectionDetails from '../components/user-token/TokenConnectionDetails';
import TokenUsageDetails, { LogTimeRange, UsageLogPage, UsagePeriod } from '../components/user-token/TokenUsageDetails';
import PortalAuthForms from '../components/user-token/PortalAuthForms';
import { PortalConfig, portalCall } from '../components/user-token/portalApi';
import { formatYuan, Price } from '../components/user-token/money';
import PortalModelCatalog from '../components/user-token/PortalModelCatalog';
import PortalConnectionGuide from '../components/user-token/PortalConnectionGuide';
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
    expired: boolean;
    expires_at: number | null;
    max_concurrency: number;
    total_tokens_used: number;
    total_requests: number;
    /** Prepaid balance in micro-yuan. */
    balance_micros: number | null;
    free_models: string[];
    available_models: string[];
    model_prices: (Price & { model: string })[];
    topup_orders: TopupOrder[];
    logs: UsageLogPage;
    daily: UsagePeriod[];
    weekly: UsagePeriod[];
    models: ModelUsage[];
}

const portalMenus = ['overview', 'balance', 'models', 'guide', 'statistics', 'logs'] as const;
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
    const overviewMenu = useRef<HTMLButtonElement>(null);
    const pending = useRef<AbortController | null>(null);
    // Read once on arrival: the payment gateway returns users here with the order number.
    const [returnedOrder, setReturnedOrder] = useState(takeReturnedOrder);
    const [paymentRefresh, setPaymentRefresh] = useState(0);
    useEffect(() => {
        const restore = (event: PageTransitionEvent) => {
            if (!event.persisted) return;
            const order = takeReturnedOrder();
            setReturnedOrder(order);
            if (order) setMenu('balance');
            setPaymentRefresh(value => value + 1);
        };
        window.addEventListener('pageshow', restore);
        return () => window.removeEventListener('pageshow', restore);
    }, []);
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
            if (firstLoad && returnedOrder) setMenu('balance');
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
    const menus = portalMenus;

    const unlimited = t('user_token.unlimited');
    const million = (n: number) => `${toQuotaMillions(n)} M tokens`;
    const exhausted = overview !== null && (overview.balance_micros ?? 0) <= 0;
    const status = overview?.expired ? t('token_portal.expired') : exhausted ? t('billing.insufficient') : t('token_portal.active');
    const rows = overview?.models.filter(m => m.model.toLowerCase().includes(search.toLowerCase())) ?? [];
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
                    {menus.map(item => <button key={item} type="button" ref={item === 'overview' ? overviewMenu : undefined} aria-current={menu === item ? 'page' : undefined}
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
                    <div id="portal-panel-overview" hidden={menu !== 'overview'} className="space-y-8">
                    <section aria-label={t('token_portal.summary')} className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-4 gap-4">
                        <button type="button" onClick={() => setMenu('balance')} aria-controls="portal-panel-balance" className="text-left bg-white border border-slate-200 rounded-xl p-5 hover:border-blue-300 hover:bg-blue-50/30 focus-visible:ring-2 focus-visible:ring-blue-500">
                            <span className="flex items-center justify-between text-sm text-slate-500 mb-3">{t('billing.balance')}<ArrowRight size={16} aria-hidden="true" /></span>
                            <span className="block text-xl font-semibold tracking-tight break-words">{formatYuan(overview.balance_micros ?? 0)}</span>
                        </button>
                        {[
                            [t('token_portal.used'), million(overview.total_tokens_used)],
                            [t('token_portal.requests'), overview.total_requests.toLocaleString()],
                            [t('user_token.max_concurrency'), overview.max_concurrency === 0 ? unlimited : String(overview.max_concurrency)],
                        ].map(([label, value]) => <div key={label} className="bg-white border border-slate-200 rounded-xl p-5"><p className="text-sm text-slate-500 mb-3">{label}</p><p className="text-xl font-semibold tracking-tight break-words">{value}</p></div>)}
                    </section>
                    {(currentKey || isAccount) && <TokenConnectionDetails token={currentKey} account={isAccount ? { onRegenerate: regenerateKey } : undefined} />}
                    </div>
                    <div id="portal-panel-balance" hidden={menu !== 'balance'}>
                        <PortalTopup config={config?.topup} balanceMicros={overview.balance_micros ?? 0} expired={overview.expired}
                            active={menu === 'balance'} refreshKey={(updatedAt?.getTime() ?? 0) + paymentRefresh} request={portalRequest}
                            returnedOrder={returnedOrder} onReturnHandled={() => setReturnedOrder(null)} onSettled={() => reload()} />
                    </div>
                    <section id="portal-panel-models" hidden={menu !== 'models'} className="space-y-3">
                        <h2 className="text-lg font-semibold">{t('token_portal.menu_models')}</h2>
                        <p className="text-sm text-slate-500">{t('billing.portal_prices_hint')}</p>
                        <PortalModelCatalog models={overview.available_models} prices={overview.model_prices} freeModels={overview.free_models} />
                    </section>
                    <div id="portal-panel-guide" hidden={menu !== 'guide'}>
                        <PortalConnectionGuide token={currentKey} models={overview.available_models} onOpenOverview={() => { setMenu('overview'); overviewMenu.current?.focus(); }} />
                    </div>
                    <div id="portal-panel-statistics" hidden={menu !== 'statistics'} className="space-y-8">
                    <section className="space-y-4">
                        <div className="flex flex-wrap items-center justify-between gap-3"><h2 className="text-lg font-semibold">{t('token_portal.by_model')}</h2><label className="sr-only" htmlFor="usage-model-filter">{t('token_portal.search')}</label><input id="usage-model-filter" type="search" value={search} onChange={e => setSearch(e.target.value)} placeholder={t('token_portal.search')} className="rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm w-full sm:w-64" /></div>
                        <div className="overflow-x-auto bg-white border border-slate-200 rounded-xl">
                            <table className="w-full text-sm text-left whitespace-nowrap">
                                <thead className="bg-slate-50 text-xs text-slate-500"><tr>{['model','requests','input','output','cached','total','cost','last_used'].map(k => <th key={k} className="px-5 py-4 font-medium" scope="col">{t(k === 'cost' ? 'billing.cost' : `token_portal.${k}`)}</th>)}</tr></thead>
                                <tbody className="divide-y divide-slate-100">{rows.map(row => <tr key={row.model}>
                                    <th scope="row" className="px-5 py-4 font-mono text-xs font-medium">{row.model}</th>
                                    <td className="px-5 py-4">{row.requests.toLocaleString()}{row.failed_requests > 0 && <span className="block text-xs text-slate-400 mt-1">{t('token_portal.failures', { count: row.failed_requests })}</span>}</td>
                                    <td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.input_tokens)}</td><td className="px-5 py-4 tabular-nums">{toQuotaMillions(row.output_tokens)}</td><td className="px-5 py-4 tabular-nums">{row.cached_tokens === null ? t('token_portal.not_recorded') : toQuotaMillions(row.cached_tokens)}{row.cached_tokens !== null && row.cache_recorded_requests < row.requests && <span className="block text-xs text-slate-400">{t('token_portal.partial_cache')}</span>}</td><td className="px-5 py-4 font-medium tabular-nums">{toQuotaMillions(row.input_tokens + row.output_tokens)}</td><td className="px-5 py-4 tabular-nums">{row.cost_micros === null ? '—' : formatYuan(row.cost_micros)}</td><td className="px-5 py-4 text-xs text-slate-500">{date(row.last_used_at)}</td>
                                </tr>)}</tbody>
                            </table>
                            {rows.length === 0 && <p className="text-center text-sm text-slate-500 py-12">{t(overview.models.length ? 'token_portal.no_matches' : 'token_portal.empty')}</p>}
                        </div>
                        <p className="text-xs text-slate-500 leading-5">{t('token_portal.statistics_note')}</p>
                    </section>
                    <TokenUsageDetails view="statistics" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} onPage={page => reload(page)} />
                    </div>
                    <div id="portal-panel-logs" hidden={menu !== 'logs'}>
                        <TokenUsageDetails view="logs" daily={overview.daily} weekly={overview.weekly} logs={overview.logs} busy={busy} showCost onPage={page => reload(page)} onFilter={range => reload(1, range)} />
                    </div>
                </main>
            )}
        </div>
    );
}
