import { useCallback, useEffect, useRef, useState } from 'react';
import { CheckCircle2, ChevronLeft, ChevronRight, Clock, CreditCard, RefreshCw, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { formatYuan } from './money';
import {
    displayOrderStatus, forgetReturnedOrder, formatCountdown, rememberReturnedOrder,
    remainingSeconds, serverNow, toCents,
    type OrderFilter, type OrderPage, type ServerClock, type TopupConfig, type TopupOrder,
} from './portalTopupState';

export type { TopupConfig, TopupOrder } from './portalTopupState';
export { takeReturnedOrder } from './portalTopupState';

type Request = (path: string, init?: RequestInit) => Promise<Response>;
type ReturnState = 'checking' | 'paid' | 'waiting' | 'closed';
const PAGE_SIZE = 10;
const POLL_MS = 2000;
const POLL_TRIES = 20;
const PAYMENT_TIMEOUT_MS = 20_000;

/** Balance page, with resumable pending payments and complete paginated history. */
export default function PortalTopup({ config, balanceMicros, expired, active, refreshKey, request, returnedOrder, onReturnHandled, onSettled }: {
    config: TopupConfig | undefined;
    balanceMicros: number;
    expired: boolean;
    active: boolean;
    refreshKey: number;
    request: Request;
    returnedOrder: string | null;
    onReturnHandled: () => void;
    onSettled: () => void;
}) {
    const { t, i18n } = useTranslation();
    const [visible, setVisible] = useState(() => document.visibilityState !== 'hidden');
    const [open, setOpen] = useState(false);
    const [amount, setAmount] = useState('');
    const [payType, setPayType] = useState('');
    const [submitting, setSubmitting] = useState<string | null>(null);
    const [error, setError] = useState('');
    const [loadError, setLoadError] = useState(false);
    const [loading, setLoading] = useState(false);
    const [history, setHistory] = useState<OrderPage | null>(null);
    const [historyQuery, setHistoryQuery] = useState({ filter: 'paid' as OrderFilter, page: 1 });
    const [filter, setFilter] = useState<OrderFilter>('paid');
    const [page, setPage] = useState(1);
    const [reloadCount, setReloadCount] = useState(0);
    const [clock, setClock] = useState<ServerClock | null>(null);
    const [now, setNow] = useState(0);
    const [returnState, setReturnState] = useState<ReturnState | null>(null);
    const [returnAmount, setReturnAmount] = useState<number | null>(null);
    const callbacks = useRef({ onSettled, onReturnHandled });
    callbacks.current = { onSettled, onReturnHandled };
    const mounted = useRef(true);
    const payment = useRef<AbortController | null>(null);
    const navigationTimer = useRef<number | null>(null);
    const paymentLocked = useRef(false);
    const dialog = useRef<HTMLFormElement>(null);
    const amountInput = useRef<HTMLInputElement>(null);
    const lastFocus = useRef<HTMLElement | null>(null);
    const expiryRefresh = useRef('');
    const canPay = !expired && config?.enabled === true;
    const foreground = active && visible;
    const types = config?.pay_types ?? [];
    const typeList = types.join(',');
    const min = config?.min_amount_cents ?? 0;
    const max = config?.max_amount_cents ?? 0;
    const cents = toCents(amount);
    const valid = canPay && cents !== null && cents >= min && cents <= max && types.includes(payType);
    const date = (ts: number) => new Date(ts * 1000).toLocaleString(i18n.language, { timeZone: 'Asia/Shanghai' });
    const refresh = useCallback(() => setReloadCount(value => value + 1), []);
    const syncClock = useCallback((seconds: number) => {
        setClock({ seconds, receivedAt: performance.now() });
        setNow(seconds);
    }, []);
    const close = () => { if (!paymentLocked.current) setOpen(false); };

    useEffect(() => {
        mounted.current = true;
        const clearNavigation = () => {
            if (navigationTimer.current !== null) window.clearTimeout(navigationTimer.current);
            navigationTimer.current = null;
        };
        const visibility = () => setVisible(document.visibilityState !== 'hidden');
        const restore = (event: PageTransitionEvent) => {
            if (!event.persisted) return;
            clearNavigation();
            payment.current?.abort(); payment.current = null;
            paymentLocked.current = false;
            setSubmitting(null); setOpen(false); refresh();
        };
        document.addEventListener('visibilitychange', visibility);
        window.addEventListener('pageshow', restore);
        window.addEventListener('pagehide', clearNavigation);
        return () => {
            mounted.current = false; payment.current?.abort();
            clearNavigation();
            document.removeEventListener('visibilitychange', visibility);
            window.removeEventListener('pageshow', restore);
            window.removeEventListener('pagehide', clearNavigation);
        };
    }, [refresh]);

    useEffect(() => {
        if (foreground) return;
        if (navigationTimer.current !== null) window.clearTimeout(navigationTimer.current);
        navigationTimer.current = null;
        payment.current?.abort(); payment.current = null;
        paymentLocked.current = false;
        setSubmitting(null); setOpen(false);
    }, [foreground]);

    useEffect(() => {
        const offered = typeList ? typeList.split(',') : [];
        if (!offered.includes(payType)) setPayType(offered[0] ?? '');
    }, [typeList, payType]);

    useEffect(() => {
        if (!open) return;
        lastFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
        amountInput.current?.focus();
        const previousOverflow = document.body.style.overflow;
        document.body.style.overflow = 'hidden';
        return () => {
            document.body.style.overflow = previousOverflow;
            if (lastFocus.current?.isConnected) lastFocus.current.focus();
        };
    }, [open]);

    useEffect(() => {
        if (open && submitting !== null) dialog.current?.focus();
    }, [open, submitting]);

    // Each list request owns an abort signal, so a stale page/filter cannot replace newer data.
    useEffect(() => {
        if (!foreground) return;
        const controller = new AbortController();
        setLoading(true); setLoadError(false);
        void (async () => {
            try {
                const query = new URLSearchParams({ status: filter, page: String(page), page_size: String(PAGE_SIZE) });
                const response = await request(`topup/orders?${query}`, { signal: controller.signal });
                if (!response.ok) throw new Error('load');
                const data = await response.json() as OrderPage;
                if (controller.signal.aborted) return;
                syncClock(data.server_time);
                const lastPage = Math.max(1, Math.ceil(data.total / data.page_size));
                if (page > lastPage) { setPage(lastPage); return; }
                setHistory(data); setHistoryQuery({ filter, page });
            } catch {
                if (!controller.signal.aborted) setLoadError(true);
            } finally {
                if (!controller.signal.aborted) setLoading(false);
            }
        })();
        return () => controller.abort();
    }, [foreground, filter, page, refreshKey, reloadCount, request, syncClock]);

    // No timers or polling while another menu/browser tab is shown.
    useEffect(() => {
        if (!foreground || !clock) return;
        const tick = () => setNow(serverNow(clock, performance.now()));
        tick();
        const timer = window.setInterval(tick, 1000);
        return () => window.clearInterval(timer);
    }, [foreground, clock]);

    const expiredOrders = (history?.pending_orders ?? [])
        .filter(order => remainingSeconds(order, now) === 0).map(order => order.out_trade_no).sort().join(',');
    useEffect(() => {
        if (foreground && expiredOrders && expiryRefresh.current !== expiredOrders) {
            expiryRefresh.current = expiredOrders;
            refresh();
        }
    }, [expiredOrders, foreground, refresh]);

    // Gateway returns are independently checked even when their order is not on the current page.
    useEffect(() => {
        if (!returnedOrder || !foreground) return;
        const controller = new AbortController();
        let timer: ReturnType<typeof setTimeout> | undefined;
        let attempts = 0;
        setReturnState('checking');
        const finish = (state: ReturnState | null) => {
            forgetReturnedOrder(); setReturnState(state);
            callbacks.current.onReturnHandled(); refresh();
        };
        const check = async () => {
            attempts++;
            try {
                const response = await request(`topup/orders/${returnedOrder}`, { signal: controller.signal });
                if (controller.signal.aborted) return;
                if (response.status === 404) { finish(null); return; }
                if (response.ok) {
                    const data = await response.json() as { order: TopupOrder; server_time: number };
                    if (controller.signal.aborted) return;
                    syncClock(data.server_time);
                    const status = displayOrderStatus(data.order, data.server_time);
                    if (status === 'paid') {
                        setReturnAmount(data.order.credit_micros); finish('paid');
                        callbacks.current.onSettled(); return;
                    }
                    if (status === 'closed') { finish('closed'); return; }
                }
            } catch { /* A transient request failure is retried while this page remains visible. */ }
            if (controller.signal.aborted) return;
            if (attempts >= POLL_TRIES) { finish('waiting'); return; }
            timer = setTimeout(() => void check(), POLL_MS);
        };
        void check();
        return () => { controller.abort(); clearTimeout(timer); };
    }, [returnedOrder, foreground, request, refresh, syncClock]);

    async function pay(order?: TopupOrder) {
        if (!canPay || paymentLocked.current || (!order && (!valid || cents === null))) return;
        if (order && displayOrderStatus(order, now) !== 'pending') { refresh(); return; }
        paymentLocked.current = true;
        const controller = new AbortController();
        payment.current = controller;
        setSubmitting(order?.out_trade_no ?? 'new'); setError('');
        let leaving = false;
        let timedOut = false;
        const timeout = window.setTimeout(() => { timedOut = true; controller.abort(); }, PAYMENT_TIMEOUT_MS);
        try {
            const response = await request(order ? `topup/orders/${order.out_trade_no}/pay` : 'topup', {
                method: 'POST', signal: controller.signal,
                ...(!order ? { headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ amount: ((cents ?? 0) / 100).toFixed(2), pay_type: payType }) } : {}),
            });
            const data = await response.json();
            controller.signal.throwIfAborted();
            if (controller.signal.aborted || !mounted.current) return;
            if (!response.ok) {
                const code = data?.error?.code ?? 'unavailable';
                setError(t(`topup.error_${code}`, { defaultValue: t('topup.error_unavailable') }));
                if (code === 'order_expired' || code === 'order_not_found') refresh();
                return;
            }
            syncClock(data.server_time);
            if (data.order?.status === 'paid') {
                forgetReturnedOrder(); setReturnAmount(data.order.credit_micros); setReturnState('paid');
                callbacks.current.onReturnHandled(); callbacks.current.onSettled(); refresh();
                return;
            }
            if (typeof data.action !== 'string' || !data.fields || typeof data.order?.out_trade_no !== 'string') throw new Error('invalid payment');
            // Store only the order number: browser Back may return without gateway URL parameters.
            rememberReturnedOrder(data.order.out_trade_no);
            const form = document.createElement('form');
            form.method = 'POST'; form.action = data.action; form.acceptCharset = 'UTF-8';
            for (const [name, value] of Object.entries(data.fields as Record<string, string>)) {
                const input = document.createElement('input');
                input.type = 'hidden'; input.name = name; input.value = value; form.appendChild(input);
            }
            document.body.appendChild(form);
            try {
                HTMLFormElement.prototype.submit.call(form);
                leaving = true;
                // Submitting a form does not guarantee navigation completed.
                // Browser Stop or a blocked navigation must not lock this page forever.
                navigationTimer.current = window.setTimeout(() => {
                    navigationTimer.current = null;
                    if (!mounted.current || payment.current !== controller) return;
                    payment.current = null; paymentLocked.current = false; setSubmitting(null);
                    setError(t('topup.error_payment_timeout')); refresh();
                }, PAYMENT_TIMEOUT_MS);
            } finally { form.remove(); }
        } catch {
            if (mounted.current && payment.current === controller && (timedOut || !controller.signal.aborted)) {
                setError(t(timedOut ? 'topup.error_payment_timeout' : 'topup.error_unavailable'));
                // The server may already have saved the order. Fetch it instead
                // of silently retrying creation and producing a duplicate.
                refresh();
            }
        } finally {
            window.clearTimeout(timeout);
            if (!leaving && payment.current === controller && mounted.current) {
                payment.current = null; paymentLocked.current = false; setSubmitting(null);
            }
        }
    }

    const pendingOrders = (history?.pending_orders ?? []).filter(order => displayOrderStatus(order, now) === 'pending');
    const currentHistory = historyQuery.filter === filter && historyQuery.page === page ? history : null;
    const pages = Math.max(1, Math.ceil((historyQuery.filter === filter ? history?.total ?? 0 : 0) / PAGE_SIZE));
    const badge = (order: TopupOrder) => {
        const status = displayOrderStatus(order, now);
        return <span className={`rounded px-2 py-0.5 text-xs ${status === 'paid' ? 'bg-emerald-50 text-emerald-700' : status === 'pending' ? 'bg-blue-50 text-blue-700' : 'bg-slate-100 text-slate-500'}`}>{t(`topup.status_${status}`)}</span>;
    };

    return <div className="space-y-6">
        {returnState && <div role="status" className={`flex items-start gap-2 rounded-lg px-4 py-3 text-sm ${returnState === 'paid' ? 'bg-emerald-50 text-emerald-800' : returnState === 'checking' ? 'bg-blue-50 text-blue-800' : 'bg-amber-50 text-amber-800'}`}>
            {returnState === 'checking' ? <RefreshCw size={16} className="animate-spin shrink-0 mt-0.5" /> : returnState === 'paid' ? <CheckCircle2 size={16} className="shrink-0 mt-0.5" /> : <Clock size={16} className="shrink-0 mt-0.5" />}
            <span className="flex-1">{returnState === 'paid' ? t('topup.return_paid', { amount: formatYuan(returnAmount ?? 0) }) : t(`topup.return_${returnState}`)}</span>
            {returnState !== 'checking' && <button type="button" onClick={() => setReturnState(null)} aria-label={t('common.close')} className="shrink-0 rounded focus-visible:ring-2 focus-visible:ring-blue-500"><X size={16} /></button>}
        </div>}
        <section className="bg-white border border-slate-200 rounded-xl p-5 sm:p-6 space-y-4">
            <div className="flex flex-wrap items-center justify-between gap-4">
                <div><h2 className="text-sm text-slate-500 mb-2">{t('billing.balance')}</h2><p className={`text-3xl font-semibold tracking-tight tabular-nums ${balanceMicros <= 0 ? 'text-amber-700' : ''}`}>{formatYuan(balanceMicros)}</p></div>
                <button type="button" disabled={!canPay || submitting !== null} onClick={() => { setOpen(true); setError(''); }} className="inline-flex items-center gap-2 rounded-lg bg-blue-600 text-white px-4 py-2.5 text-sm font-medium hover:bg-blue-700 disabled:opacity-50 focus-visible:ring-2 focus-visible:ring-blue-500 focus-visible:ring-offset-2"><CreditCard size={16} />{t('topup.button')}</button>
            </div>
            <p className="text-xs text-slate-500 leading-5">{t('billing.portal_balance_hint')}</p>
            <p className="text-xs text-slate-500">{canPay ? t('topup.hint', { min: formatYuan(min * 10_000) }) : t(expired ? 'topup.error_topup_key_expired' : 'topup.error_topup_disabled')}</p>
        </section>
        {error && !open && <p role="alert" className="text-sm text-red-600 rounded-lg bg-red-50 px-4 py-3">{error}</p>}
        {loadError && <div role="alert" className="flex flex-wrap items-center justify-between gap-3 text-sm text-red-600 rounded-lg bg-red-50 px-4 py-3"><span>{t('topup.load_error')}</span><button type="button" onClick={refresh} className="underline focus-visible:ring-2 focus-visible:ring-red-500">{t('common.refresh')}</button></div>}
        <section aria-labelledby="pending-orders-title" className="bg-white border border-slate-200 rounded-xl p-5 sm:p-6 space-y-4" aria-busy={loading}>
            <h2 id="pending-orders-title" className="font-semibold">{t('topup.pending_orders')}</h2>
            <p className="text-xs text-slate-500">{t('topup.pending_hint')}</p>
            {pendingOrders.length > 0 ? <div className="divide-y divide-slate-100">
                {pendingOrders.map(order => <div key={order.out_trade_no} className="flex flex-wrap items-center justify-between gap-3 py-3 first:pt-0 last:pb-0">
                    <div className="min-w-0 space-y-1"><p className="text-sm font-medium tabular-nums">{formatYuan(order.amount_cents * 10_000)} <span className="ml-2 font-normal text-slate-500">{t(`topup.type_${order.pay_type}`, { defaultValue: order.pay_type })}</span></p><p className="text-xs text-slate-500 break-all">{t('topup.col_order')}: {order.out_trade_no} · {date(order.created_at)}</p></div>
                    <div className="flex flex-wrap items-center gap-3"><span className="text-xs text-blue-700 tabular-nums inline-flex items-center gap-1.5"><Clock size={14} />{t('topup.remaining', { time: formatCountdown(remainingSeconds(order, now)) })}</span><button type="button" disabled={!canPay || submitting !== null} onClick={() => void pay(order)} className="inline-flex items-center gap-2 rounded-lg border border-blue-200 text-blue-700 px-3 py-2 text-sm hover:bg-blue-50 disabled:opacity-50 focus-visible:ring-2 focus-visible:ring-blue-500">{submitting === order.out_trade_no && <RefreshCw size={14} className="animate-spin" />}{t('topup.continue_pay')}</button></div>
                </div>)}
            </div> : <p className="text-sm text-slate-500 py-3">{t(loading && !history ? 'topup.loading' : loadError && !history ? 'topup.history_unavailable' : 'topup.empty_pending')}</p>}
        </section>
        <section aria-labelledby="topup-history-title" className="bg-white border border-slate-200 rounded-xl p-5 sm:p-6 space-y-4" aria-busy={loading}>
            <div className="flex flex-wrap justify-between items-center gap-3"><h2 id="topup-history-title" className="font-semibold">{t('topup.orders')}</h2><div role="group" aria-label={t('topup.filter')} className="inline-flex gap-1 rounded-lg bg-slate-100 p-1">
                {(['paid', 'closed', 'all'] as const).map(value => <button type="button" key={value} aria-pressed={filter === value} onClick={() => { setFilter(value); setPage(1); }} className={`rounded-md px-3 py-1.5 text-xs font-medium focus-visible:ring-2 focus-visible:ring-blue-500 ${filter === value ? 'bg-white text-blue-700 shadow-sm' : 'text-slate-500 hover:text-slate-900'}`}>{t(`topup.filter_${value}`)}</button>)}
            </div></div>
            <div className="overflow-x-auto border border-slate-100 rounded-lg">
                <table className="w-full text-sm text-left whitespace-nowrap"><thead className="bg-slate-50 text-xs text-slate-500"><tr>{['time', 'order', 'amount', 'method', 'status'].map(key => <th key={key} scope="col" className="px-4 py-2.5 font-medium">{t(`topup.col_${key}`)}</th>)}</tr></thead>
                    <tbody className="divide-y divide-slate-100">{currentHistory?.orders.map(order => <tr key={order.out_trade_no}><td className="px-4 py-3 text-xs text-slate-500">{date(order.paid_at ?? order.created_at)}</td><td className="px-4 py-3 text-xs font-mono text-slate-500">{order.out_trade_no}</td><td className="px-4 py-3 tabular-nums">{formatYuan(order.amount_cents * 10_000)}</td><td className="px-4 py-3">{t(`topup.type_${order.pay_type}`, { defaultValue: order.pay_type })}</td><td className="px-4 py-3">{badge(order)}</td></tr>)}</tbody>
                </table>
                {(!currentHistory || currentHistory.orders.length === 0) && <p className="text-sm text-center text-slate-500 py-10">{t(loading ? 'topup.loading' : loadError ? 'topup.history_unavailable' : `topup.empty_${filter}`)}</p>}
            </div>
            <div className="flex flex-wrap justify-between items-center gap-3 text-xs text-slate-500"><span>{loading ? t('topup.loading') : currentHistory ? t('topup.pagination', { page, pages, total: currentHistory.total }) : ''}</span><div className="flex gap-2"><button type="button" aria-label={t('topup.previous')} disabled={loading || page <= 1} onClick={() => setPage(value => value - 1)} className="inline-flex items-center gap-1 border border-slate-200 rounded-lg px-3 py-2 disabled:opacity-40 focus-visible:ring-2 focus-visible:ring-blue-500"><ChevronLeft size={14} />{t('topup.previous')}</button><button type="button" aria-label={t('topup.next')} disabled={loading || page >= pages} onClick={() => setPage(value => value + 1)} className="inline-flex items-center gap-1 border border-slate-200 rounded-lg px-3 py-2 disabled:opacity-40 focus-visible:ring-2 focus-visible:ring-blue-500">{t('topup.next')}<ChevronRight size={14} /></button></div></div>
        </section>
        {open && <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/40 px-4 py-6" role="dialog" aria-modal="true" aria-labelledby="topup-title" onKeyDown={event => {
            if (event.key === 'Escape') { event.preventDefault(); close(); }
            if (event.key === 'Tab') {
                const focusable = dialog.current?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), [tabindex="0"]');
                if (!focusable?.length) { event.preventDefault(); dialog.current?.focus(); return; }
                const first = focusable[0], last = focusable[focusable.length - 1];
                if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
                else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
            }
        }}>
            <form ref={dialog} tabIndex={-1} onSubmit={event => { event.preventDefault(); void pay(); }} className="w-full max-w-md max-h-full overflow-y-auto bg-white rounded-2xl shadow-xl p-6 space-y-5">
                <div className="flex items-center justify-between"><h2 id="topup-title" className="text-lg font-semibold">{t('topup.title')}</h2><button type="button" disabled={submitting !== null} onClick={close} aria-label={t('common.close')} className="rounded-lg p-1.5 hover:bg-slate-100 focus-visible:ring-2 focus-visible:ring-blue-500 disabled:opacity-50"><X size={18} /></button></div>
                <label className="block space-y-2 text-sm"><span className="font-medium">{t('topup.amount')}</span><div className="flex items-center rounded-lg border border-slate-200 focus-within:ring-2 focus-within:ring-blue-500"><span className="pl-3 text-slate-400">¥</span><input ref={amountInput} type="text" inputMode="decimal" disabled={submitting !== null} value={amount} onChange={event => setAmount(event.target.value)} placeholder={t('topup.amount_placeholder', { min: formatYuan(min * 10_000) })} aria-describedby="topup-amount-rule" className="w-full rounded-lg px-2 py-2.5 text-base outline-none" /></div><span id="topup-amount-rule" className={`block text-xs ${amount && !valid ? 'text-red-600' : 'text-slate-500'}`}>{t('topup.amount_rule', { min: formatYuan(min * 10_000), max: formatYuan(max * 10_000) })}</span></label>
                <fieldset disabled={submitting !== null} className="space-y-2 text-sm"><legend className="font-medium mb-2">{t('topup.method')}</legend><div className="flex flex-wrap gap-2">{types.map(type => <label key={type} className={`flex items-center gap-2 rounded-lg border px-3 py-2 cursor-pointer focus-within:ring-2 focus-within:ring-blue-500 ${payType === type ? 'border-blue-500 bg-blue-50 text-blue-700' : 'border-slate-200'}`}><input type="radio" name="topup-type" value={type} checked={payType === type} onChange={() => setPayType(type)} className="sr-only" />{t(`topup.type_${type}`, { defaultValue: type })}</label>)}</div></fieldset>
                {valid && cents !== null && <p className="text-sm text-slate-600">{t('topup.credit_note', { amount: formatYuan(cents * 10_000) })}</p>}
                {error && <p role="alert" className="text-sm text-red-600">{error}</p>}
                <p className="text-xs text-slate-500 leading-5">{t('topup.redirect_note')}</p>
                <div className="flex justify-end gap-2"><button type="button" disabled={submitting !== null} onClick={close} className="rounded-lg border border-slate-200 px-4 py-2 text-sm focus-visible:ring-2 focus-visible:ring-blue-500 disabled:opacity-50">{t('common.cancel')}</button><button type="submit" disabled={!valid || submitting !== null} className="inline-flex items-center gap-2 rounded-lg bg-blue-600 text-white px-4 py-2 text-sm font-medium disabled:opacity-50 focus-visible:ring-2 focus-visible:ring-blue-500 focus-visible:ring-offset-2">{submitting !== null && <RefreshCw size={14} className="animate-spin" />}{t('topup.pay')}</button></div>
            </form>
        </div>}
    </div>;
}
