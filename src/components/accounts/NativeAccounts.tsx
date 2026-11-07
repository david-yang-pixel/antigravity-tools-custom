import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { ExternalLink, KeyRound, Loader2, Plus, RefreshCw, Trash2, Upload, X } from 'lucide-react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { request } from '../../utils/request';
import { isTauri } from '../../utils/env';
import ModalDialog from '../common/ModalDialog';
import { showToast } from '../common/ToastContainer';
import WorkBuddyEgressPanel, { WorkBuddyEgressCell, egressArgs, nativeEgress, type EgressView } from './WorkBuddyEgress';
import QoderClientVersion from './QoderClientVersion';
import { nativeAccountStatus, nativeBalance, type NativeAccount, type NativeLogin, type NativeLoginPoll, type NativeProvider, type NativeRealm, type NativeSettings } from '../../types/nativeProviders';

const input = 'w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-800';
const button = 'inline-flex items-center justify-center gap-2 rounded-lg border border-gray-300 px-3 py-2 text-sm hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800';
const action = 'inline-flex shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-lg border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800';
const date = (seconds: number) => seconds > 0 ? new Date(seconds * 1000).toLocaleString() : '—';
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const percent = (value: number) => Math.min(100, Math.max(0, Math.round(value)));
const labels: Record<NativeProvider, string> = { qoder: 'Qoder', chatgpt: 'ChatGPT' };

// ChatGPT quota: one percentage bar per rate-limit window (5h / 7d) with its reset time.
function WindowBars({ quota, t }: { quota: NativeAccount['quota']; t: TFunction }) {
    if (!quota?.packs.length) return <div className="font-medium">—</div>;
    return <div className="space-y-2">{quota.packs.map(pack => {
        const used = percent(pack.used ?? 0);
        const label = pack.name === '5h' ? t('nativeProviders.quota.window5h') : pack.name === '7d' ? t('nativeProviders.quota.window7d') : pack.name;
        return <div key={pack.name}>
            <div className="flex justify-between gap-2 text-xs"><span>{label}</span><span className="tabular-nums text-gray-500">{t('nativeProviders.quota.used', { percent: used })}</span></div>
            <div role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={used} className="mt-1 h-1.5 overflow-hidden rounded-full bg-gray-200 dark:bg-gray-700"><div className={`h-full ${used >= 90 ? 'bg-red-500' : used >= 70 ? 'bg-amber-500' : 'bg-green-500'}`} style={{ width: `${used}%` }} /></div>
            {pack.expires_at ? <div className="mt-0.5 text-xs text-gray-500">{t('nativeProviders.quota.reset', { time: date(pack.expires_at) })}</div> : null}
        </div>;
    })}</div>;
}

