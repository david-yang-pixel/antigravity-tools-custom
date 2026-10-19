import { Fragment, useState } from 'react';
import { CalendarCheck, ChevronDown, HeartPulse, RefreshCw, Trash2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';

export interface WorkBuddyAccount {
    id: string; uid: string; nickname: string; realm: 'cn' | 'global'; enabled: boolean;
    expires_at: number; models: { id: string; name: string }[]; last_error: string | null;
    candidate_model_count?: number; models_verified_at?: number;
    status: string; in_flight: number; cooldown_until: number; cooldown_reason: string | null;
    health: { successes: number; failures: number; consecutive_failures: number; last_success_at: number; last_failure_at: number; last_error: string | null };
    management: {
        managed: boolean; auto_checkin: boolean; keepalive: boolean;
        credits: { available: number; updated_at: number; expiries: { at: number; amount: number }[] } | null;
        credits_error: string | null; last_sync_at: number; next_sync_at: number;
        last_keepalive_at: number; keepalive_attempt_at: number; keepalive_error: string | null;
        checkin_date: string | null; checkin_at: number; checkin_attempt_at?: number; checkin_status: string; checkin_error: string | null;
    };
}
export interface AccountChanges { enabled?: boolean; refresh?: boolean; managed?: boolean; auto_checkin?: boolean; keepalive?: boolean; checkin?: boolean }
const cell = 'px-4 py-4 align-middle';
const muted = 'mt-1 text-xs font-normal text-gray-500 dark:text-gray-400';
const actionButton = 'inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-gray-500 hover:bg-gray-100 hover:text-gray-900 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 disabled:cursor-not-allowed disabled:opacity-40 dark:text-gray-400 dark:hover:bg-gray-800 dark:hover:text-white';

export default function WorkBuddyAccountRow({ account, now, busy, onChange, onDelete }: {
    account: WorkBuddyAccount; now: number; busy: boolean;
    onChange: (changes: AccountChanges) => void; onDelete: () => void;
}) {
    const { t } = useTranslation();
    const [expanded, setExpanded] = useState(false);
    const m = account.management;
    const h = account.health;
    const date = (at: number) => at > 0 ? new Date(at * 1000).toLocaleString() : t('workbuddy.accounts.not_yet');
    const shortDate = (at: number) => new Date(at * 1000).toLocaleDateString();
    const shortTime = (at: number) => new Date(at * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
    const seconds = Math.max(0, Math.ceil(account.cooldown_until - now / 1000));
    const lifetime = Math.max(0, Math.ceil((account.expires_at - now / 1000) / 3600));
    const total = h.successes + h.failures;
    const health = h.consecutive_failures > 0 ? 'degraded' : h.successes > 0 ? 'healthy' : 'unknown';
    const today = new Intl.DateTimeFormat('en-CA', { timeZone: 'Asia/Shanghai', year: 'numeric', month: '2-digit', day: '2-digit' }).format(new Date(now));
    const checked = m.checkin_date === today;
    const checkinStatus = account.realm === 'global' ? 'unsupported' : checked ? m.checkin_status : m.checkin_status === 'failed' ? 'failed' : 'pending';
    const checkinTime = checkinStatus === 'failed'
        ? m.checkin_attempt_at && m.checkin_attempt_at > 0
            ? t('workbuddy.accounts.last_checkin_attempt', { time: date(m.checkin_attempt_at) })
            : t('workbuddy.accounts.checkin_attempt_unknown')
        : checked && m.checkin_at > 0 ? date(m.checkin_at) : undefined;
    const expiring = m.credits?.expiries.filter(e => e.at > now / 1000).sort((a, b) => a.at - b.at)[0];
    const errors = [account.last_error, m.credits_error, m.keepalive_error, m.checkin_error, seconds ? account.cooldown_reason : h.last_error].filter((e, i, all): e is string => !!e && all.indexOf(e) === i);
    const detailId = `workbuddy-details-${account.id}`;
    return <Fragment>
        <tr className={`border-b border-gray-100 text-sm dark:border-gray-800 ${expanded ? 'bg-blue-50/30 dark:bg-blue-950/10' : 'bg-white hover:bg-gray-50/60 dark:bg-gray-900 dark:hover:bg-gray-800/30'}`}>
            <th scope="row" className={`${cell} min-w-44 text-left font-normal`}>
                <div className="flex items-center gap-2"><span className="max-w-48 truncate font-medium" title={account.nickname || account.uid}>{account.nickname || account.uid}</span>
                    <span className={`whitespace-nowrap rounded px-1.5 py-0.5 text-[11px] ${account.status === 'ready' ? 'bg-green-50 text-green-700 dark:bg-green-900/30 dark:text-green-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`workbuddy.accounts.status_${account.status}`)}</span></div>
                <div className={muted}>{t(`workbuddy.accounts.${account.realm}`)} · {t('workbuddy.accounts.model_count', { count: account.models.length })}</div>
                {!account.models_verified_at && !!account.candidate_model_count && <div className="mt-1 text-xs text-amber-600">{t('workbuddy.models_not_verified')}</div>}
                {errors.length > 0 && <button className="mt-1 text-xs text-amber-700 underline dark:text-amber-400" onClick={() => setExpanded(true)} aria-controls={detailId}>{t('workbuddy.accounts.needs_attention')}</button>}
            </th>
            <td className={`${cell} whitespace-nowrap tabular-nums`}>
                <div className="font-semibold">{m.credits ? m.credits.available.toLocaleString(undefined, { maximumFractionDigits: 2 }) : '—'}</div>
                <div className={muted} title={m.credits ? date(m.credits.updated_at) : undefined}>{m.credits_error && m.credits ? t('workbuddy.accounts.stale_credits') : m.credits ? t('workbuddy.accounts.updated_at', { time: shortTime(m.credits.updated_at) }) : t('workbuddy.accounts.not_queried')}</div>
            </td>
            <td className={`${cell} whitespace-nowrap`}>
                <span title={t('workbuddy.accounts.health_hint')} className={health === 'healthy' ? 'text-green-700 dark:text-green-400' : health === 'degraded' ? 'text-amber-700 dark:text-amber-400' : 'text-gray-500'}>{t(`workbuddy.accounts.health_${health}`)}{total > 0 && <span className="ml-2 text-xs tabular-nums">{Math.round(h.successes / total * 100)}%</span>}</span>
                <div className={`${muted} tabular-nums`} title={account.cooldown_reason || undefined}>{seconds ? t('workbuddy.accounts.cooldown_remaining', { time: `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}` }) : t('workbuddy.accounts.no_cooldown')}</div>
            </td>
            <td className={`${cell} whitespace-nowrap`} title={account.expires_at > 0 ? date(account.expires_at) : undefined}>
                <div>{account.expires_at > 0 ? shortDate(account.expires_at) : t('workbuddy.accounts.unknown_expiry')}</div>
                <div className={muted}>{account.expires_at > 0 && t(lifetime ? 'workbuddy.accounts.hours_remaining' : 'workbuddy.accounts.status_expired', { hours: lifetime })}</div>
            </td>
            <td className={`${cell} whitespace-nowrap`}>
                <div title={checkinTime}>{t(`workbuddy.accounts.checkin_${checkinStatus}`)}</div>
                <div className={muted} title={date(m.last_keepalive_at)}>{t('workbuddy.accounts.keepalive_short')} · {m.keepalive_error ? t('common.error') : m.last_keepalive_at > 0 ? shortTime(m.last_keepalive_at) : t('workbuddy.accounts.not_yet')}</div>
            </td>
            <td className={cell}>
                <div className="flex items-center gap-3 whitespace-nowrap text-xs">
                    <label className="flex items-center gap-1.5" title={t('workbuddy.accounts.managed')}><input aria-label={t('workbuddy.accounts.managed')} type="checkbox" className="accent-blue-600" checked={m.managed} disabled={busy} onChange={e => onChange({ managed: e.target.checked })} />{t('workbuddy.accounts.managed_short')}</label>
                    <label className="flex items-center gap-1.5" title={t('workbuddy.accounts.auto_checkin')}><input aria-label={t('workbuddy.accounts.auto_checkin')} type="checkbox" className="accent-blue-600" checked={m.auto_checkin && account.realm === 'cn'} disabled={busy || !m.managed || account.realm === 'global'} onChange={e => onChange({ auto_checkin: e.target.checked })} />{t('workbuddy.accounts.checkin_short')}</label>
                    <label className="flex items-center gap-1.5" title={t('workbuddy.accounts.keepalive')}><input aria-label={t('workbuddy.accounts.keepalive')} type="checkbox" className="accent-blue-600" checked={m.keepalive} disabled={busy || !m.managed} onChange={e => onChange({ keepalive: e.target.checked })} />{t('workbuddy.accounts.keepalive_short')}</label>
                </div>
            </td>
            <td className={`${cell} sticky right-0 bg-white shadow-[-12px_0_12px_-12px_rgba(0,0,0,0.12)] dark:bg-gray-900`}>
                <div className="flex items-center justify-end gap-1">
                    <label className="mr-2 flex items-center gap-1.5 whitespace-nowrap text-xs"><input type="checkbox" className="accent-blue-600" checked={account.enabled} disabled={busy} onChange={e => onChange({ enabled: e.target.checked })} />{t('workbuddy.enable')}</label>
                    <button className={actionButton} disabled={busy} title={t('workbuddy.accounts.sync')} aria-label={t('workbuddy.accounts.sync')} onClick={() => onChange({})}><RefreshCw size={15} /></button>
                    <button className={actionButton} disabled={busy} title={t('workbuddy.accounts.refresh')} aria-label={t('workbuddy.accounts.refresh')} onClick={() => onChange({ refresh: true })}><HeartPulse size={15} /></button>
                    {account.realm === 'cn' && <button className={actionButton} disabled={busy} title={t('workbuddy.accounts.checkin_now')} aria-label={t('workbuddy.accounts.checkin_now')} onClick={() => onChange({ checkin: true })}><CalendarCheck size={15} /></button>}
                    <button className={actionButton} title={t('common.details')} aria-label={`${t('common.details')} ${account.nickname || account.uid}`} aria-expanded={expanded} aria-controls={detailId} onClick={() => setExpanded(!expanded)}><ChevronDown size={16} className={`transition-transform ${expanded ? 'rotate-180' : ''}`} /></button>
                    <button className={`${actionButton} hover:!text-red-600`} disabled={busy} title={t('common.delete')} aria-label={`${t('common.delete')} ${account.nickname || account.uid}`} onClick={onDelete}><Trash2 size={15} /></button>
                </div>
            </td>
        </tr>
        {expanded && <tr id={detailId} className="border-b border-gray-100 bg-gray-50/70 dark:border-gray-800 dark:bg-gray-800/30"><td colSpan={7} className="px-5 py-4">
            <dl className="grid grid-cols-2 gap-x-8 gap-y-4 text-xs lg:grid-cols-4">
                <div><dt className="text-gray-500">{t('workbuddy.accounts.account_id')}</dt><dd className="mt-1 break-all font-mono">{account.uid}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.expires')}</dt><dd className="mt-1">{account.expires_at > 0 ? date(account.expires_at) : t('workbuddy.accounts.unknown_expiry')}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.last_keepalive')}</dt><dd className="mt-1">{date(m.last_keepalive_at)}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.next_sync')}</dt><dd className="mt-1">{m.managed && account.enabled ? m.next_sync_at > now / 1000 ? date(m.next_sync_at) : t('workbuddy.accounts.scheduled') : t('workbuddy.accounts.status_disabled')}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.checkin')}</dt><dd className="mt-1">{t(`workbuddy.accounts.checkin_${checkinStatus}`)}{checkinTime && <> · {checkinTime}</>}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.credits')}</dt><dd className="mt-1">{m.credits ? t('workbuddy.accounts.updated_at', { time: date(m.credits.updated_at) }) : t('workbuddy.accounts.not_queried')}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.credit_expiry')}</dt><dd className="mt-1">{expiring ? t('workbuddy.accounts.credit_expiry_value', { amount: expiring.amount, time: date(expiring.at) }) : '—'}</dd></div>
                <div><dt className="text-gray-500">{t('workbuddy.accounts.health')}</dt><dd className="mt-1">{total ? t('workbuddy.accounts.success_rate', { rate: Math.round(h.successes / total * 100), success: h.successes, total }) : t('workbuddy.accounts.health_hint')}</dd><dd className="mt-1 text-gray-500">{t('workbuddy.accounts.capacity', { count: account.models.length, active: account.in_flight })}</dd></div>
            </dl>
            {errors.length > 0 && <div role="status" className="mt-4 space-y-1 text-xs text-amber-700 dark:text-amber-400">{errors.map(error => <p key={error} className="break-words">{error}</p>)}</div>}
            {account.models.length > 0 && <div className="mt-4 border-t border-gray-200 pt-3 dark:border-gray-700"><h3 className="text-xs font-medium">{account.models_verified_at ? <>{t('workbuddy.accounts.models')} · {date(account.models_verified_at)}</> : t('workbuddy.pending_verification')}</h3><div className="mt-2 flex max-h-40 flex-wrap gap-x-4 gap-y-2 overflow-auto font-mono text-xs text-gray-500">{account.models.map(model => <span key={model.id}>{model.id}</span>)}</div></div>}
        </td></tr>}
    </Fragment>;
}
