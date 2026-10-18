import { parseQuotaMillions, toQuotaMillions } from './tokenQuota';

// Money is integer micro-yuan (¥0.000001) — the same six-decimal fixed point as
// million-token quotas, so amounts round-trip through the form exactly.
export const parseYuan = parseQuotaMillions;

export function toYuan(micros: number): string {
    return micros < 0 ? `-${toQuotaMillions(-micros)}` : toQuotaMillions(micros);
}

/** "¥1.50", "¥0.0002", "-¥0.30": at least two decimals, never rounded. */
export function formatYuan(micros: number): string {
    const [whole, fraction = ''] = toQuotaMillions(Math.abs(micros)).split('.');
    return `${micros < 0 ? '-' : ''}¥${whole}.${fraction.padEnd(2, '0')}`;
}

export type Price =
    | { mode: 'per_call'; call_price_micros: number }
    | { mode: 'per_token'; input_price_micros: number; output_price_micros: number; cached_price_micros?: number | null };

export type ModelPrice = Price & { pattern: string };
