import { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowUpCircle, Loader2, RefreshCw, RotateCcw, Terminal } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import { showToast } from '../common/ToastContainer';

export interface ChatGptClientVersionInfo {
    current: string; source: 'env' | 'saved' | 'default'; default_version: string; min_version: string;
    latest_version: string | null; update_available: boolean | null; checked_at: number | null;
    error: 'network' | 'http' | 'invalid_metadata' | 'proxy' | 'timeout' | null;
}
const button = 'inline-flex items-center gap-1.5 whitespace-nowrap rounded-lg border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800';
const pill = 'whitespace-nowrap rounded-full px-2 py-0.5 text-xs';
const tone = {
    amber: 'bg-amber-50 text-amber-800 dark:bg-amber-950 dark:text-amber-300',
    green: 'bg-green-50 text-green-700 dark:bg-green-950 dark:text-green-300',
    gray: 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300',
};

/** Codex CLI version advertised to chatgpt.com: shows it, checks the latest public release and applies it. */
export default function ChatGptClientVersion({ onSaved }: { onSaved?: () => void }) {
    const { t } = useTranslation();
    const [info, setInfo] = useState<ChatGptClientVersionInfo | null>(null);
    const [checking, setChecking] = useState(true);
    const [saving, setSaving] = useState(false);
    const [failed, setFailed] = useState(false);
    const generation = useRef(0);
    const check = useCallback(async (refresh: boolean) => {
        const current = ++generation.current;
        setChecking(true); setFailed(false);
        try {
            const result = await request<ChatGptClientVersionInfo>('check_chatgpt_client_version', { request: { refresh } });
            if (generation.current === current) { setInfo(result); setFailed(!!result.error); }
        } catch { if (generation.current === current) setFailed(true); }
        finally { if (generation.current === current) setChecking(false); }
    }, []);
    useEffect(() => { void check(false); return () => { generation.current++; }; }, [check]);
    const save = async (version: string | null) => {
        setSaving(true);
        try {
            const result = await request<ChatGptClientVersionInfo>('save_chatgpt_client_version', { request: { version } });
            generation.current++; setInfo(result); setChecking(false);
            showToast(t('nativeProviders.clientVersion.saved', { version: result.current }), 'success');
            onSaved?.();
        } catch (error) { showToast(error instanceof Error ? error.message : String(error), 'error'); }
        finally { setSaving(false); }
    };
    const locked = info?.source === 'env';
    const status = checking ? 'checking' : failed ? 'failed' : info?.update_available ? 'available' : info?.update_available === false ? 'current' : 'unknown';
    const statusTone = status === 'available' || status === 'failed' ? tone.amber : status === 'current' ? tone.green : tone.gray;
    const busy = checking || saving;
    return <div className="rounded-xl border border-gray-200 px-4 py-3 text-sm dark:border-gray-700">
        <div className="flex flex-wrap items-center justify-between gap-3">
            <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
                <Terminal size={16} className="text-blue-600" aria-hidden />
                <span className="font-medium">{t('nativeProviders.clientVersion.title')}</span>
                {info && <code className="rounded bg-gray-100 px-1.5 py-0.5 text-xs dark:bg-gray-800">{info.current}</code>}
                {info && <span className={`${pill} ${tone.gray}`}>{t(`nativeProviders.clientVersion.source_${info.source}`)}</span>}
                {info?.latest_version && <span className="text-xs text-gray-500">{t('nativeProviders.clientVersion.latest', { version: info.latest_version })}</span>}
                <span role="status" aria-live="polite" className={`${pill} ${statusTone}`}>{t(`nativeProviders.clientVersion.${status}`)}</span>
            </div>
            <div className="flex flex-wrap gap-2">
                <button type="button" className={button} disabled={busy} onClick={() => void check(true)}>
                    {checking ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <RefreshCw size={13} aria-hidden />}{t('nativeProviders.clientVersion.check')}
                </button>
                {info?.update_available && info.latest_version && !locked && <button type="button" className={`${button} border-blue-600 bg-blue-600 text-white hover:bg-blue-700 dark:border-blue-500 dark:hover:bg-blue-600`} disabled={busy} onClick={() => void save(info.latest_version)}>
                    {saving ? <Loader2 size={13} className="animate-spin" aria-hidden /> : <ArrowUpCircle size={13} aria-hidden />}{t('nativeProviders.clientVersion.update', { version: info.latest_version })}
                </button>}
                {info?.source === 'saved' && <button type="button" className={button} disabled={busy} onClick={() => void save(null)}>
                    <RotateCcw size={13} aria-hidden />{t('nativeProviders.clientVersion.reset', { version: info.default_version })}
                </button>}
            </div>
        </div>
        <p className="mt-2 text-xs text-gray-500">{locked ? t('nativeProviders.clientVersion.envLocked') : t('nativeProviders.clientVersion.hint')}</p>
        {info?.checked_at && <p className="mt-1 text-xs text-gray-500">{t('nativeProviders.clientVersion.checkedAt', { time: new Date(info.checked_at * 1000).toLocaleString() })}</p>}
        {failed && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('nativeProviders.clientVersion.failureHint')}</p>}
    </div>;
}
