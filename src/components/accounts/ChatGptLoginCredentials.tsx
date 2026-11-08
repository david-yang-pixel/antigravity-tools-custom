import { useEffect, useState } from 'react';
import { Copy, Eye, EyeOff, KeyRound } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { LoginImportJob, NativeAccount, NativeProvider } from '../../types/nativeProviders';
import { request } from '../../utils/request';
import { copyToClipboard } from '../../utils/clipboard';
import { showToast } from '../common/ToastContainer';

interface LoginCredentials { email: string; password: string; two_factor: string }
const line = (value: LoginCredentials) => `${value.email}----${value.password}----${value.two_factor}`;
const button = 'inline-flex items-center gap-1 rounded border border-gray-300 px-2 py-1 text-xs disabled:opacity-50 dark:border-gray-600';

export function LoginCredentialsCell({ account, disabled, onEdit }: { account: NativeAccount; disabled: boolean; onEdit: () => void }) {
    const { t } = useTranslation();
    const [value, setValue] = useState('');
    const [loading, setLoading] = useState(false);
    async function read(copy = false) {
        setLoading(true);
        try {
            const login = await request<LoginCredentials | null>('get_native_login_credentials', { request: { provider: account.provider, id: account.id } });
            if (!login) throw new Error(t('nativeProviders.login.missing'));
            if (copy) {
                if (!await copyToClipboard(line(login))) throw new Error(t('nativeProviders.login.copyFailed'));
                showToast(t('nativeProviders.login.copied'), 'success');
            } else setValue(line(login));
        } catch (error) { showToast(String(error), 'error'); }
        finally { setLoading(false); }
    }
    return <div className="mt-1.5 space-y-1">
        {account.has_login_credentials && <code className="block max-w-64 break-all text-xs text-gray-500">{value || `${account.nickname || account.uid}----••••----••••`}</code>}
        <div className="flex flex-wrap gap-1">
            {account.has_login_credentials && <>
                <button className={button} disabled={disabled || loading} title={t(value ? 'nativeProviders.login.hide' : 'nativeProviders.login.show')} aria-label={t(value ? 'nativeProviders.login.hide' : 'nativeProviders.login.show')} onClick={() => value ? setValue('') : void read()}>{value ? <EyeOff size={12} /> : <Eye size={12} />}</button>
                <button className={button} disabled={disabled || loading} title={t('nativeProviders.login.copy')} aria-label={t('nativeProviders.login.copy')} onClick={() => void read(true)}><Copy size={12} /></button>
            </>}
            <button className={button} disabled={disabled} onClick={onEdit}><KeyRound size={12} />{t(account.has_login_credentials ? 'nativeProviders.login.edit' : 'nativeProviders.login.add')}</button>
        </div>
    </div>;
}

export function LoginCredentialsEditor({ account, provider, busy, onSave, onClose }: {
    account: NativeAccount | null; provider: NativeProvider; busy: boolean;
    onSave: (input: string, auto: boolean) => void; onClose: () => void;
}) {
    const { t } = useTranslation();
    const [input, setInput] = useState('');
    const [auto, setAuto] = useState(account?.has_login_credentials ? account.auto_reauthorize : true);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState('');
    useEffect(() => {
        if (!account?.has_login_credentials) return;
        const controller = new AbortController();
        let active = true;
        setLoading(true);
        void request<LoginCredentials | null>('get_native_login_credentials', { request: { provider, id: account.id } }, { signal: controller.signal })
            .then(value => { if (active && value) setInput(line(value)); })
            .catch(error => { if (active) setError(String(error)); })
            .finally(() => { if (active) setLoading(false); });
        return () => { active = false; controller.abort(); };
    }, [account, provider]);
    return <div className="space-y-3 rounded-xl border border-blue-200 p-4 dark:border-blue-900">
        <div className="flex justify-between gap-3"><h2 className="font-medium">{t('nativeProviders.login.title')}{account && ` · ${account.nickname || account.uid}`}</h2><button className={button} disabled={busy} onClick={onClose}>{t('nativeProviders.cancel')}</button></div>
        <p className="text-sm text-gray-500">{t('nativeProviders.login.hint')}</p>
        {error && <p role="alert" className="text-sm text-red-600">{error}</p>}
        <label className="block space-y-2 text-sm"><span>email----password----2fa</span><textarea className="min-h-24 w-full rounded-lg border border-gray-300 bg-white p-3 font-mono text-sm dark:border-gray-600 dark:bg-gray-800" autoComplete="off" spellCheck={false} value={input} disabled={busy || loading} onChange={e => setInput(e.target.value)} placeholder="name@example.com----password----BASE32SECRET" /></label>
        <label className="flex items-center gap-2 text-sm"><input type="checkbox" checked={auto} disabled={busy} onChange={e => setAuto(e.target.checked)} />{t('nativeProviders.login.auto')}</label>
        <p className="text-xs text-gray-500">{t('nativeProviders.login.serviceHint')}</p>
        <button className={button} disabled={busy || loading || !input.trim()} onClick={() => onSave(input, auto)}>{t('nativeProviders.login.save')}</button>
    </div>;
}

export function LoginImportJobs({ jobs, busy, onRetry, onRemove }: {
    jobs: LoginImportJob[]; busy: boolean; onRetry: (id: string) => void; onRemove: (id: string) => void;
}) {
    const { t } = useTranslation();
    if (!jobs.length) return null;
    return <section aria-label={t('nativeProviders.login.importTasks')} className="space-y-3 rounded-xl border border-blue-200 p-4 dark:border-blue-900">
        <h2 className="font-medium">{t('nativeProviders.login.importTasks')}</h2>
        <p className="text-xs text-gray-500">{t('nativeProviders.login.importTasksHint')}</p>
        <ul className="space-y-2">{jobs.map(job => <li key={job.id} className="flex flex-wrap items-start justify-between gap-3 rounded-lg bg-gray-50 p-3 text-sm dark:bg-gray-800">
            <div className="min-w-0"><div className="break-all font-medium">{job.email}</div><p role="status" className={`mt-1 text-xs ${job.status === 'failed' ? 'text-red-600' : 'text-blue-600'}`}>{t(`nativeProviders.login.import_${job.status}`)}</p>{job.error && <p className="mt-1 max-w-2xl break-words text-xs text-amber-700">{job.error}</p>}</div>
            <div className="flex gap-2">{job.status === 'failed' && <button className={button} disabled={busy} onClick={() => onRetry(job.id)}>{t('nativeProviders.login.retryImport')}</button>}<button className={button} disabled={busy} onClick={() => onRemove(job.id)}>{t('nativeProviders.login.removeImport')}</button></div>
        </li>)}</ul>
    </section>;
}
