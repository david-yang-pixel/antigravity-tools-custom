const pad = (n: number) => String(n).padStart(2, '0');

// <input type="datetime-local"> holds local wall-clock time; toISOString() would shift it to UTC.
export function toDateTimeLocal(seconds: number): string {
    const d = new Date(seconds * 1000);
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

export function fromDateTimeLocal(value: string): number {
    return Math.floor(new Date(value).getTime() / 1000);
}

export const nowSeconds = () => Math.floor(Date.now() / 1000);
