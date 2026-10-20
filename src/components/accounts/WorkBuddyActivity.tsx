import { Gift, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { AccountChanges, WorkBuddyAccount } from './WorkBuddyAccountRow';

type ActivityKind = 'task' | 'streak' | 'travel' | 'lottery';
export interface ActivityItem {
    id: string; kind: ActivityKind; name: string;
    status: 'available' | 'pending' | 'claimed' | 'unsupported' | 'failed' | 'unknown';
    credits: number | null; detail: string | null;
}
export interface ActivityRecord {
    at: number; kind: ActivityKind; id: string; name: string;
    status: 'claimed' | 'skipped' | 'failed' | 'unknown';
    credits: number | null; detail: string | null;
}
export interface ActivityState {
    last_attempt_at: number; checked_at: number; next_run_at: number;
    status: 'idle' | 'running' | 'success' | 'partial' | 'failed';
    error: string | null; items: ActivityItem[]; history: ActivityRecord[];
}

const button = 'inline-flex items-center justify-center gap-1.5 rounded-md border border-gray-200 bg-white px-3 py-1.5 text-xs hover:bg-gray-50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 disabled:cursor-not-allowed disabled:opacity-40 dark:border-gray-700 dark:bg-gray-900 dark:hover:bg-gray-800';
const statusColor = (status: string) => status === 'claimed' || status === 'success'
    ? 'text-green-700 dark:text-green-400'
    : ['failed', 'partial', 'unknown'].includes(status) ? 'text-amber-700 dark:text-amber-400'
        : status === 'available' || status === 'running' ? 'text-blue-600 dark:text-blue-400' : 'text-gray-500 dark:text-gray-400';

export default function WorkBuddyActivity({ account, now, busy, onChange }: {
    account: WorkBuddyAccount; now: number; busy: boolean; onChange: (changes: AccountChanges) => void;
}) {
    const { t } = useTranslation();
    const m = account.management;
    const activity = m.activity;
    const running = activity?.status === 'running';
    const actionDisabled = busy || running || !account.enabled;
    // Allow stopping automatic collection while a background run is in progress.
    const autoDisabled = busy || !account.enabled || !m.managed;
    const autoEnabled = !!m.auto_rewards || !!m.auto_lottery;
    const date = (at?: number) => at && at > 0 ? new Date(at * 1000).toLocaleString() : t('workbuddy.activity.time_unknown');
    const credits = (amount: number | null) => amount === null || amount === undefined
        ? t('workbuddy.activity.amount_unknown') : t('workbuddy.activity.credits', { amount: amount.toLocaleString(undefined, { maximumFractionDigits: 2 }) });
    const items = activity?.items ?? [];
    const history = [...(activity?.history ?? [])].sort((a, b) => b.at - a.at).slice(0, 50);
    const titleId = `workbuddy-activity-${account.id}`;

    return <section aria-labelledby={titleId} className="mt-4 border-t border-gray-200 pt-3 text-xs dark:border-gray-700">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <h3 id={titleId} className="inline-flex items-center gap-1.5 font-medium"><Gift size={15} />{t('workbuddy.activity.title')}</h3>
            {account.realm === 'cn' && <div className="flex flex-wrap gap-2">
                <button className={button} disabled={actionDisabled} onClick={() => onChange({ activity: 'query' })}><RefreshCw size={13} className={running ? 'animate-spin motion-reduce:animate-none' : undefined} />{t('workbuddy.activity.query')}</button>
                <button className={`${button} text-blue-600 dark:text-blue-400`} disabled={actionDisabled} onClick={() => onChange({ activity: 'claim' })}>{t('workbuddy.activity.claim')}</button>
            </div>}
        </div>
        {account.realm === 'global' ? <p className="mt-2 text-gray-500 dark:text-gray-400">{t('workbuddy.activity.unsupported')}</p> : <>
            <div className="mt-3 flex flex-wrap items-center gap-x-5 gap-y-2">
                <label className="inline-flex items-center gap-1.5"><input type="checkbox" className="accent-blue-600" checked={!!m.auto_rewards} disabled={autoDisabled} onChange={e => onChange({ auto_rewards: e.target.checked })} />{t('workbuddy.activity.auto_rewards')}</label>
                <label className="inline-flex items-center gap-1.5"><input type="checkbox" className="accent-blue-600" checked={!!m.auto_lottery} disabled={autoDisabled} onChange={e => onChange({ auto_lottery: e.target.checked })} />{t('workbuddy.activity.auto_lottery')}</label>
                <span role="status" className={statusColor(activity?.status || 'idle')}>{t(`workbuddy.activity.status_${activity?.status || 'idle'}`)}</span>
            </div>
            <p className="mt-2 leading-relaxed text-gray-500 dark:text-gray-400">{t('workbuddy.activity.hint')}</p>
            {(!account.enabled || !m.managed) && <p className="mt-1 text-gray-500 dark:text-gray-400">{t('workbuddy.activity.paused_hint')}</p>}
            <dl className="mt-3 grid grid-cols-2 gap-x-8 gap-y-2 lg:grid-cols-3">
                <div><dt className="text-gray-500 dark:text-gray-400">{t('workbuddy.activity.checked_at')}</dt><dd className="mt-1">{date(activity?.checked_at)}</dd></div>
                <div><dt className="text-gray-500 dark:text-gray-400">{t('workbuddy.activity.last_attempt')}</dt><dd className="mt-1">{date(activity?.last_attempt_at)}</dd></div>
                <div><dt className="text-gray-500 dark:text-gray-400">{t('workbuddy.activity.next_run')}</dt><dd className="mt-1">{!account.enabled || !m.managed ? t('workbuddy.activity.paused') : !autoEnabled ? t('workbuddy.activity.auto_off') : (activity?.next_run_at ?? 0) > now / 1000 ? date(activity?.next_run_at) : t('workbuddy.accounts.scheduled')}</dd></div>
            </dl>
            {activity?.error && <p role="status" className="mt-3 break-words text-amber-700 dark:text-amber-400">{activity.error}</p>}
            {items.length > 0 ? <ul className="mt-3 divide-y divide-gray-200 dark:divide-gray-700">
                {items.map(item => <li key={`${item.kind}:${item.id}`} className="flex flex-wrap items-start justify-between gap-x-5 gap-y-1 py-2">
                    <div className="min-w-0 flex-1"><div className="flex flex-wrap items-baseline gap-x-2 gap-y-1"><span className="break-all font-medium">{item.name || item.id}</span><span className="text-gray-500 dark:text-gray-400">{t(`workbuddy.activity.kind_${item.kind}`)}</span></div>{item.detail && <p className="mt-1 break-words text-gray-500 dark:text-gray-400">{item.detail}</p>}</div>
                    <div className="text-right"><div className={statusColor(item.status)}>{t(`workbuddy.activity.item_${item.status}`)}</div><div className="mt-1 tabular-nums text-gray-500 dark:text-gray-400">{credits(item.credits)}</div></div>
                </li>)}
            </ul> : <p className="mt-3 text-gray-500 dark:text-gray-400">{t(activity?.checked_at ? 'workbuddy.activity.empty' : 'workbuddy.activity.not_queried')}</p>}
            <details className="mt-3 border-t border-gray-200 pt-3 dark:border-gray-700">
                <summary className="w-fit cursor-pointer font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500">{t('workbuddy.activity.history', { count: history.length })}</summary>
                {history.length > 0 ? <ol aria-label={t('workbuddy.activity.history_label')} tabIndex={0} className="mt-2 max-h-64 divide-y divide-gray-200 overflow-y-auto focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 dark:divide-gray-700">
                    {history.map((record, index) => <li key={`${record.at}:${record.kind}:${record.id}:${index}`} className="flex flex-wrap items-start justify-between gap-x-5 gap-y-1 py-2 pr-2">
                        <div className="min-w-0 flex-1"><div className="break-all font-medium">{record.name || record.id}</div><div className="mt-1 text-gray-500 dark:text-gray-400">{date(record.at)} · {t(`workbuddy.activity.kind_${record.kind}`)}</div>{record.detail && <p className="mt-1 break-words text-gray-500 dark:text-gray-400">{record.detail}</p>}</div>
                        <div className="text-right"><div className={statusColor(record.status)}>{t(`workbuddy.activity.item_${record.status}`)}</div><div className="mt-1 tabular-nums text-gray-500 dark:text-gray-400">{credits(record.credits)}</div></div>
                    </li>)}
                </ol> : <p className="mt-2 text-gray-500 dark:text-gray-400">{t('workbuddy.activity.no_history')}</p>}
            </details>
        </>}
    </section>;
}
