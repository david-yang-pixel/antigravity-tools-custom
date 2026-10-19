import { useEffect, useRef, useState } from 'react';
import { CheckCircle2, Clock, CreditCard, RefreshCw, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { formatYuan } from './money';

export interface TopupConfig {
    enabled: boolean;
    pay_types: string[];
    min_amount_cents: number;
    max_amount_cents: number;
}

export interface TopupOrder {
    out_trade_no: string;
    amount_cents: number;
    credit_micros: number;
    pay_type: string;
    status: 'pending' | 'paid' | 'closed';
    created_at: number;
    paid_at: number | null;
}

type Request = (path: string, init?: RequestInit) => Promise<Response>;
type ReturnState = 'checking' | 'paid' | 'waiting';

const RETURN_KEY = 'portal_topup_return';
const POLL_MS = 2000;
const POLL_TRIES = 20;

/** Order number the gateway sent the user back with (or one remembered across a re-login). */
export function takeReturnedOrder(): string | null {
    const params = new URLSearchParams(window.location.search);
    const fromUrl = params.get('out_trade_no');
    if (fromUrl !== null) {
        // The gateway appends its signed result to the return URL; only the order
        // number is used (the server re-checks the order itself).
        window.history.replaceState(null, '', window.location.pathname);
        if (/^\d{1,32}$/.test(fromUrl)) {
            try { sessionStorage.setItem(RETURN_KEY, fromUrl); } catch { /* storage unavailable */ }
            return fromUrl;
        }
    }
    try { return sessionStorage.getItem(RETURN_KEY); } catch { return null; }
}

function forgetReturnedOrder() {
    try { sessionStorage.removeItem(RETURN_KEY); } catch { /* storage unavailable */ }
}

/** Yuan with at most two decimals → cents, or null. */
function toCents(value: string): number | null {
    const match = /^(\d{1,9})(?:\.(\d{1,2}))?$/.exec(value.trim());
    return match ? Number(match[1]) * 100 + Number((match[2] ?? '').padEnd(2, '0')) : null;
}

/** Balance top-up: start a payment, confirm it on return, list recent orders. */
export default function PortalTopup({ config, orders, request, returnedOrder, onReturnHandled, onSettled }: {
    config: TopupConfig | undefined;
    orders: TopupOrder[];
    request: Request;
    returnedOrder: string | null;
    /** The returned order needs no more checking (paid, timed out or not this key's). */
    onReturnHandled: () => void;
    onSettled: () => void;
}) {
    const { t, i18n } = useTranslation();
    const [open, setOpen] = useState(false);
    const [amount, setAmount] = useState('');
    const [payType, setPayType] = useState('');
    const [submitting, setSubmitting] = useState(false);
    const [error, setError] = useState('');
    const [returnState, setReturnState] = useState<ReturnState | null>(null);
    const [returnAmount, setReturnAmount] = useState<number | null>(null);
    const settled = useRef(onSettled);
    settled.current = onSettled;
    const handled = useRef(onReturnHandled);
    handled.current = onReturnHandled;
    const trigger = useRef<HTMLButtonElement>(null);
    const close = () => { setOpen(false); trigger.current?.focus(); };

    // Coming back to this page from the browser cache after leaving for the
    // cashier: unlock the dialog again.
    useEffect(() => {
        const unlock = (event: PageTransitionEvent) => { if (event.persisted) setSubmitting(false); };
        window.addEventListener('pageshow', unlock);
        return () => window.removeEventListener('pageshow', unlock);
    }, []);

    const types = config?.pay_types ?? [];
    const typeList = types.join(',');
    useEffect(() => {
        const offered = typeList ? typeList.split(',') : [];
        if (!offered.includes(payType)) setPayType(offered[0] ?? '');
    }, [typeList, payType]);

    // Back from the gateway: ask the server (which re-checks the gateway) until the order settles.
    useEffect(() => {
        if (!returnedOrder) return;
        let cancelled = false;
        setReturnState('checking');
        void (async () => {
            for (let attempt = 0; attempt < POLL_TRIES && !cancelled; attempt++) {
                try {
                    const response = await request(`topup/orders/${returnedOrder}`);
                    if (response.status === 404) {
                        // Not this key's order (e.g. signed in with another key): nothing to confirm.
                        if (!cancelled) { forgetReturnedOrder(); setReturnState(null); handled.current(); }
                        return;
                    }
                    if (response.ok) {
                        const { order } = await response.json() as { order: TopupOrder };
                        if (order.status === 'paid') {
                            if (cancelled) return;
                            forgetReturnedOrder();
                            setReturnAmount(order.credit_micros);
                            setReturnState('paid');
                            handled.current();
                            settled.current();
                            return;
                        }
                    }
                } catch { /* retry */ }
                await new Promise(resolve => setTimeout(resolve, POLL_MS));
            }
            if (!cancelled) { forgetReturnedOrder(); setReturnState('waiting'); handled.current(); }
        })();
        return () => { cancelled = true; };
    }, [returnedOrder, request]);

    if (!config?.enabled && orders.length === 0 && !returnState) return null;
    const min = config?.min_amount_cents ?? 0;
    const max = config?.max_amount_cents ?? 0;
    const cents = toCents(amount);
    const valid = cents !== null && cents >= min && cents <= max && payType !== '';
    const date = (ts: number) => new Date(ts * 1000).toLocaleString(i18n.language, { timeZone: 'Asia/Shanghai' });

    async function pay() {
        if (!valid || cents === null) return;
        setSubmitting(true); setError('');
        try {
            const response = await request('topup', {
                method: 'POST', headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({ amount: (cents / 100).toFixed(2), pay_type: payType }),
            });
            const data = await response.json().catch(() => ({}));
            if (!response.ok) {
                const code = data?.error?.code ?? 'unavailable';
                setError(t(`topup.error_${code}`, { defaultValue: t('topup.error_unavailable') }));
                setSubmitting(false);
                return;
            }
            // Hand the browser to the gateway cashier with the server-signed form.
            const form = document.createElement('form');
            form.method = 'POST';
            form.action = data.action;
            form.acceptCharset = 'UTF-8';
            for (const [name, value] of Object.entries(data.fields as Record<string, string>)) {
                const input = document.createElement('input');
                input.type = 'hidden'; input.name = name; input.value = value;
                form.appendChild(input);
            }
            document.body.appendChild(form);
            // Stay locked while the browser leaves for the cashier, so a second
            // click cannot open a duplicate order.
            form.submit();
        } catch {
            setError(t('topup.error_unavailable'));
            setSubmitting(false);
        }
    }

    return <>
        {returnState && <div role="status" className={`flex items-start gap-2 rounded-lg px-4 py-3 text-sm ${returnState === 'paid' ? 'bg-emerald-50 text-emerald-800' : returnState === 'checking' ? 'bg-blue-50 text-blue-800' : 'bg-amber-50 text-amber-800'}`}>
            {returnState === 'checking' ? <RefreshCw size={16} className="animate-spin shrink-0 mt-0.5" /> : returnState === 'paid' ? <CheckCircle2 size={16} className="shrink-0 mt-0.5" /> : <Clock size={16} className="shrink-0 mt-0.5" />}
            <span className="flex-1">{returnState === 'paid' ? t('topup.return_paid', { amount: formatYuan(returnAmount ?? 0) }) : t(`topup.return_${returnState}`)}</span>
            {returnState !== 'checking' && <button type="button" onClick={() => setReturnState(null)} aria-label={t('common.close')} className="shrink-0"><X size={16} /></button>}
        </div>}
        {config?.enabled && <div className="flex flex-wrap items-center justify-between gap-3 pt-1">
            <p className="text-xs text-slate-500">{t('topup.hint', { min: formatYuan(min * 10_000) })}</p>
            <button ref={trigger} type="button" onClick={() => { setOpen(true); setError(''); }} className="inline-flex items-center gap-2 rounded-lg bg-blue-600 text-white px-4 py-2 text-sm font-medium hover:bg-blue-700 focus-visible:ring-2 focus-visible:ring-blue-500 focus-visible:ring-offset-2">
                <CreditCard size={16} />{t('topup.button')}
            </button>
        </div>}
        {orders.length > 0 && <div className="pt-2">
            <h3 className="text-sm font-medium mb-2">{t('topup.orders')}</h3>
            <div className="overflow-x-auto border border-slate-100 rounded-lg">
                <table className="w-full text-sm text-left whitespace-nowrap">
                    <thead className="bg-slate-50 text-xs text-slate-500"><tr>
                        {['time', 'amount', 'method', 'status'].map(key => <th key={key} scope="col" className="px-4 py-2.5 font-medium">{t(`topup.col_${key}`)}</th>)}
                    </tr></thead>
                    <tbody className="divide-y divide-slate-100">{orders.map(order => <tr key={order.out_trade_no}>
                        <td className="px-4 py-2.5 text-xs text-slate-500">{date(order.created_at)}</td>
                        <td className="px-4 py-2.5 tabular-nums">{formatYuan(order.amount_cents * 10_000)}</td>
                        <td className="px-4 py-2.5">{t(`topup.type_${order.pay_type}`, { defaultValue: order.pay_type })}</td>
                        <td className="px-4 py-2.5"><span className={`rounded px-2 py-0.5 text-xs ${order.status === 'paid' ? 'bg-emerald-50 text-emerald-700' : order.status === 'pending' ? 'bg-blue-50 text-blue-700' : 'bg-slate-100 text-slate-500'}`}>{t(`topup.status_${order.status}`)}</span></td>
                    </tr>)}</tbody>
                </table>
            </div>
        </div>}
        {open && <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/40 px-4" role="dialog" aria-modal="true" aria-labelledby="topup-title"
            onKeyDown={event => { if (event.key === 'Escape') close(); }}>
            <form onSubmit={event => { event.preventDefault(); void pay(); }} className="w-full max-w-md bg-white rounded-2xl shadow-xl p-6 space-y-5">
                <div className="flex items-center justify-between">
                    <h2 id="topup-title" className="text-lg font-semibold">{t('topup.title')}</h2>
                    <button type="button" onClick={close} aria-label={t('common.close')} className="rounded-lg p-1.5 hover:bg-slate-100"><X size={18} /></button>
                </div>
                <label className="block space-y-2 text-sm">
                    <span className="font-medium">{t('topup.amount')}</span>
                    <div className="flex items-center rounded-lg border border-slate-200 focus-within:ring-2 focus-within:ring-blue-500">
                        <span className="pl-3 text-slate-400">¥</span>
                        <input autoFocus type="text" inputMode="decimal" value={amount} onChange={e => setAmount(e.target.value)}
                            placeholder={t('topup.amount_placeholder', { min: formatYuan(min * 10_000) })}
                            className="w-full rounded-lg px-2 py-2.5 text-base outline-none" />
                    </div>
                    <span className={`block text-xs ${amount && !valid ? 'text-red-600' : 'text-slate-500'}`}>
                        {t('topup.amount_rule', { min: formatYuan(min * 10_000), max: formatYuan(max * 10_000) })}
                    </span>
                </label>
                <fieldset className="space-y-2 text-sm">
                    <legend className="font-medium mb-2">{t('topup.method')}</legend>
                    <div className="flex flex-wrap gap-2">
                        {types.map(type => <label key={type} className={`flex items-center gap-2 rounded-lg border px-3 py-2 cursor-pointer focus-within:ring-2 focus-within:ring-blue-500 ${payType === type ? 'border-blue-500 bg-blue-50 text-blue-700' : 'border-slate-200'}`}>
                            <input type="radio" name="topup-type" value={type} checked={payType === type} onChange={() => setPayType(type)} className="sr-only" />
                            {t(`topup.type_${type}`, { defaultValue: type })}
                        </label>)}
                    </div>
                </fieldset>
                {valid && cents !== null && <p className="text-sm text-slate-600">{t('topup.credit_note', { amount: formatYuan(cents * 10_000) })}</p>}
                {error && <p role="alert" className="text-sm text-red-600">{error}</p>}
                <p className="text-xs text-slate-500 leading-5">{t('topup.redirect_note')}</p>
                <div className="flex justify-end gap-2">
                    <button type="button" onClick={close} className="rounded-lg border border-slate-200 px-4 py-2 text-sm">{t('common.cancel')}</button>
                    <button type="submit" disabled={!valid || submitting} className="inline-flex items-center gap-2 rounded-lg bg-blue-600 text-white px-4 py-2 text-sm font-medium disabled:opacity-50">
                        {submitting && <RefreshCw size={14} className="animate-spin" />}{t('topup.pay')}
                    </button>
                </div>
            </form>
        </div>}
    </>;
}
