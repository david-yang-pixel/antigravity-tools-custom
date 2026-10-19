import { useEffect, useState } from 'react';
import { RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';

interface Account { id: string; nickname: string; uid: string; enabled: boolean }
interface Settings { preferred_account_id: string | null }

export default function WorkBuddyScheduling() {
    const { t } = useTranslation();
    const [accounts, setAccounts] = useState<Account[]>([]);
    const [selected, setSelected] = useState<string | null>(null);
    const [busy, setBusy] = useState(true);
    const [error, setError] = useState('');

    async function load() {
        setBusy(true);
        setError('');
        try {
            const [list, settings] = await Promise.all([
                request<Account[]>('list_workbuddy_accounts'),
                request<Settings>('get_workbuddy_settings'),
            ]);
            setAccounts(list);
            setSelected(settings.preferred_account_id);
        } catch (e) { setError(String(e)); }
        finally { setBusy(false); }
    }
    useEffect(() => { void load(); }, []);

    async function change(id: string) {
        setBusy(true);
        setError('');
        try {
            const saved = await request<Settings>('save_workbuddy_settings', {
                request: { preferred_account_id: id || null },
            });
            setSelected(saved.preferred_account_id);
        } catch (e) { setError(String(e)); }
        finally { setBusy(false); }
    }

    return <div className="mt-4 space-y-2 border-t border-indigo-200 pt-3 dark:border-indigo-800">
        <div className="flex items-center justify-between gap-2">
            <label htmlFor="workbuddy-fixed-account" className="text-xs font-medium text-gray-700 dark:text-gray-300">
                {t('workbuddy.scheduling.fixed_account')}
            </label>
            <button type="button" onClick={load} disabled={busy} className="rounded p-1 text-gray-500 hover:text-blue-600 disabled:opacity-50" aria-label={t('workbuddy.scheduling.refresh_accounts')}>
                <RefreshCw size={14} className={busy ? 'animate-spin' : ''} />
            </button>
        </div>
        <select id="workbuddy-fixed-account" className="select select-bordered select-sm w-full text-xs" value={selected ?? ''} disabled={busy} onChange={e => void change(e.target.value)}>
            <option value="">{t('workbuddy.scheduling.automatic')}</option>
            {selected && !accounts.some(a => a.id === selected) && <option value={selected} disabled>{t('workbuddy.scheduling.unavailable')}</option>}
            {accounts.map(a => <option key={a.id} value={a.id} disabled={!a.enabled}>
                {a.nickname || a.uid}{!a.enabled ? ` · ${t('workbuddy.accounts.status_disabled')}` : ''}
            </option>)}
        </select>
        <p className="text-[10px] leading-relaxed text-gray-500">{t('workbuddy.scheduling.hint')}</p>
        {error && <p role="alert" className="break-words text-xs text-red-600">{error}</p>}
    </div>;
}
