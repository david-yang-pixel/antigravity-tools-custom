import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { toQuotaMillions } from './tokenQuota';
import { formatYuan } from './money';

export interface LogTimeRange { start_time?: number; end_time?: number; }

export interface UsagePeriod {
    start_date: string;
    end_date: string;
    requests: number;
    input_tokens: number;
    output_tokens: number;
    cached_tokens: number | null;
    cache_recorded_requests: number;
}
export interface UsageLogPage {
    items: {
        id: string;
        model: string;
        input_tokens: number;
        output_tokens: number;
        cached_tokens: number | null;
        request_time: number;
        status: number;
        /** Charge for this request; null when the key was not billed. */
        cost_micros?: number | null;
        billing_mode?: string | null;
    }[];
    page: number;
    page_size: number;
    total: number;
}
const cell = 'px-5 py-3 tabular-nums';
const tableContainer = 'overflow-x-auto bg-white border border-slate-200 rounded-xl';

export default function TokenUsageDetails({ view, daily, weekly, logs, busy, showCost = false, onPage, onFilter }: {
    view: 'statistics' | 'logs';
    /** Adds a per-request cost column for balance-billed keys. */
    showCost?: boolean;
    daily: UsagePeriod[];
    weekly: UsagePeriod[];
    logs: UsageLogPage;
    busy: boolean;
    onPage: (page: number) => void;
    onFilter?: (range: LogTimeRange) => void;
}) {
    const { t, i18n } = useTranslation();
    const [start, setStart] = useState('');
    const [end, setEnd] = useState('');
    const [rangeError, setRangeError] = useState('');
    const [period, setPeriod] = useState<'day' | 'week'>('day');
    const periods = period === 'day' ? daily : weekly;
    const maxTotal = Math.max(1, ...periods.map(row => row.input_tokens + row.output_tokens));
    const pages = Math.max(1, Math.ceil(logs.total / logs.page_size));
    const cache = (value: number | null) => value === null ? t('token_portal.not_recorded') : toQuotaMillions(value);
    const header = (keys: string[]) => <thead className="bg-slate-50 text-xs text-slate-500"><tr>{keys.map(k => <th key={k} scope="col" className="px-5 py-4 font-medium">{t(k === 'cost' ? 'billing.request_cost' : `token_portal.${k}`)}</th>)}</tr></thead>;

    function filterLogs() {
        const start_time = start ? Math.floor(new Date(`${start}+08:00`).getTime() / 1000) : undefined;
        const end_time = end ? Math.floor(new Date(`${end}+08:00`).getTime() / 1000) : undefined;
        if ((start_time !== undefined && !Number.isFinite(start_time)) || (end_time !== undefined && !Number.isFinite(end_time))
            || (start_time !== undefined && end_time !== undefined && start_time > end_time)) {
            setRangeError(t('token_portal.invalid_time_range')); return;
        }
        setRangeError(''); onFilter?.({ start_time, end_time });
    }

    return <>
        {view === 'statistics' && <section className="space-y-4" aria-labelledby="period-usage-title">
            <div className="flex flex-wrap items-center justify-between gap-3">
                <div><h2 id="period-usage-title" className="text-lg font-semibold">{t('token_portal.period_usage')}</h2><p className="text-xs text-slate-500 mt-1">{t('token_portal.calendar_note')}</p></div>
                <div className="inline-flex rounded-lg border border-slate-200 bg-white p-1" role="group" aria-label={t('token_portal.period_usage')}>
                    {(['day', 'week'] as const).map(value => <button key={value} aria-pressed={period === value} onClick={() => setPeriod(value)} className={`rounded-md px-4 py-2 text-sm ${period === value ? 'bg-blue-600 text-white' : 'text-slate-600 hover:bg-slate-50'}`}>{t(`token_portal.${value}`)}</button>)}
                </div>
            </div>
            <p className="text-xs text-slate-500">{t(period === 'day' ? 'token_portal.recent_days' : 'token_portal.recent_weeks')}</p>
            <div className={`${tableContainer} max-h-96`}>
                <table className="w-full text-sm text-left whitespace-nowrap">
                    {header(['period', 'requests', 'input', 'output', 'cached', 'total'])}
                    <tbody className="divide-y divide-slate-100">{periods.map(row => <tr key={row.start_date}>
                        <th scope="row" className="px-5 py-3 font-normal">{row.start_date}{period === 'week' && <span className="text-xs text-slate-400"> — {row.end_date}</span>}</th>
                        <td className={cell}>{row.requests.toLocaleString()}</td><td className={cell}>{toQuotaMillions(row.input_tokens)}</td><td className={cell}>{toQuotaMillions(row.output_tokens)}</td>
                        <td className={cell}>{cache(row.cached_tokens)}{row.cached_tokens !== null && row.cache_recorded_requests < row.requests && <span className="block text-xs text-slate-400">{t('token_portal.partial_cache')}</span>}</td>
                        <td className={`${cell} min-w-32`}><span className="font-medium">{toQuotaMillions(row.input_tokens + row.output_tokens)}</span><div aria-hidden="true" className="mt-1 h-1 rounded-full bg-slate-100"><div className="h-1 rounded-full bg-blue-400" style={{ width: `${(row.input_tokens + row.output_tokens) / maxTotal * 100}%` }} /></div></td>
                    </tr>)}</tbody>
                </table>
            </div>
        </section>}
        {view === 'logs' && <section className="space-y-4" aria-labelledby="request-logs-title" aria-busy={busy}>
            <div className="flex flex-wrap items-center justify-between gap-3"><h2 id="request-logs-title" className="text-lg font-semibold">{t('token_portal.request_logs')}</h2><span className="text-xs text-slate-500">{t('token_portal.log_count', { count: logs.total })}</span></div>
            {showCost && <p className="text-xs text-slate-500 leading-5">{t('billing.request_cost_hint')}</p>}
            <form onSubmit={event => { event.preventDefault(); filterLogs(); }} className="flex flex-wrap items-end gap-3 bg-white border border-slate-200 rounded-xl p-4">
                <label className="space-y-2 text-xs text-slate-600 flex-1 min-w-0 basis-56">{t('token_portal.start_time')}
                    <input type="datetime-local" step="1" min="1970-01-01T08:00:00" max="9999-12-31T23:59:59" value={start} onChange={e => setStart(e.target.value)} disabled={busy} className="block w-full min-w-0 rounded-lg border border-slate-200 p-2.5 text-sm bg-white" />
                </label>
                <label className="space-y-2 text-xs text-slate-600 flex-1 min-w-0 basis-56">{t('token_portal.end_time')}
                    <input type="datetime-local" step="1" min="1970-01-01T08:00:00" max="9999-12-31T23:59:59" value={end} onChange={e => setEnd(e.target.value)} disabled={busy} className="block w-full min-w-0 rounded-lg border border-slate-200 p-2.5 text-sm bg-white" />
                </label>
                <button type="submit" disabled={busy} className="rounded-lg bg-blue-600 text-white px-4 py-2.5 text-sm disabled:opacity-50">{t('token_portal.apply_filter')}</button>
                <button type="button" disabled={busy} onClick={() => { setStart(''); setEnd(''); setRangeError(''); onFilter?.({}); }} className="rounded-lg border border-slate-200 px-4 py-2.5 text-sm disabled:opacity-50">{t('token_portal.reset_filter')}</button>
                <p className="w-full text-xs text-slate-500">{t('token_portal.time_filter_hint')}</p>
                {rangeError && <p role="alert" className="w-full text-xs text-red-600">{rangeError}</p>}
            </form>
            <div className={tableContainer}>
                <table className="w-full text-sm text-left whitespace-nowrap">
                    {header(['time', 'model', ...(showCost ? ['cost'] : []), 'status', 'input', 'output', 'cached', 'total'])}
                    <tbody className="divide-y divide-slate-100">{logs.items.map(row => <tr key={row.id}>
                        <td className={`${cell} text-xs text-slate-500`}>{new Date(row.request_time * 1000).toLocaleString(i18n.language, { timeZone: 'Asia/Shanghai' })}</td>
                        <th scope="row" className="px-5 py-3 font-mono text-xs font-medium">{row.model}</th>
                        {showCost && <td className={`${cell} font-medium`} title={row.billing_mode ? t(`billing.mode_${row.billing_mode}`) : undefined}>{row.cost_micros === null || row.cost_micros === undefined ? '—' : formatYuan(row.cost_micros)}</td>}
                        <td className={cell}><span className={`rounded px-2 py-1 text-xs ${row.status >= 400 ? 'bg-red-50 text-red-700' : 'bg-emerald-50 text-emerald-700'}`}>{row.status || '—'}</span></td>
                        <td className={cell}>{toQuotaMillions(row.input_tokens)}</td><td className={cell}>{toQuotaMillions(row.output_tokens)}</td><td className={cell}>{cache(row.cached_tokens)}</td><td className={`${cell} font-medium`}>{toQuotaMillions(row.input_tokens + row.output_tokens)}</td>
                    </tr>)}</tbody>
                </table>
                {logs.items.length === 0 && <p className="text-center text-sm text-slate-500 py-10">{t('token_portal.empty_logs')}</p>}
            </div>
            <div className="flex justify-end items-center gap-3 text-sm">
                <button onClick={() => onPage(logs.page - 1)} disabled={busy || logs.page <= 1} className="border border-slate-200 bg-white rounded-lg px-3 py-2 disabled:opacity-40">{t('token_portal.previous')}</button>
                <span className="text-slate-500">{logs.page} / {pages}</span>
                <button onClick={() => onPage(logs.page + 1)} disabled={busy || logs.page >= pages} className="border border-slate-200 bg-white rounded-lg px-3 py-2 disabled:opacity-40">{t('token_portal.next')}</button>
            </div>
            <p className="text-xs text-slate-500 leading-5">{t('token_portal.cache_note')}</p>
        </section>}
    </>;
}
