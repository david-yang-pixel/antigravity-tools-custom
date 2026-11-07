/** A rate-limit window named by its length, e.g. `5h`, `7d`, `30d`, `90m` (see backend `length_name`). */
export interface QuotaWindow { count: number; unit: 'minute' | 'hour' | 'day' }

const units = { m: 'minute', h: 'hour', d: 'day' } as const;

/** Length and unit of a window name, or `null` when the name is not a window length. */
export function quotaWindow(name: string): QuotaWindow | null {
    const match = /^([1-9]\d*)([mhd])$/.exec(name);
    return match ? { count: Number(match[1]), unit: units[match[2] as keyof typeof units] } : null;
}
