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
    expires_at: number;
    paid_at: number | null;
}

export type OrderFilter = 'paid' | 'closed' | 'all';
export interface OrderPage {
    orders: TopupOrder[];
    pending_orders: TopupOrder[];
    total: number;
    page: number;
    page_size: number;
    server_time: number;
}
export interface ServerClock { seconds: number; receivedAt: number }

/** Use a monotonic local clock only for elapsed time, never the device's wall clock. */
export function serverNow(clock: ServerClock, monotonicNow: number): number {
    return clock.seconds + Math.max(0, monotonicNow - clock.receivedAt) / 1000;
}

export function remainingSeconds(order: TopupOrder, now: number): number {
    return Math.max(0, Math.ceil(order.expires_at - now));
}

export function displayOrderStatus(order: TopupOrder, now: number): TopupOrder['status'] {
    return order.status === 'pending' && remainingSeconds(order, now) === 0 ? 'closed' : order.status;
}

export function formatCountdown(seconds: number): string {
    const hours = Math.floor(seconds / 3600);
    const minutes = Math.floor(seconds % 3600 / 60);
    const rest = String(seconds % 60).padStart(2, '0');
    return hours > 0 ? `${hours}:${String(minutes).padStart(2, '0')}:${rest}` : `${minutes}:${rest}`;
}

/** Yuan with at most two decimals → cents, or null. */
export function toCents(value: string): number | null {
    const match = /^(\d{1,9})(?:\.(\d{1,2}))?$/.exec(value.trim());
    return match ? Number(match[1]) * 100 + Number((match[2] ?? '').padEnd(2, '0')) : null;
}

const RETURN_KEY = 'portal_topup_return';
export function rememberReturnedOrder(orderNo: string) {
    if (!/^\d{1,32}$/.test(orderNo)) return;
    try { sessionStorage.setItem(RETURN_KEY, orderNo); } catch { /* storage unavailable */ }
}

/** Retain only the order number across the gateway and a possible re-login. */
export function takeReturnedOrder(): string | null {
    const params = new URLSearchParams(window.location.search);
    const fromUrl = params.get('out_trade_no');
    if (fromUrl !== null) {
        window.history.replaceState(null, '', window.location.pathname);
        if (/^\d{1,32}$/.test(fromUrl)) {
            rememberReturnedOrder(fromUrl);
            return fromUrl;
        }
    }
    try {
        const remembered = sessionStorage.getItem(RETURN_KEY);
        return remembered && /^\d{1,32}$/.test(remembered) ? remembered : null;
    } catch { return null; }
}

export function forgetReturnedOrder() {
    try { sessionStorage.removeItem(RETURN_KEY); } catch { /* storage unavailable */ }
}
