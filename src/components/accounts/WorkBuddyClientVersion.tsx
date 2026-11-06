import { useCallback, useEffect, useRef, useState } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import type { WorkBuddyAccount } from './WorkBuddyAccountRow';

type Region = 'cn' | 'global';
type Platform = 'linux-x64-deb' | 'linux-arm64-deb' | 'darwin-x64' | 'darwin-arm64' | 'win32-x64-user';
interface Target { region: Region; platform: Platform }
export interface WorkBuddyClientVersionInfo {
    target: Target; verified_target: Target; verified_for_target: boolean;
    bundled_version: string; bundled_release: string; bundled_cli_version: string;
    latest_version: string | null; update_available: boolean | null;
    checked_at: number | null; last_attempt_at: number | null;
    error: 'network' | 'http' | 'invalid_metadata' | 'proxy' | 'timeout' | 'busy' | null;
    legacy_transport: boolean;
}
const platforms: Platform[] = ['linux-x64-deb', 'linux-arm64-deb', 'darwin-x64', 'darwin-arm64', 'win32-x64-user'];
const select = 'rounded-md border border-gray-300 bg-transparent px-2 py-1 text-xs dark:border-gray-600';

export function WorkBuddyClientVersionDetails({ target, version, busy, failed, accounts, onTarget, onCheck }: {
    target: Target; version: WorkBuddyClientVersionInfo | null; busy: boolean; failed: boolean;
    accounts: WorkBuddyAccount[]; onTarget: (target: Target) => void; onCheck: () => void;
}) {
    const { t } = useTranslation();
    const status = busy ? 'checking' : failed ? 'failed' : version && !version.verified_for_target ? 'unverified' : version?.update_available ? 'available' : version?.update_available === false ? 'current' : 'unknown';
    const mismatches = accounts.filter(a => a.client_profile && !a.client_profile.matches_verified_version).length;
    const baseline = version ? `${t(`workbuddy.accounts.${version.verified_target.region}`)} / ${t(`workbuddy.clientVersion.platforms.${version.verified_target.platform}`)}` : '';
    const amber = failed || (!busy && (version?.update_available || (version && !version.verified_for_target)));
    return <div className="rounded-lg border border-gray-200 px-4 py-3 text-sm dark:border-gray-700">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
                <span className="font-medium">{t('workbuddy.clientVersion.title')}</span>
                <select aria-label={t('workbuddy.clientVersion.region')} value={target.region} onChange={e => onTarget({ ...target, region: e.target.value as Region })} className={select}>
                    <option value="cn">{t('workbuddy.accounts.cn')}</option><option value="global">{t('workbuddy.accounts.global')}</option>
                </select>
                <select aria-label={t('workbuddy.clientVersion.platform')} value={target.platform} onChange={e => onTarget({ ...target, platform: e.target.value as Platform })} className={select}>
                    {platforms.map(platform => <option key={platform} value={platform}>{t(`workbuddy.clientVersion.platforms.${platform}`)}</option>)}
                </select>
                {version?.latest_version && <span className="text-gray-600 dark:text-gray-400">{t('workbuddy.clientVersion.latest', { version: version.latest_version })}</span>}
                <span role="status" aria-live="polite" className={`rounded-full px-2 py-0.5 text-xs ${amber ? 'bg-amber-50 text-amber-800 dark:bg-amber-950 dark:text-amber-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`workbuddy.clientVersion.${status}`)}</span>
            </div>
            <button type="button" disabled={busy} onClick={onCheck} className="inline-flex items-center gap-1.5 rounded-lg border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800">
                {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <RefreshCw size={13} aria-hidden />}{t('workbuddy.clientVersion.check')}
            </button>
        </div>
        {version && <p className="mt-2 text-xs text-gray-600 dark:text-gray-400">{t('workbuddy.clientVersion.baseline', { target: baseline, version: version.bundled_release, cli: version.bundled_cli_version })}</p>}
        <p className="mt-2 text-xs text-gray-500">{t('workbuddy.clientVersion.hint')}</p>
        {version?.checked_at && <p className="mt-1 text-xs text-gray-500">{t('workbuddy.clientVersion.checkedAt', { time: new Date(version.checked_at * 1000).toLocaleString() })}</p>}
        {failed && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('workbuddy.clientVersion.failureHint')}</p>}
        {version?.legacy_transport && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('workbuddy.clientVersion.legacy')}</p>}
        {mismatches > 0 && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('workbuddy.clientVersion.mismatches', { count: mismatches })}</p>}
    </div>;
}

export default function WorkBuddyClientVersion({ accounts }: { accounts: WorkBuddyAccount[] }) {
    const [target, setTarget] = useState<Target>({ region: 'cn', platform: 'linux-x64-deb' });
    const [version, setVersion] = useState<WorkBuddyClientVersionInfo | null>(null);
    const [busy, setBusy] = useState(true);
    const [failed, setFailed] = useState(false);
    const generation = useRef(0);
    const check = useCallback(async (refresh: boolean) => {
        const current = ++generation.current;
        setBusy(true); setFailed(false);
        try {
            const result = await request<WorkBuddyClientVersionInfo>('check_workbuddy_client_version', { request: { target, refresh } });
            if (generation.current === current) { setVersion(result); setFailed(!!result.error); }
        } catch { if (generation.current === current) setFailed(true); }
        finally { if (generation.current === current) setBusy(false); }
    }, [target]);
    useEffect(() => {
        void check(false);
        return () => { generation.current++; };
    }, [check]);
    const currentVersion = version?.target.region === target.region && version.target.platform === target.platform ? version : null;
    return <WorkBuddyClientVersionDetails target={target} version={currentVersion} busy={busy} failed={failed} accounts={accounts} onTarget={setTarget} onCheck={() => void check(true)} />;
}
