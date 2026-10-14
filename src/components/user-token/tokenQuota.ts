// Convert decimal million-token values without floating-point multiplication,
// so opening and saving an existing quota never changes its exact token count.
export function parseQuotaMillions(value: string): number | null {
    const match = /^(\d+)(?:\.(\d{1,6}))?$/.exec(value);
    if (!match) return null;
    const tokens = Number(match[1] + (match[2] ?? '').padEnd(6, '0'));
    return Number.isSafeInteger(tokens) ? tokens : null;
}

export function toQuotaMillions(tokens: number): string {
    const whole = Math.floor(tokens / 1_000_000);
    const fraction = String(tokens % 1_000_000).padStart(6, '0').replace(/0+$/, '');
    return fraction ? `${whole}.${fraction}` : String(whole);
}

export function validTokenLimits(concurrency: string, quota: string): boolean {
    return /^\d+$/.test(concurrency)
        && Number.isSafeInteger(Number(concurrency)) && Number(concurrency) <= 2147483647
        && parseQuotaMillions(quota) !== null;
}
