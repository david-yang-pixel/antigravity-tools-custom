import { AlertTriangle, Coins, RefreshCw } from 'lucide-react';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { WorkBuddyAccount } from '../accounts/WorkBuddyAccountRow';
import { request } from '../../utils/request';

const REFRESH_MS = 30_000;
const REQUEST_TIMEOUT_MS = 15_000;
// Managed accounts normally sync every 15 minutes; allow one missed cycle.
const STALE_SECONDS = 30 * 60;
type CreditAccount = Pick<WorkBuddyAccount, 'realm' | 'management'>;

export function summarizeWorkBuddyCredits(accounts: readonly CreditAccount[], realm: WorkBuddyAccount['realm'], now: number) {
    const realmAccounts = accounts.filter(account => account.realm === realm);
    let total = 0;
    let known = 0;
    let stale = 0;
    let failed = 0;
    let oldestUpdate: number | null = null;
    for (const { management } of realmAccounts) {
        if (management.credits_error) failed++;
        const credits = management.credits;
        if (!credits || !Number.isFinite(credits.available)) continue;
        total += credits.available;
        known++;
        if (management.credits_error || credits.updated_at <= 0 || now / 1000 - credits.updated_at > STALE_SECONDS) stale++;
        if (credits.updated_at > 0) oldestUpdate = Math.min(oldestUpdate ?? credits.updated_at, credits.updated_at);
    }
    return { count: realmAccounts.length, known, total: known > 0 ? total : null, stale, failed, oldestUpdate };
}

/** Pure display, also usable with local fixtures without any upstream requests. */
export default function WorkBuddyCreditSummary({ accounts, loading, error, now, onRefresh }: {
    accounts: readonly CreditAccount[] | null;
    loading: boolean;
    error: boolean;
    now: number;
    onRefresh: () => void;
}) {
    const { t, i18n } = useTranslation();
    return <section aria-labelledby="workbuddy-credits-title" aria-busy={loading} className="space-y-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
                <h2 id="workbuddy-credits-title" className="text-sm font-semibold text-gray-900 dark:text-base-content">{t('dashboard.workbuddy_credits.title')}</h2>
                <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">{t('dashboard.workbuddy_credits.hint')}</p>
            </div>
            <button type="button" onClick={onRefresh} disabled={loading}
                className="inline-flex items-center gap-1.5 rounded-lg border border-gray-200 bg-white px-3 py-1.5 text-xs font-medium text-gray-600 hover:bg-gray-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 disabled:cursor-not-allowed disabled:opacity-50 dark:border-base-300 dark:bg-base-100 dark:text-gray-300 dark:hover:bg-base-200">
                <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} aria-hidden="true" />
                {t(`dashboard.workbuddy_credits.${loading ? 'loading' : error ? 'retry' : 'refresh'}`)}
            </button>
        </div>
        {error && <p role="alert" className="flex items-start gap-1.5 rounded-lg bg-amber-50 px-3 py-2 text-xs text-amber-800 dark:bg-amber-900/20 dark:text-amber-300"><AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden="true" />{t(`dashboard.workbuddy_credits.${accounts ? 'refresh_error' : 'load_error'}`)}</p>}
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            {(['cn', 'global'] as const).map(realm => {
                const summary = summarizeWorkBuddyCredits(accounts ?? [], realm, now);
                const incomplete = summary.known < summary.count;
                return <div key={realm} className="min-w-0 rounded-xl border border-gray-100 bg-white p-4 shadow-sm dark:border-base-200 dark:bg-base-100">
                    <div className="mb-3 flex items-center gap-2">
                        <div className="rounded-md bg-blue-50 p-1.5 dark:bg-blue-900/20"><Coins className="h-4 w-4 text-blue-500 dark:text-blue-400" aria-hidden="true" /></div>
                        <h3 className="text-xs font-bold text-gray-800 dark:text-gray-200">{t(`dashboard.workbuddy_credits.${realm}`)}</h3>
                    </div>
                    <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
                        <span className="break-all font-mono text-3xl font-extrabold tabular-nums text-gray-900 dark:text-base-content">{summary.total === null ? '—' : summary.total.toLocaleString(i18n.language, { maximumFractionDigits: 2 })}</span>
                        <span className="text-xs text-gray-500 dark:text-gray-400">{t(`dashboard.workbuddy_credits.${incomplete ? 'subtotal' : 'total'}`)}</span>
                    </div>
                    <p className="mt-2 text-xs text-gray-500 dark:text-gray-400">{accounts === null
                        ? t(`dashboard.workbuddy_credits.${loading ? 'loading' : 'unavailable'}`)
                        : summary.count === 0 ? t('dashboard.workbuddy_credits.empty')
                            : t('dashboard.workbuddy_credits.accounts', { count: summary.count, known: summary.known })}</p>
                    {incomplete && <p className="mt-2 text-xs text-amber-700 dark:text-amber-400">{t('dashboard.workbuddy_credits.incomplete', { count: summary.count - summary.known })}</p>}
                    {summary.failed > 0 && <p className="mt-2 text-xs text-amber-700 dark:text-amber-400">{t('dashboard.workbuddy_credits.failed', { count: summary.failed })}</p>}
                    {summary.stale > 0 && <p className="mt-2 text-xs text-amber-700 dark:text-amber-400">{t('dashboard.workbuddy_credits.stale', { count: summary.stale })}</p>}
                    {summary.oldestUpdate !== null && <p className="mt-2 text-[10px] text-gray-400 dark:text-gray-500">{t('dashboard.workbuddy_credits.oldest_update', { time: new Date(summary.oldestUpdate * 1000).toLocaleString(i18n.language) })}</p>}
                </div>;
            })}
        </div>
    </section>;
}

/** Refresh saved account observations only; never starts account sync, check-in or inference. */
export function WorkBuddyCreditOverview() {
    const [accounts, setAccounts] = useState<WorkBuddyAccount[] | null>(null);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState(false);
    const [now, setNow] = useState(Date.now);
    const [revision, setRevision] = useState(0);
    useEffect(() => {
        let active = true;
        let inFlight = false;
        let timeout: ReturnType<typeof setTimeout> | undefined;
        let currentController: AbortController | null = null;
        const load = async () => {
            if (!active || inFlight || document.visibilityState === 'hidden') return;
            inFlight = true;
            setLoading(true); setNow(Date.now());
            const controller = new AbortController();
            currentController = controller;
            timeout = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
            try {
                // Web requests are cancelled on timeout. Tauri invoke is not cancellable,
                // so keep the request locked until it actually settles there.
                const values = await request<WorkBuddyAccount[]>('list_workbuddy_accounts', undefined, { signal: controller.signal });
                if (!Array.isArray(values)) throw new Error('invalid accounts');
                if (active) { setAccounts(values); setError(false); }
            } catch {
                if (active) setError(true);
            } finally {
                clearTimeout(timeout);
                currentController = null;
                inFlight = false;
                if (active) setLoading(false);
            }
        };
        const refreshVisible = () => { if (document.visibilityState !== 'hidden') void load(); };
        void load();
        const interval = setInterval(refreshVisible, REFRESH_MS);
        document.addEventListener('visibilitychange', refreshVisible);
        return () => {
            active = false;
            currentController?.abort();
            clearTimeout(timeout); clearInterval(interval);
            document.removeEventListener('visibilitychange', refreshVisible);
        };
    }, [revision]);
    return <WorkBuddyCreditSummary accounts={accounts} loading={loading} error={error} now={now} onRefresh={() => setRevision(value => value + 1)} />;
}
