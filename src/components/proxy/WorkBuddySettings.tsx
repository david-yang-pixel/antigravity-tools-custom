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
    verified_models: string[];
    account_count: number;
    eligible_account_count: number;
    unverified_account_count: number;
    env_locked: string[];
}
type Route = { alias: string; target: string };
interface Verification {
    running: boolean; total: number; completed: number; available: number;
    unavailable: number; failed: number; current_model: string | null;
    started_at: number; finished_at: number; errors: string[];
}
const inputClass = 'w-full min-w-0 rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-800 px-3 py-2 text-xs font-mono outline-none focus:border-blue-500 disabled:opacity-50';

export default function WorkBuddySettings({ onSaved }: { onSaved: () => void }) {
    const { t } = useTranslation();
    const [settings, setSettings] = useState<Settings | null>(null);
    const [routes, setRoutes] = useState<Route[]>([]);
    const [error, setError] = useState('');
    const [saving, setSaving] = useState(false);
    const [verification, setVerification] = useState<Verification | null>(null);
    const [starting, setStarting] = useState(false);
    const [savedEnabled, setSavedEnabled] = useState<boolean | null>(null);
    const busy = saving || starting || !!verification?.running;

    function apply(value: Settings) {
        setSettings(value);
        setSavedEnabled(value.enabled);
        setRoutes(Object.entries(value.models).filter(([, target]) => value.available_models.includes(target)).map(([alias, target]) => ({ alias, target })));
    }
    async function load() {
        setError('');
        try {
            const [value, status] = await Promise.all([request<Settings>('get_workbuddy_settings'), request<Verification>('get_workbuddy_model_verification')]);
            apply(value); setVerification(status);
        }
        catch (e) { setError(String(e)); }
    }
    useEffect(() => { void load(); }, []);
    useEffect(() => {
        if (!verification?.running) return;
        let cancelled = false;
        let timer: ReturnType<typeof setTimeout>;
        async function poll() {
            try {
                const status = await request<Verification>('get_workbuddy_model_verification');
                if (cancelled) return;
                if (!status.running) {
                    const value = await request<Settings>('get_workbuddy_settings');
                    if (!cancelled) { apply(value); setVerification(status); onSaved(); }
                    return;
                }
                setVerification(status);
            } catch (e) { if (!cancelled) setError(String(e)); }
            if (!cancelled) timer = setTimeout(poll, 1500);
        }
        timer = setTimeout(poll, 1000);
        return () => { cancelled = true; clearTimeout(timer); };
    }, [verification?.running, onSaved]);
    const locked = (field: string) => busy || !!settings?.env_locked.includes(field);
    const savedRoutes = settings ? Object.entries(settings.models).filter(([, target]) => settings.available_models.includes(target)).map(([alias, target]) => ({ alias, target })) : [];
    const dirty = !!settings && (savedEnabled !== null && savedEnabled !== settings.enabled || JSON.stringify(routes) !== JSON.stringify(savedRoutes));

    async function fetchModels() {
        setStarting(true); setError('');
        try { setVerification(await request<Verification>('start_workbuddy_model_verification')); }
        catch (e) { setError(String(e)); }
        finally { setStarting(false); }
    }

    async function save() {
        if (!settings) return;
        setError('');
        const normalized = routes.map(({ alias, target }) => ({ alias: alias.trim(), target: target.trim() }));
        if (normalized.some(({ alias, target }) => !alias || !target)
            || new Set(normalized.map(r => r.alias)).size !== normalized.length) {
            setError(t('workbuddy.invalid_routes'));
            return;
        }
        if (normalized.some(({ target }) => !settings.available_models.includes(target))) {
            setError(t('workbuddy.unverified_target'));
            return;
        }
        setSaving(true);
        try {
            const value = await request<Settings>('save_workbuddy_settings', { request: {
                enabled: settings.enabled,
                // Retain hidden routes for provider classification and future re-verification.
                // Failed checks cannot erase provider classification or custom aliases.
                models: { ...Object.fromEntries(Object.entries(settings.models).filter(([, target]) => !settings.available_models.includes(target))),
                    ...Object.fromEntries(normalized.map(r => [r.alias, r.target])) },
            } });
            apply(value);
            setSavedEnabled(value.enabled);
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
            <div className="flex shrink-0 flex-wrap items-center justify-end gap-3">
            <button type="button" className="inline-flex items-center gap-2 rounded-lg border border-blue-200 px-3 py-2 text-xs text-blue-600 hover:bg-blue-50 disabled:cursor-not-allowed disabled:opacity-50 dark:border-blue-800 dark:hover:bg-blue-950"
                disabled={busy || dirty || settings.eligible_account_count === 0} onClick={fetchModels} title={dirty ? t('workbuddy.save_before_fetch') : t('workbuddy.fetch_hint')}>
                <RefreshCw size={14} className={starting || verification?.running ? 'animate-spin' : ''} />{t(verification?.running ? 'workbuddy.verifying_models' : 'workbuddy.fetch_models')}
            </button>
            <label className="flex shrink-0 items-center gap-2">
                <span>{t('workbuddy.enable')}</span>
                <input type="checkbox" className="toggle toggle-sm bg-gray-200 border-gray-300 checked:bg-blue-600 checked:border-blue-600" checked={settings.enabled}
                    disabled={locked('enabled')} onChange={e => setSettings({ ...settings, enabled: e.target.checked })} />
            </label>
            </div>
        </div>
        <p className="text-xs leading-relaxed text-gray-500">{t('workbuddy.fetch_hint')}</p>
        {dirty && <p className="text-xs text-amber-600">{t('workbuddy.save_before_fetch')}</p>}
        {settings.unverified_account_count > 0 && <p className="text-xs text-amber-600">{t('workbuddy.unverified_accounts', { count: settings.unverified_account_count })}</p>}
        {verification?.running && <div role="status" className="space-y-2 text-xs text-blue-600">
            <p>{t('workbuddy.verification_progress', { done: verification.completed, total: verification.total })}{verification.current_model && <> · <span className="font-mono">{verification.current_model}</span></>}</p>
            <progress className="progress progress-primary w-full" value={verification.completed} max={Math.max(1, verification.total)} />
        </div>}
        {!!verification?.finished_at && !verification.running && <p role="status" className="text-xs text-gray-500">{t('workbuddy.verification_result', { available: settings.verified_models.length, unavailable: verification.unavailable, failed: verification.failed, time: new Date(verification.finished_at * 1000).toLocaleString() })}</p>}
        {!!verification?.errors.length && <div role="alert" className="space-y-1 text-xs text-amber-600">{verification.errors.map((message, index) => <p key={index}>{message}</p>)}</div>}
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
                <div className="min-w-0"><input className={inputClass} aria-label={`${t('workbuddy.upstream_model')} ${index + 1}`}
                    list="workbuddy-native-models" value={route.target} disabled={locked('models')} placeholder="glm-5.3-flash"
                    onChange={e => setRoutes(routes.map((r, i) => i === index ? { ...r, target: e.target.value, alias: r.alias === r.target ? e.target.value : r.alias } : r))} />
                    {route.target && !settings.verified_models.includes(route.target) && <span className="mt-1 block text-[11px] text-amber-600">{t('workbuddy.pending_verification')}</span>}</div>
                <button type="button" className="flex items-center justify-center rounded-lg text-gray-400 hover:bg-red-50 hover:text-red-600 disabled:opacity-50" disabled={locked('models')}
                    aria-label={t('workbuddy.remove_route')} onClick={() => setRoutes(routes.filter((_, i) => i !== index))}><Trash2 size={14} /></button>
            </div>)}
            {routes.length === 0 && <p className="rounded-lg bg-gray-50 p-4 text-xs text-gray-500 dark:bg-gray-800">{t('workbuddy.no_verified_models')}</p>}
            <button type="button" className="flex items-center gap-1 rounded-lg px-2 py-2 text-xs text-blue-600 hover:bg-blue-50 disabled:opacity-50" disabled={locked('models') || settings.available_models.length === 0}
                onClick={() => setRoutes([...routes, { alias: '', target: '' }])}><Plus size={14} />{t('workbuddy.add_route')}</button>
            <p className="text-xs text-gray-500 leading-relaxed">{t('workbuddy.routing_hint')}</p>
            <p className="text-xs text-gray-500 leading-relaxed">{t('workbuddy.billing_hint')}</p>
        </div>
        {error && <p role="alert" className="text-sm text-red-600 break-words">{error}</p>}
        <div className="flex justify-end"><button type="button" className="rounded-lg bg-blue-600 px-4 py-2 text-xs font-medium text-white hover:bg-blue-700 disabled:opacity-50" disabled={busy}
            onClick={save}>{saving ? t('common.saving') : t('common.save')}</button></div>
    </div>;
}