export default function NativeAccounts({ provider }: { provider: NativeProvider }) {
    const { t } = useTranslation();
    const label = labels[provider];
    const chatgpt = provider === 'chatgpt';
    const [accounts, setAccounts] = useState<NativeAccount[]>([]);
    const [settings, setSettings] = useState<NativeSettings | null>(null);
    const [loading, setLoading] = useState(true);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [mode, setMode] = useState<'login' | 'import' | null>(null);
    const [realm, setRealm] = useState<NativeRealm>('cn');
    const [login, setLogin] = useState<NativeLogin | null>(null);
    const [target, setTarget] = useState<NativeAccount | null>(null);
    const [auth, setAuth] = useState('');
    const [deleting, setDeleting] = useState<NativeAccount | null>(null);
    const [now, setNow] = useState(Date.now() / 1000);
    const [egress, setEgress] = useState<EgressView | null>(null);
    const egressApi = useMemo(() => nativeEgress(provider), [provider]);
    const alive = useRef(true);
    const locked = useRef(false);
    const sessionRef = useRef<NativeLogin | null>(null);
    const translate = useRef(t); translate.current = t;

    const load = useCallback(async () => {
        try {
            const [rows, settings] = await Promise.all([
                request<NativeAccount[]>('list_native_accounts', { request: { provider } }),
                request<NativeSettings>('get_native_provider_settings', { request: { provider } }),
            ]);
            if (alive.current) { setAccounts(rows); setSettings(settings); }
        } catch (error) { if (alive.current) setError(errorText(error)); }
        finally { if (alive.current) setLoading(false); }
    }, [provider]);
    const loadEgress = useCallback(async () => {
        try { const view = await request<EgressView>(egressApi.get, egressArgs(egressApi)); if (alive.current) setEgress(view); }
        catch (error) { if (alive.current) setError(errorText(error)); }
    }, [egressApi]);

    useEffect(() => {
        alive.current = true; void load(); void loadEgress();
        const tick = window.setInterval(() => { if (alive.current) setNow(Date.now() / 1000); }, 1000);
        const refresh = window.setInterval(() => { if (!locked.current) void load(); }, 30000);
        return () => {
            alive.current = false; window.clearInterval(tick); window.clearInterval(refresh);
            const session = sessionRef.current;
            if (session) void request('cancel_native_login', { request: { provider, id: session.id } }).catch(() => undefined);
        };
    }, [provider, load, loadEgress]);

    useEffect(() => {
        if (!login) return;
        let cancelled = false; let timer: number | undefined;
        const poll = async () => {
            if (cancelled || !alive.current) return;
            if (Date.now() / 1000 >= login.expires_at) { setError(translate.current('nativeProviders.loginExpired')); setLogin(null); sessionRef.current = null; return; }
            try {
                const result = await request<NativeLoginPoll>('poll_native_login', { request: { provider, id: login.id } });
                if (cancelled || !alive.current) return;
                if (result.status === 'authorized') {
                    setLogin(null); sessionRef.current = null; setMode(null); setTarget(null); setError('');
                    showToast(translate.current('nativeProviders.authorized'), 'success'); await load(); void loadEgress(); return;
                }
            } catch (error) { if (!cancelled && alive.current) setError(errorText(error)); }
            if (!cancelled) timer = window.setTimeout(poll, 4000);
        };
        timer = window.setTimeout(poll, 1500);
        return () => { cancelled = true; window.clearTimeout(timer); };
    }, [login, provider, load, loadEgress]);

    async function run(action: () => Promise<void>) {
        if (locked.current) return;
        locked.current = true; setBusy(true); setError('');
        try { await action(); if (alive.current) { await load(); void loadEgress(); window.dispatchEvent(new Event('model-registry-changed')); } }
        catch (error) { if (alive.current) setError(errorText(error)); }
        finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    async function close() {
        const session = sessionRef.current;
        sessionRef.current = null; setLogin(null); setMode(null); setTarget(null); setAuth('');
        if (session) await request('cancel_native_login', { request: { provider, id: session.id } }).catch(() => undefined);
    }
    const change = (account: NativeAccount, patch: { enabled?: boolean; auto_refresh?: boolean; refresh?: boolean }) => run(async () => {
        await request('update_native_account', { request: { provider, id: account.id, ...patch } });
    });
    const begin = () => run(async () => {
        const result = await request<NativeLogin>('start_native_login', { request: { provider, realm: target?.realm || realm, account_id: target?.id || null } });
        if (alive.current) { sessionRef.current = result; setLogin(result); }
        else await request('cancel_native_login', { request: { provider, id: result.id } });
    });
    const importAccount = () => run(async () => {
        let parsed: unknown = auth.trim();
        // ChatGPT accepts a bare token or JSON; Qoder credentials are always JSON.
        if (!chatgpt || /^[{"]/.test(auth.trim())) { try { parsed = JSON.parse(auth); } catch { throw new Error(t('nativeProviders.invalidJson')); } }
        await request('import_native_account', chatgpt ? { request: { provider, auth: parsed, ...(target ? { account_id: target.id } : {}) } } : { request: { provider, realm, auth: parsed } });
        if (alive.current) { setAuth(''); setMode(null); }
    });
    const reimport = (account: NativeAccount) => { setTarget(account); setMode('import'); setAuth(''); setError(''); };
    const reauthorize = (account: NativeAccount) => { setTarget(account); setRealm(account.realm); setMode('login'); setError(''); };
    return <div className="min-h-0 flex-1 overflow-auto px-5 pb-6">
        <div className="mx-auto max-w-7xl space-y-5">
            <div className="flex flex-wrap items-center justify-between gap-3">
                <div><h1 className="text-xl font-semibold">{t('nativeProviders.title', { provider: label })}</h1><p className="mt-2 max-w-3xl text-sm text-gray-500">{t('nativeProviders.description')}</p></div>
                <div className="flex gap-2">
                    <button className={button} disabled={busy || loading} aria-label={t('nativeProviders.reload')} title={t('nativeProviders.reload')} onClick={() => void run(load)}><RefreshCw size={16} /></button>
                    <button className={button} disabled={busy || !!login} onClick={() => { setMode('import'); setTarget(null); setError(''); }}><Upload size={16} />{t('nativeProviders.import')}</button>
                    {!chatgpt && <button className={`${button} !border-blue-600 !bg-blue-600 text-white`} disabled={busy || !!login} onClick={() => { setMode('login'); setTarget(null); setError(''); }}><Plus size={16} />{t('nativeProviders.add')}</button>}
                </div>
            </div>
            {provider === 'qoder' && <QoderClientVersion />}
            <WorkBuddyEgressPanel view={egress} onChange={setEgress} api={egressApi} description={t('nativeProviders.egressDescription', { provider: label })} />
            {error && <div role="alert" className="flex items-start justify-between gap-3 rounded-lg border border-red-200 bg-red-50 p-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300"><span className="break-all">{error}</span><button aria-label={t('nativeProviders.dismiss')} onClick={() => setError('')}><X size={16} /></button></div>}
            {settings && !settings.enabled && <p role="status" className="rounded-lg border border-amber-200 bg-amber-50 p-3 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-300">{t('nativeProviders.sourceDisabled')} <Link className="underline" to="/api-proxy?section=sources">{t('nativeProviders.sourceSettingsLink')}</Link></p>}
            {mode && <div className="space-y-4 rounded-xl border border-blue-200 p-4 dark:border-blue-900">
                <div className="flex items-center justify-between"><h2 className="font-medium">{mode === 'import' ? (target ? t('nativeProviders.chatgpt.reimportTitle') : t('nativeProviders.import')) : target ? t('nativeProviders.reauthorize') : t('nativeProviders.add')}</h2><button className={button} disabled={busy} onClick={() => void close()}>{t('nativeProviders.cancel')}</button></div>
                {!login && !chatgpt && <div className="flex flex-wrap items-center gap-4">
                    <label className="flex items-center gap-2 text-sm">{t('nativeProviders.region')}<select className={`${input} !w-auto`} disabled={busy || !!target} value={target?.realm || realm} onChange={e => setRealm(e.target.value as NativeRealm)}><option value="cn">{t('nativeProviders.cn')}</option><option value="global">{t('nativeProviders.global')}</option></select></label>
                </div>}
                {mode === 'login' && (!login ? <><p className="text-sm text-gray-500">{t('nativeProviders.loginHint')}</p><button className={button} disabled={busy} onClick={() => void begin()}>{busy && <Loader2 size={15} className="animate-spin" />}{t('nativeProviders.getLink')}</button></> :
                    <div className="flex flex-wrap items-center gap-3"><a className={`${button} !border-blue-600 !bg-blue-600 text-white`} href={login.auth_url} target="_blank" rel="noopener noreferrer" onClick={event => {
                        if (isTauri()) { event.preventDefault(); void import('@tauri-apps/plugin-opener').then(({ openUrl }) => openUrl(login.auth_url)).catch(error => setError(errorText(error))); }
                    }}><ExternalLink size={16} />{t('nativeProviders.openLogin')}</a><span role="status" className="text-sm text-gray-500">{t('nativeProviders.waiting', { seconds: Math.max(0, Math.ceil(login.expires_at - now)) })}</span></div>)}
                {mode === 'import' && <><p className="text-sm text-gray-500">{chatgpt ? <>{t('nativeProviders.chatgpt.importHint')}{target && <> {t('nativeProviders.chatgpt.reimportHint')}</>}</> : t('nativeProviders.importHint')}</p><label className="block space-y-2 text-sm"><span>{t('nativeProviders.credentials')}</span><textarea className={`${input} min-h-32 font-mono`} autoComplete="off" spellCheck={false} value={auth} onChange={e => setAuth(e.target.value)} placeholder={chatgpt ? t('nativeProviders.chatgpt.importPlaceholder') : '{"accessToken":"…","refreshToken":"…","expiresAt":0}'} /></label><button className={button} disabled={busy || !auth.trim()} onClick={() => void importAccount()}>{t('nativeProviders.importSave')}</button></>}
            </div>}
            {loading ? <div role="status" className="flex items-center justify-center gap-2 p-10 text-gray-500"><Loader2 size={18} className="animate-spin" />{t('nativeProviders.loading')}</div> : !accounts.length ? <div className="rounded-xl border border-dashed border-gray-300 p-10 text-center dark:border-gray-700"><h2 className="font-medium">{t('nativeProviders.empty')}</h2><p className="mt-2 text-sm text-gray-500">{t('nativeProviders.emptyHint', { provider: label })}</p></div> :
                <div className="overflow-x-auto rounded-xl border border-gray-200 dark:border-gray-700" role="region" aria-label={t('nativeProviders.title', { provider: label })} tabIndex={0}>
                    <table className="w-full text-left text-sm"><thead className="bg-gray-50 dark:bg-gray-800"><tr>{['account', 'egress', 'balance', 'health', 'authorization', 'maintenance', 'actions'].map(key => <th key={key} scope="col" className="whitespace-nowrap px-4 py-3 font-medium">{t(`nativeProviders.${key}`)}</th>)}</tr></thead>
                        <tbody>{accounts.map(account => {
                            const errors = [account.auth_error, account.quota_error, account.models_error, account.last_error].filter((value): value is string => !!value);
                            const status = nativeAccountStatus(account, now);
                            return <tr key={account.id} className="border-t border-gray-100 align-top dark:border-gray-700">
                                <td className="min-w-40 px-4 py-4"><div className="font-medium">{account.nickname || account.uid}</div>{!chatgpt && <div className="mt-1 text-xs text-gray-500">{t(`nativeProviders.${account.realm}`)}</div>}<details className="mt-2 text-xs"><summary className="cursor-pointer text-blue-600">{t('nativeProviders.details', { count: account.models.length })}</summary><div className="mt-2 max-w-sm space-y-2 break-all text-gray-500"><div>{account.uid}</div><p>{t('nativeProviders.modelsUpdated', { time: date(account.models_updated_at) })}</p>{account.models.map(model => <div key={model.id}><code>{model.id}</code>{model.name && <span> · {model.name}</span>}{model.is_free && <span className="ml-2 text-green-700 dark:text-green-400">{t('nativeProviders.freeModel')}</span>}</div>)}{!chatgpt && account.quota?.packs.map((pack, index) => <div key={index}>{pack.name} · {pack.unlimited ? '∞' : pack.remaining ?? '—'} {account.quota?.unit}</div>)}{errors.map((error, index) => <p key={index} className="text-amber-700 dark:text-amber-400">{error}</p>)}</div></details></td>
                                <td className="px-4 py-4"><WorkBuddyEgressCell accountId={account.id} view={egress} disabled={busy} onChange={setEgress} api={egressApi} /></td>
                                <td className="min-w-36 px-4 py-4">{chatgpt ? <WindowBars quota={account.quota} t={t} /> : <div className="font-medium tabular-nums">{nativeBalance(account.quota)} <span className="text-xs font-normal text-gray-500">{account.quota?.unit}</span></div>}<div className="mt-1 text-xs text-gray-500">{account.quota ? date(account.quota.updated_at) : t('nativeProviders.notQueried')}</div>{account.quota_error && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('nativeProviders.staleBalance')}</p>}{!account.quota?.unlimited && account.quota?.remaining !== null && account.quota?.remaining !== undefined && account.quota.remaining <= 0 && account.models.some(model => model.is_free) && <p className="mt-1 text-xs text-green-700 dark:text-green-400">{t('nativeProviders.freeModelsAvailable')}</p>}</td>
                                <td className="min-w-28 px-4 py-4">{account.auth_error && <div role="alert" className="mb-2 max-w-56 rounded-lg border border-red-200 bg-red-50 p-2 text-xs text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300">{chatgpt && <p className="font-medium">{t('nativeProviders.chatgpt.authError')}</p>}<p className="mt-1 break-all opacity-80">{account.auth_error}</p></div>}<span className={`rounded-full px-2 py-1 text-xs ${status === 'ready' ? 'bg-green-50 text-green-700 dark:bg-green-950 dark:text-green-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`nativeProviders.status_${status}`)}</span><div className="mt-2 text-xs tabular-nums text-gray-500">{t('nativeProviders.requests', { success: account.successes, failed: account.failures })}</div>{account.cooldown_until > now && <div className="mt-1 text-xs text-amber-700">{t('nativeProviders.cooldownSeconds', { seconds: Math.ceil(account.cooldown_until - now) })}</div>}</td>
                                <td className="min-w-36 px-4 py-4"><div>{date(account.expires_at)}</div><p className="mt-1 text-xs text-gray-500">{t(account.can_refresh ? 'nativeProviders.refreshable' : 'nativeProviders.noRefresh')}</p></td>
                                <td className="min-w-40 space-y-2 px-4 py-4"><label className="flex items-center gap-2"><input type="checkbox" className="accent-blue-600" disabled={busy} checked={account.enabled} onChange={e => void change(account, { enabled: e.target.checked })} />{t('nativeProviders.enableAccount')}</label><label className="flex items-center gap-2"><input type="checkbox" className="accent-blue-600" disabled={busy} checked={account.auto_refresh} onChange={e => void change(account, { auto_refresh: e.target.checked })} />{t('nativeProviders.autoRefresh')}</label><p className="text-xs text-gray-500">{account.next_sync_at > 0 ? t('nativeProviders.nextSync', { time: date(account.next_sync_at) }) : t('nativeProviders.syncIdle')}</p></td>
                                <td className="px-4 py-4"><div className="flex flex-nowrap items-center gap-2"><button className={action} disabled={busy} onClick={() => void change(account, {})}><RefreshCw size={13} />{t('nativeProviders.sync')}</button><button className={action} disabled={busy || !account.can_refresh} onClick={() => void change(account, { refresh: true })}><KeyRound size={13} />{t('nativeProviders.renew')}</button>{chatgpt ? <button className={action} disabled={busy} onClick={() => reimport(account)}><Upload size={13} />{t('nativeProviders.chatgpt.reimport')}</button> : <button className={action} disabled={busy || !!login} onClick={() => reauthorize(account)}>{t('nativeProviders.reauthorize')}</button>}<button className={`${action} text-red-600`} disabled={busy} onClick={() => setDeleting(account)}><Trash2 size={13} />{t('nativeProviders.delete')}</button></div></td>
                            </tr>;
                        })}</tbody>
                    </table>
                </div>}
            <p className="text-xs leading-relaxed text-gray-500">{t('nativeProviders.routingHint')} <Link className="text-blue-600 underline" to="/api-proxy">{t('nativeProviders.routingLink')}</Link></p>
            <ModalDialog isOpen={!!deleting} title={t('nativeProviders.deleteTitle')} message={t('nativeProviders.deleteHint', { name: deleting?.nickname || deleting?.uid })} type="confirm" isDestructive isLoading={busy} onConfirm={() => void run(async () => { if (!deleting) return; await request('delete_native_account', { request: { provider, id: deleting.id } }); if (alive.current) setDeleting(null); })} onCancel={() => setDeleting(null)} />
        </div>
    </div>;
}
