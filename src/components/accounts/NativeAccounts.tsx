import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { KeyRound, Loader2, RefreshCw, Trash2, Upload, X } from 'lucide-react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';
import { request } from '../../utils/request';
import { quotaWindow } from '../../utils/quotaWindow';
import ModalDialog from '../common/ModalDialog';
import Pagination from '../common/Pagination';
import { showToast } from '../common/ToastContainer';
import ChatGptClientVersion from './ChatGptClientVersion';
import { AuthorizationServiceFields, AuthorizationServiceLabel, LoginCredentialsCell, LoginCredentialsEditor, LoginImportJobs } from './ChatGptLoginCredentials';
import WorkBuddyEgressPanel, { WorkBuddyEgressCell, egressArgs, nativeEgress, type EgressView } from './WorkBuddyEgress';
import { nativeAccountStatus, type AuthorizationConfig, type BatchItem, type LoginImportJob, type LoginImportReport, type NativeAccount, type NativeProvider, type NativeSettings } from '../../types/nativeProviders';

const PAGE_SIZE = 10;
const input = 'w-full rounded-lg border border-gray-300 bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-800';
const button = 'inline-flex items-center justify-center gap-2 rounded-lg border border-gray-300 px-3 py-2 text-sm hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800';
const action = 'inline-flex shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-lg border border-gray-300 px-2.5 py-1.5 text-xs hover:bg-gray-50 disabled:opacity-50 dark:border-gray-600 dark:hover:bg-gray-800';
const date = (seconds: number) => seconds > 0 ? new Date(seconds * 1000).toLocaleString() : '—';
// Month, day and time only: keeps reset and maintenance times on one line.
const shortDate = (seconds: number) => seconds > 0 ? new Date(seconds * 1000).toLocaleString(undefined, { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit' }) : '—';
const errorText = (error: unknown) => error instanceof Error ? error.message : String(error);
const percent = (value: number) => Math.min(100, Math.max(0, Math.round(value)));
const labels: Record<NativeProvider, string> = { chatgpt: 'ChatGPT' };

const windowKeys = { minute: 'nativeProviders.quota.windowMinutes', hour: 'nativeProviders.quota.windowHours', day: 'nativeProviders.quota.windowDays' } as const;

// ChatGPT quota: one percentage bar per rate-limit window, named by its reported length
// (e.g. 5 hours, 7 days, or 30 days on free accounts), with its reset time. Windows sit
// side by side so a row stays as short as one window.
function WindowBars({ quota, t }: { quota: NativeAccount['quota']; t: TFunction }) {
    if (!quota?.packs.length) return <div className="font-medium">—</div>;
    return <div className="grid grid-flow-col auto-cols-[minmax(7.5rem,1fr)] gap-4">{quota.packs.map(pack => {
        const used = percent(pack.used ?? 0);
        const window = quotaWindow(pack.name);
        const label = window ? t(windowKeys[window.unit], { count: window.count }) : pack.name;
        return <div key={pack.name}>
            <div className="flex justify-between gap-2 whitespace-nowrap text-xs"><span>{label}</span><span className="tabular-nums text-gray-500">{t('nativeProviders.quota.used', { percent: used })}</span></div>
            <div role="progressbar" aria-label={label} aria-valuemin={0} aria-valuemax={100} aria-valuenow={used} className="mt-1 h-1.5 overflow-hidden rounded-full bg-gray-200 dark:bg-gray-700"><div className={`h-full ${used >= 90 ? 'bg-red-500' : used >= 70 ? 'bg-amber-500' : 'bg-green-500'}`} style={{ width: `${used}%` }} /></div>
            {pack.expires_at ? <div className="mt-0.5 whitespace-nowrap text-xs text-gray-500" title={date(pack.expires_at)}>{t('nativeProviders.quota.reset', { time: shortDate(pack.expires_at) })}</div> : null}
        </div>;
    })}</div>;
}

export default function NativeAccounts({ provider }: { provider: NativeProvider }) {
    const { t } = useTranslation();
    const label = labels[provider];
    const [accounts, setAccounts] = useState<NativeAccount[]>([]);
    const [loginImports, setLoginImports] = useState<LoginImportJob[]>([]);
    const [settings, setSettings] = useState<NativeSettings | null>(null);
    const [loading, setLoading] = useState(true);
    const [busy, setBusy] = useState(false);
    const [error, setError] = useState('');
    const [importing, setImporting] = useState(false);
    const [target, setTarget] = useState<NativeAccount | null>(null);
    const [auth, setAuth] = useState('');
    const [authorization, setAuthorization] = useState<AuthorizationConfig>({ service: 'built_in', cdk: '' });
    const [deleting, setDeleting] = useState<NativeAccount | null>(null);
    const [editingLogins, setEditingLogins] = useState(false);
    const [loginTarget, setLoginTarget] = useState<NativeAccount | null>(null);
    const [loginVersion, setLoginVersion] = useState(0);
    const [now, setNow] = useState(Date.now() / 1000);
    const [egress, setEgress] = useState<EgressView | null>(null);
    const [page, setPage] = useState(1);
    const egressApi = useMemo(() => nativeEgress(provider), [provider]);
    const alive = useRef(true);
    const locked = useRef(false);
    const totalPages = Math.max(1, Math.ceil(accounts.length / PAGE_SIZE));
    const currentPage = Math.min(page, totalPages);
    const visibleAccounts = accounts.slice((currentPage - 1) * PAGE_SIZE, currentPage * PAGE_SIZE);
    useEffect(() => { setPage(current => Math.min(current, totalPages)); }, [totalPages]);

    const load = useCallback(async () => {
        try {
            // Read jobs first: a removed successful job has already saved its new account.
            const jobs = await request<LoginImportJob[]>('list_native_login_imports', { request: { provider } });
            const [rows, settings] = await Promise.all([
                request<NativeAccount[]>('list_native_accounts', { request: { provider } }),
                request<NativeSettings>('get_native_provider_settings', { request: { provider } }),
            ]);
            if (alive.current) { setAccounts(rows); setSettings(settings); setLoginImports(jobs); }
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
        return () => { alive.current = false; window.clearInterval(tick); window.clearInterval(refresh); };
    }, [provider, load, loadEgress]);
    const activeImports = loginImports.some(job => job.status !== 'failed');
    useEffect(() => {
        if (!activeImports) return;
        const tick = window.setInterval(() => { if (!locked.current) void load(); }, 5000);
        return () => window.clearInterval(tick);
    }, [activeImports, load]);

    async function run(action: () => Promise<void>) {
        if (locked.current) return;
        locked.current = true; setBusy(true); setError('');
        try { await action(); if (alive.current) { await load(); void loadEgress(); window.dispatchEvent(new Event('model-registry-changed')); } }
        catch (error) { if (alive.current) setError(errorText(error)); }
        finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    const close = () => { setImporting(false); setTarget(null); setAuth(''); setAuthorization({ service: 'built_in', cdk: '' }); };
    const change = (account: NativeAccount, patch: { enabled?: boolean; auto_refresh?: boolean; auto_reauthorize?: boolean; refresh?: boolean }) => run(async () => {
        await request('update_native_account', { request: { provider, id: account.id, ...patch } });
    });
    const importAccount = () => run(async () => {
        const raw = auth.trim();
        if (!/^[{"\[]/.test(raw) && raw.split('\n')[0].includes('@') && raw.includes('----')) {
            if (target) {
                await request('save_native_login_credentials', { request: { provider, id: target.id, input: raw, auto_reauthorize: true, authorization } });
                await request('reauthorize_native_account', { request: { provider, id: target.id } });
                setLoginVersion(value => value + 1);
            } else {
                const report = await request<LoginImportReport>('import_native_login_accounts', { request: { provider, input: raw, auto_reauthorize: true, authorization } });
                showToast(t('nativeProviders.login.importQueued', { count: report.queued.length, skipped: report.skipped.length }), report.skipped.length ? 'warning' : 'success');
            }
            if (alive.current) close();
            return;
        }
        let parsed: unknown = auth.trim();
        // A bare token is sent as-is; anything that looks like JSON must parse.
        if (/^[{"]/.test(auth.trim())) { try { parsed = JSON.parse(auth); } catch { throw new Error(t('nativeProviders.invalidJson')); } }
        // A multi-account export (e.g. sub2api's {"accounts": [...]}) imports account by account.
        if (!target && Array.isArray((parsed as { accounts?: unknown })?.accounts)) {
            const report = await request<BatchItem[]>('import_native_accounts_batch', { request: { provider, auth: parsed, authorization } });
            const count = (status: BatchItem['status']) => report.filter(item => item.status === status).length;
            showToast(t('nativeProviders.chatgpt.batchResult', { imported: count('imported'), skipped: count('skipped'), failed: count('failed') }), count('failed') ? 'warning' : 'success');
            const failed = report.filter(item => item.status === 'failed');
            if (failed.length) throw new Error(failed.map(item => `${item.label}: ${item.message}`).join('\n'));
            if (alive.current) close();
            return;
        }
        await request('import_native_account', { request: { provider, auth: parsed, authorization, ...(target ? { account_id: target.id } : {}) } });
        if (alive.current) close();
    });
    const reimport = (account: NativeAccount) => { setTarget(account); setImporting(true); setAuth(''); setError(''); setAuthorization({ service: account.reauthorization_service || 'built_in', cdk: '' }); };
    const editLogins = (account: NativeAccount | null) => { setLoginTarget(account); setEditingLogins(true); setError(''); };
    const saveLogins = (input: string, auto: boolean, authorization: AuthorizationConfig) => void run(async () => {
        const count = await request<number>('save_native_login_credentials', { request: { provider, id: loginTarget?.id, input, auto_reauthorize: auto, authorization } });
        setLoginVersion(value => value + 1); setEditingLogins(false); setLoginTarget(null);
        showToast(t('nativeProviders.login.saved', { count }), 'success');
    });
    return <div className="min-h-0 flex-1 overflow-auto px-5 pb-6">
        <div className="mx-auto max-w-7xl space-y-5">
            <div className="flex flex-wrap items-center justify-between gap-3">
                <div><h1 className="text-xl font-semibold">{t('nativeProviders.title', { provider: label })}</h1><p className="mt-2 max-w-3xl text-sm text-gray-500">{t('nativeProviders.description')}</p></div>
                <div className="flex gap-2">
                    <button className={button} disabled={busy || !accounts.length} onClick={() => editLogins(null)}><KeyRound size={16} />{t('nativeProviders.login.title')}</button>
                    <button className={button} disabled={busy || loading} aria-label={t('nativeProviders.reload')} title={t('nativeProviders.reload')} onClick={() => void run(load)}><RefreshCw size={16} /></button>
                    <button className={button} disabled={busy} onClick={() => { setImporting(true); setTarget(null); setError(''); }}><Upload size={16} />{t('nativeProviders.import')}</button>
                </div>
            </div>
            <WorkBuddyEgressPanel view={egress} onChange={setEgress} api={egressApi} description={t('nativeProviders.egressDescription', { provider: label })} />
            {provider === 'chatgpt' && <ChatGptClientVersion onSaved={() => void load()} />}
            {editingLogins && <LoginCredentialsEditor key={loginTarget?.id || 'batch'} account={loginTarget} provider={provider} busy={busy} onSave={saveLogins} onClose={() => { setEditingLogins(false); setLoginTarget(null); }} />}
            {error && <div role="alert" className="flex items-start justify-between gap-3 rounded-lg border border-red-200 bg-red-50 p-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300"><span className="break-all">{error}</span><button aria-label={t('nativeProviders.dismiss')} onClick={() => setError('')}><X size={16} /></button></div>}
            {settings && !settings.enabled && <p role="status" className="rounded-lg border border-amber-200 bg-amber-50 p-3 text-sm text-amber-800 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-300">{t('nativeProviders.sourceDisabled')} <Link className="underline" to="/api-proxy?section=sources">{t('nativeProviders.sourceSettingsLink')}</Link></p>}
            {importing && <div className="space-y-4 rounded-xl border border-blue-200 p-4 dark:border-blue-900">
                <div className="flex items-center justify-between"><h2 className="font-medium">{target ? t('nativeProviders.chatgpt.reimportTitle') : t('nativeProviders.import')}</h2><button className={button} disabled={busy} onClick={close}>{t('nativeProviders.cancel')}</button></div>
                <p className="text-sm text-gray-500">{t('nativeProviders.chatgpt.importHint')}{target && <> {t('nativeProviders.chatgpt.reimportHint')}</>}</p>
                <label className="block space-y-2 text-sm"><span>{t('nativeProviders.credentials')}</span><textarea className={`${input} min-h-32 font-mono`} autoComplete="off" spellCheck={false} value={auth} onChange={e => setAuth(e.target.value)} placeholder={t('nativeProviders.chatgpt.importPlaceholder')} /></label>
                <AuthorizationServiceFields value={authorization} disabled={busy} onChange={setAuthorization} />
                <button className={button} disabled={busy || !auth.trim()} onClick={() => void importAccount()}>{t('nativeProviders.importSave')}</button>
            </div>}
            <LoginImportJobs jobs={loginImports} busy={busy} onRetry={id => void run(async () => { await request('retry_native_login_import', { request: { provider, id } }); })} onRemove={id => void run(async () => { await request('remove_native_login_import', { request: { provider, id } }); })} />
            {loading ? <div role="status" className="flex items-center justify-center gap-2 p-10 text-gray-500"><Loader2 size={18} className="animate-spin" />{t('nativeProviders.loading')}</div> : !accounts.length ? <div className="rounded-xl border border-dashed border-gray-300 p-10 text-center dark:border-gray-700"><h2 className="font-medium">{t('nativeProviders.empty')}</h2><p className="mt-2 text-sm text-gray-500">{t('nativeProviders.emptyHint', { provider: label })}</p></div> :
                <div className="overflow-x-auto rounded-xl border border-gray-200 dark:border-gray-700" role="region" aria-label={t('nativeProviders.title', { provider: label })} tabIndex={0}>
                    <table className="w-full text-left text-sm"><thead className="bg-gray-50 dark:bg-gray-800"><tr>{['account', 'authorizationService', 'egress', 'balance', 'health', 'authorization', 'maintenance', 'actions'].map(key => <th key={key} scope="col" className="whitespace-nowrap px-3 py-2.5 font-medium">{t(`nativeProviders.${key}`)}</th>)}</tr></thead>
                        <tbody>{visibleAccounts.map(account => {
                            const errors = [account.auth_error, account.quota_error, account.models_error, account.last_error].filter((value): value is string => !!value);
                            const status = nativeAccountStatus(account, now);
                            const name = account.nickname || account.uid;
                            return <tr key={account.id} className="border-t border-gray-100 align-top dark:border-gray-700">
                                <td className="min-w-64 max-w-72 px-3 py-2.5"><div className="truncate font-medium" title={name}>{name}</div><LoginCredentialsCell key={`${account.id}-${loginVersion}`} account={account} disabled={busy} onEdit={() => editLogins(account)} /><details className="mt-1 text-xs"><summary className="cursor-pointer text-blue-600">{t('nativeProviders.details', { count: account.models.length })}</summary><div className="mt-2 max-w-sm space-y-2 break-all text-gray-500"><div>{account.uid}</div><p>{t('nativeProviders.modelsUpdated', { time: date(account.models_updated_at) })}</p>{account.models.map(model => <div key={model.id}><code>{model.id}</code>{model.name && <span> · {model.name}</span>}</div>)}{errors.map((error, index) => <p key={index} className="text-amber-700 dark:text-amber-400">{error}</p>)}</div></details></td>
                                <td className="whitespace-nowrap px-3 py-2.5 text-xs text-gray-500">{account.reauthorization_service ? <AuthorizationServiceLabel service={account.reauthorization_service} /> : t('nativeProviders.login.notConfigured')}</td>
                                <td className="px-3 py-2.5"><WorkBuddyEgressCell accountId={account.id} view={egress} disabled={busy} onChange={setEgress} api={egressApi} /></td>
                                <td className="min-w-72 px-3 py-2.5"><WindowBars quota={account.quota} t={t} /><div className="mt-1 whitespace-nowrap text-xs text-gray-500">{account.quota ? date(account.quota.updated_at) : t('nativeProviders.notQueried')}</div>{account.quota_error && <p className="mt-1 text-xs text-amber-700 dark:text-amber-400">{t('nativeProviders.staleBalance')}</p>}</td>
                                <td className="px-3 py-2.5">{account.auth_error && <details className="mb-2 min-w-40 max-w-64 rounded-lg border border-red-200 bg-red-50 p-2 text-xs text-red-700 dark:border-red-900 dark:bg-red-950 dark:text-red-300"><summary className="cursor-pointer font-medium">{t(account.credential_dead ? 'nativeProviders.chatgpt.authError' : 'nativeProviders.login.temporary')}</summary><p className="mt-1 break-words">{account.auth_error}</p></details>}<span className={`whitespace-nowrap rounded-full px-2 py-1 text-xs ${status === 'ready' ? 'bg-green-50 text-green-700 dark:bg-green-950 dark:text-green-300' : 'bg-gray-100 text-gray-600 dark:bg-gray-800 dark:text-gray-300'}`}>{t(`nativeProviders.status_${status}`)}</span><div className="mt-1.5 whitespace-nowrap text-xs tabular-nums text-gray-500">{t('nativeProviders.requests', { success: account.successes, failed: account.failures })}</div>{account.cooldown_until > now && <div className="mt-1 whitespace-nowrap text-xs text-amber-700">{t('nativeProviders.cooldownSeconds', { seconds: Math.ceil(account.cooldown_until - now) })}</div>}</td>
                                <td className="px-3 py-2.5"><div className="whitespace-nowrap">{date(account.expires_at)}</div>{account.credential_dead && <p className="mt-1 max-w-56 text-xs text-red-600">{t('nativeProviders.login.revoked')}</p>}{!account.credential_dead && <p className="mt-1 whitespace-nowrap text-xs text-gray-500">{t(account.can_refresh ? 'nativeProviders.refreshable' : 'nativeProviders.noRefresh')}</p>}<div className="mt-2"><button className={action} disabled={busy || !account.has_login_credentials || account.reauthorizing} onClick={() => void run(async () => { await request('reauthorize_native_account', { request: { provider, id: account.id } }); })}><KeyRound size={13} />{t('nativeProviders.login.reauthorize')}</button></div></td>
                                <td className="space-y-1 px-3 py-2.5"><label className="flex items-center gap-2 whitespace-nowrap"><input type="checkbox" className="accent-blue-600" disabled={busy || !account.has_login_credentials} checked={account.auto_reauthorize} onChange={e => void change(account, { auto_reauthorize: e.target.checked })} />{t('nativeProviders.login.auto')}</label>{account.reauthorizing && <p role="status" className="text-xs text-blue-600">{t('nativeProviders.login.running')}</p>}{account.reauthorization_error && <p className="max-w-56 break-words text-xs text-amber-700">{account.reauthorization_error}</p>}<label className="flex items-center gap-2 whitespace-nowrap"><input type="checkbox" className="accent-blue-600" disabled={busy} checked={account.enabled} onChange={e => void change(account, { enabled: e.target.checked })} />{t('nativeProviders.enableAccount')}</label><label className="flex items-center gap-2 whitespace-nowrap"><input type="checkbox" className="accent-blue-600" disabled={busy} checked={account.auto_refresh} onChange={e => void change(account, { auto_refresh: e.target.checked })} />{t('nativeProviders.autoRefresh')}</label><p className="whitespace-nowrap text-xs text-gray-500" title={account.next_sync_at > 0 ? date(account.next_sync_at) : t('nativeProviders.syncIdle')}>{account.next_sync_at > 0 ? t('nativeProviders.nextSync', { time: shortDate(account.next_sync_at) }) : t('nativeProviders.syncIdleShort')}</p></td>
                                <td className="px-3 py-2.5"><div className="flex flex-nowrap items-center gap-2"><button className={action} disabled={busy} onClick={() => void change(account, {})}><RefreshCw size={13} />{t('nativeProviders.sync')}</button><button className={action} disabled={busy || !account.can_refresh || account.credential_dead} onClick={() => void change(account, { refresh: true })}><KeyRound size={13} />{t('nativeProviders.renew')}</button><button className={action} disabled={busy} onClick={() => reimport(account)}><Upload size={13} />{t('nativeProviders.chatgpt.reimport')}</button><button className={`${action} text-red-600`} disabled={busy} onClick={() => setDeleting(account)}><Trash2 size={13} />{t('nativeProviders.delete')}</button></div></td>
                            </tr>;
                        })}</tbody>
                    </table>
                </div>}
            {!loading && <Pagination currentPage={currentPage} totalPages={totalPages} onPageChange={setPage} totalItems={accounts.length} itemsPerPage={PAGE_SIZE} />}
            <p className="text-xs leading-relaxed text-gray-500">{t('nativeProviders.routingHint')} <Link className="text-blue-600 underline" to="/api-proxy">{t('nativeProviders.routingLink')}</Link></p>
            <ModalDialog isOpen={!!deleting} title={t('nativeProviders.deleteTitle')} message={t('nativeProviders.deleteHint', { name: deleting?.nickname || deleting?.uid })} type="confirm" isDestructive isLoading={busy} onConfirm={() => void run(async () => { if (!deleting) return; await request('delete_native_account', { request: { provider, id: deleting.id } }); if (alive.current) setDeleting(null); })} onCancel={() => setDeleting(null)} />
        </div>
    </div>;
}
