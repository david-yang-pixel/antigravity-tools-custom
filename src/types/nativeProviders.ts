export type NativeProvider = 'qoder' | 'chatgpt';
export type NativeRealm = 'cn' | 'global';
export interface NativeClientVersion {
    bundled_version: string;
    advertised_version: string;
    legacy_protocol: boolean;
    latest_version: string | null;
    update_available: boolean | null;
    checked_at: number | null;
    last_attempt_at: number | null;
    error: 'network' | 'http' | 'invalid_metadata' | 'proxy' | 'timeout' | 'busy' | null;
}
export interface NativeModel { id: string; name: string; context_window: number; vision: boolean; reasoning: boolean; is_free?: boolean }
export interface NativeQuotaPack { name: string; total: number | null; remaining: number | null; used: number | null; unlimited: boolean; expires_at: number | null }
export interface NativeQuota { unit: string; total: number | null; remaining: number | null; used: number | null; unlimited: boolean; updated_at: number; packs: NativeQuotaPack[] }
export interface NativeAccount {
    id: string; provider: NativeProvider; realm: NativeRealm; uid: string; nickname: string;
    enabled: boolean; auto_refresh: boolean; expires_at: number; can_refresh: boolean;
    models: NativeModel[]; models_updated_at: number; quota: NativeQuota | null;
    quota_error: string | null; models_error: string | null; auth_error: string | null;
    last_sync_at: number; next_sync_at: number; in_flight: number; cooldown_until: number;
    last_error: string | null; successes: number; failures: number;
}
export interface NativeSettings { provider: NativeProvider; enabled: boolean; preferred_account_id: string | null; env_locked: boolean }
export interface NativeLogin { id: string; auth_url: string; expires_at: number }
export interface NativeLoginPoll { status: 'pending' | 'authorized'; account: NativeAccount | null; }

export function nativeBalance(quota: NativeQuota | null): string {
    if (!quota) return '—';
    if (quota.unlimited) return '∞';
    return quota.remaining !== null && Number.isFinite(quota.remaining) ? quota.remaining.toLocaleString(undefined, { maximumFractionDigits: 2 }) : '—';
}
export function nativeAccountStatus(account: NativeAccount, now: number): 'disabled' | 'reauthorize' | 'exhausted' | 'cooldown' | 'busy' | 'ready' {
    if (!account.enabled) return 'disabled';
    if (account.auth_error || (account.expires_at > 0 && account.expires_at <= now && !account.can_refresh)) return 'reauthorize';
    if (account.quota && !account.quota.unlimited && account.quota.remaining !== null && account.quota.remaining <= 0 && now - account.quota.updated_at < 900 && !account.models?.some(model => model.is_free)) return 'exhausted';
    if (account.cooldown_until > now) return 'cooldown';
    return account.in_flight > 0 ? 'busy' : 'ready';
}
