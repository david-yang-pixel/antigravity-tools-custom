import { useCallback, useEffect, useRef, useState } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import type { NativeClientVersion } from '../../types/nativeProviders';

export default function QoderClientVersion() {
    const { t } = useTranslation();
    const [version, setVersion] = useState<NativeClientVersion | null>(null);
    const [busy, setBusy] = useState(true);
    const [failed, setFailed] = useState(false);
    const generation = useRef(0);

    const check = useCallback(async (refresh: boolean) => {
        const current = ++generation.current;
        setBusy(true);
        setFailed(false);
        try {
            const result = await request<NativeClientVersion>('check_native_client_version', { request: { provider: 'qoder', refresh } });
            if (generation.current === current) { setVersion(result); setFailed(!!result.error); }
        } catch {
            if (generation.current === current) setFailed(true);
        } finally {
            if (generation.current === current) setBusy(false);
        }
    }, []);

    useEffect(() => {
        void check(false);
        return () => { generation.current++; };
    }, [check]);

    const status = busy ? 'checking' : failed ? 'failed' : version?.update_available ? 'available' : version?.update_available === false ? 'current' : 'unknown';
    const changedProtocol = version && (version.legacy_protocol || version.advertised_version !== version.bundled_version);

    return <div className="rounded-lg border border-gray-200 px-4 py-3 text-sm dark:border-gray-700">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
                <span className="font-medium">{t('nativeProviders.clientVersion.title')}</span>
                {version && <span className="text-gray-600 dark:text-gray-400">{t('nativeProviders.clientVersion.bundled', { version: version.bundled_version })}</span>}
                {version?.latest_version && <span className="text-gray-600 dark:text-gray-400">{t('nativeProviders.clientVersion.latest', { version: version.latest_version })}</span>}
                <span role="status" aria-live="polite" className={`rounded-full px-2 py-0.5 text-xs ${failed || (!busy && version?.update_available) ? 'bg-amber-50 text-amber-800 dark:bg-amber-950 dark:text-amber-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`nativeProviders.clientVersion.${status}`)}</span>
            </div>
            <button type="button" disabled={busy} onClick={() => void check(true)} className="inline-flex items-center gap-1.5 rounded-lg border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800">
                {busy ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <RefreshCw size={13} aria-hidden />}{t('nativeProviders.clientVersion.check')}
            </button>
        </div>
        <p className="mt-2 text-xs text-gray-500">{t('nativeProviders.clientVersion.hint')}</p>
        {version?.checked_at && <p className="mt-1 text-xs text-gray-500">{t('nativeProviders.clientVersion.checkedAt', { time: new Date(version.checked_at * 1000).toLocaleString() })}</p>}
        {changedProtocol && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t(version.legacy_protocol ? 'nativeProviders.clientVersion.legacy' : 'nativeProviders.clientVersion.override', { version: version.advertised_version })}</p>}
        {failed && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('nativeProviders.clientVersion.failureHint')}</p>}
    </div>;
}
