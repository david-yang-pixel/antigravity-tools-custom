import { Link } from 'react-router-dom';
import { useEffect, useState } from 'react';
import { Plus, RefreshCw, Trash2 } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import { showToast } from '../common/ToastContainer';

interface Settings {
    enabled: boolean;
    models: Record<string, string>;
    available_models: string[];
    account_count: number;
    env_locked: string[];
}
type Route = { alias: string; target: string };
const inputClass = 'w-full min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-xs font-mono outline-none focus:border-blue-500 disabled:opacity-50';

export default function WorkBuddySettings({ onSaved }: { onSaved: () => void }) {
    const { t } = useTranslation();
    const [settings, setSettings] = useState<Settings | null>(null);
    const [routes, setRoutes] = useState<Route[]>([]);
    const [error, setError] = useState('');
    const [saving, setSaving] = useState(false);

    function apply(value: Settings) {
        setSettings(value);
        setRoutes(Object.entries(value.models).map(([alias, target]) => ({ alias, target })));
    }
    async function load() {
        setError('');
        try { apply(await request<Settings>('get_workbuddy_settings')); }
        catch (e) { setError(String(e)); }
    }
    useEffect(() => { void load(); }, []);
    const locked = (field: string) => saving || !!settings?.env_locked.includes(field);

    async function save() {
        if (!settings) return;
        setError('');
        const normalized = routes.map(({ alias, target }) => ({ alias: alias.trim(), target: target.trim() }));
        if (normalized.some(({ alias, target }) => !alias || !target)
            || new Set(normalized.map(r => r.alias)).size !== normalized.length) {
            setError(t('workbuddy.invalid_routes'));
            return;
        }
        setSaving(true);
        try {
            const value = await request<Settings>('save_workbuddy_settings', { request: {
                enabled: settings.enabled,
                models: Object.fromEntries(normalized.map(r => [r.alias, r.target])),
            } });
            apply(value);
            onSaved();
            showToast(t('workbuddy.saved'), 'success');
        } catch (e) { setError(String(e)); }
        finally { setSaving(false); }
    }

    if (!settings) return <div className="space-y-2 text-sm">
        {error ? <><p role="alert" className="text-red-600">{error}</p><button className="btn btn-sm" onClick={load}><RefreshCw size={14} />{t('common.retry')}</button></>
            : <p className="text-gray-500">{t('common.loading')}</p>}
    </div>;

    return <div className="space-y-4 text-sm">
        <div className="flex items-start justify-between gap-4">
            <p className="text-xs text-gray-500 dark:text-gray-400 leading-relaxed">{t('workbuddy.description')}</p>
            <label className="flex shrink-0 items-center gap-2">
                <span>{t('workbuddy.enable')}</span>
                <input type="checkbox" className="toggle toggle-sm bg-gray-200 border-gray-300 checked:bg-blue-600 checked:border-blue-600" checked={settings.enabled}
                    disabled={locked('enabled')} onChange={e => setSettings({ ...settings, enabled: e.target.checked })} />
            </label>
        </div>
        {settings.env_locked.length > 0 && <p className="text-xs text-amber-600">{t('workbuddy.env_locked')}</p>}
        <div className="flex flex-wrap items-center gap-3 rounded-lg bg-gray-50 px-3 py-2 text-xs dark:bg-gray-800">
            <span>{t('workbuddy.account_count', { count: settings.account_count })}</span>
            <Link className="text-blue-600 underline dark:text-blue-400" to="/accounts?provider=workbuddy">{t('workbuddy.manage_accounts')}</Link>
        </div>
        <datalist id="workbuddy-native-models">{settings.available_models.map(id => <option key={id} value={id} />)}</datalist>
        <div className="space-y-2">
            <div className="grid grid-cols-[1fr_1fr_2rem] gap-2 text-xs text-gray-500">
                <span>{t('workbuddy.client_model')}</span><span>{t('workbuddy.upstream_model')}</span><span />
            </div>
            {routes.map((route, index) => <div key={index} className="grid grid-cols-[1fr_1fr_2rem] gap-2">
                <input className={inputClass} aria-label={`${t('workbuddy.client_model')} ${index + 1}`}
                    value={route.alias} disabled={locked('models')} placeholder="glm-5.3-flash"
                    onChange={e => setRoutes(routes.map((r, i) => i === index ? { ...r, alias: e.target.value } : r))} />
                <input className={inputClass} aria-label={`${t('workbuddy.upstream_model')} ${index + 1}`}
                    list="workbuddy-native-models" value={route.target} disabled={locked('models')} placeholder="glm-5.3-flash"
                    onChange={e => setRoutes(routes.map((r, i) => i === index ? { ...r, target: e.target.value, alias: r.alias === r.target ? e.target.value : r.alias } : r))} />
                <button type="button" className="flex items-center justify-center rounded-lg text-gray-400 hover:bg-red-50 hover:text-red-600 disabled:opacity-50" disabled={locked('models')}
                    aria-label={t('workbuddy.remove_route')} onClick={() => setRoutes(routes.filter((_, i) => i !== index))}><Trash2 size={14} /></button>
            </div>)}
            <button type="button" className="flex items-center gap-1 rounded-lg px-2 py-2 text-xs text-blue-600 hover:bg-blue-50 disabled:opacity-50" disabled={locked('models')}
                onClick={() => setRoutes([...routes, { alias: '', target: '' }])}><Plus size={14} />{t('workbuddy.add_route')}</button>
            <p className="text-xs text-gray-500 leading-relaxed">{t('workbuddy.routing_hint')}</p>
            <p className="text-xs text-gray-500 leading-relaxed">{t('workbuddy.billing_hint')}</p>
        </div>
        {error && <p role="alert" className="text-sm text-red-600 break-words">{error}</p>}
        <div className="flex justify-end"><button type="button" className="rounded-lg bg-blue-600 px-4 py-2 text-xs font-medium text-white hover:bg-blue-700 disabled:opacity-50" disabled={saving}
            onClick={save}>{saving ? t('common.saving') : t('common.save')}</button></div>
    </div>;
}
