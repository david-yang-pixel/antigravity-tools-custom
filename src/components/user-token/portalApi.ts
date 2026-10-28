// Usage-portal account API. Account sessions use an HttpOnly cookie scoped to
// /api/token-portal; key sign-in keeps sending the key as a Bearer header.
import type { TopupConfig } from './PortalTopup';

export interface PortalAnnouncementData {
    level: 'info' | 'warning';
    content: string;
}

export interface PortalConfig {
    registration_enabled: boolean;
    password_reset_enabled: boolean;
    topup?: TopupConfig;
    announcement?: PortalAnnouncementData | null;
}

export interface PortalResult<T = Record<string, unknown>> {
    ok: boolean;
    status: number;
    data: T;
    code?: string;
    retryAfter?: number;
}

export async function portalCall<T = Record<string, unknown>>(path: string, body?: unknown, signal?: AbortSignal): Promise<PortalResult<T>> {
    try {
        const response = await fetch(`/api/token-portal/${path}`, {
            method: body === undefined ? 'GET' : 'POST',
            headers: body === undefined ? undefined : { 'Content-Type': 'application/json' },
            body: body === undefined ? undefined : JSON.stringify(body),
            cache: 'no-store', credentials: 'same-origin', referrerPolicy: 'no-referrer', signal,
        });
        const data = await response.json().catch(() => ({}));
        return {
            ok: response.ok, status: response.status, data,
            code: data?.error?.code, retryAfter: data?.error?.retry_after,
        };
    } catch (error) {
        if ((error as Error)?.name === 'AbortError') throw error;
        return { ok: false, status: 0, data: {} as T, code: 'network' };
    }
}
