import { useCallback, useEffect, useRef, useState } from 'react';
import { ExternalLink, Plus, RefreshCw, Upload, X } from 'lucide-react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import { isTauri } from '../../utils/env';
import ModalDialog from '../common/ModalDialog';
import Pagination from '../common/Pagination';
import { showToast } from '../common/ToastContainer';

import WorkBuddyAccountRow, { type WorkBuddyAccount as Account, type AccountChanges } from './WorkBuddyAccountRow';
import WorkBuddyEgressPanel, { WorkBuddyEgressCell, type EgressView } from './WorkBuddyEgress';

interface Login { id: string; auth_url: string; expires_at: number }
const PAGE_SIZE = 10;
const button = 'inline-flex items-center justify-center gap-2 rounded-lg border border-gray-200 dark:border-gray-700 px-3 py-2 text-sm hover:bg-gray-50 dark:hover:bg-gray-800 disabled:opacity-50 disabled:cursor-not-allowed';
const input = 'w-full rounded-lg border border-gray-200 dark:border-gray-700 bg-white dark:bg-gray-900 px-3 py-2 text-sm outline-none focus:border-blue-500';

export default function WorkBuddyAccounts() {
    const { t } = useTranslation();
    const [accounts, setAccounts] = useState<Account[]>([]);
    const [page, setPage] = useState(1);
    const totalPages = Math.max(1, Math.ceil(accounts.length / PAGE_SIZE));
    const currentPage = Math.min(page, totalPages);
    const visibleAccounts = accounts.slice((currentPage - 1) * PAGE_SIZE, currentPage * PAGE_SIZE);
    useEffect(() => { setPage(current => Math.min(current, totalPages)); }, [totalPages]);
    const [loading, setLoading] = useState(true);
    const [error, setError] = useState('');
    const [busy, setBusy] = useState('');
    const [mode, setMode] = useState<'login' | 'import' | null>(null);
    const [realm, setRealm] = useState<'cn' | 'global'>('cn');
    const [auth, setAuth] = useState('');
    const [login, setLogin] = useState<Login | null>(null);
    const [now, setNow] = useState(Date.now());
    const [deleting, setDeleting] = useState<Account | null>(null);
    const [egress, setEgress] = useState<EgressView | null>(null);
    const mounted = useRef(true);
    const translate = useRef(t);
    translate.current = t;
    const load = useCallback(async () => {
        try { const values = await request<Account[]>('list_workbuddy_accounts'); if (mounted.current) setAccounts(values); }
        catch (e) { if (mounted.current) setError(String(e)); }
        finally { if (mounted.current) setLoading(false); }
    }, []);
    const loadEgress = useCallback(async () => {
        try { const value = await request<EgressView>('get_workbuddy_egress'); if (mounted.current) setEgress(value); }
        catch (e) { if (mounted.current) setError(String(e)); }
    }, []);
    useEffect(() => {
        mounted.current = true; void load(); void loadEgress();
        return () => { mounted.current = false; };
    }, [load, loadEgress]);
    useEffect(() => {
        const clock = setInterval(() => setNow(Date.now()), 1000);
        const refresh = setInterval(() => { if (document.visibilityState === 'visible') void load(); }, 15000);
        return () => { clearInterval(clock); clearInterval(refresh); };
    }, [load]);
    useEffect(() => {
        if (!login) return;
        let cancelled = false; let timer: ReturnType<typeof setTimeout>;
        const poll = async () => {
            if (cancelled) return;
            if (Date.now() >= login.expires_at * 1000) { setError(translate.current('workbuddy.accounts.login_expired')); setLogin(null); return; }
            try {
                const result = await request<{ status: string; account: Account | null }>('poll_workbuddy_login', { request: { id: login.id } });
                if (cancelled) return;
                if (result.status === 'ready' && result.account) {
                    setLogin(null); setMode(null); setError(''); setBusy(result.account.id);
                    showToast(translate.current('workbuddy.accounts.authorized'), 'success');
                    await load();
                    try {
                        const synced = await request<Account>('update_workbuddy_account', { request: { id: result.account.id } });
                        if (mounted.current && synced.last_error) setError(synced.last_error);
                    } catch (e) { if (mounted.current) setError(String(e)); }
                    finally { if (mounted.current) { setBusy(''); void load(); } }
                    return;
                }
                if (result.status === 'expired') { setError(translate.current('workbuddy.accounts.login_expired')); setLogin(null); return; }
                setError('');
            } catch (e) { if (!cancelled) setError(String(e)); }
            if (!cancelled) timer = setTimeout(poll, 3000);
        };
        timer = setTimeout(poll, 3000);
        return () => {
            cancelled = true; clearTimeout(timer);
            void request('cancel_workbuddy_login', { request: { id: login.id } }).catch(() => undefined);
        };
    }, [login, load]);
    async function start() {
        setBusy('login'); setError('');
        try {
            const session = await request<Login>('start_workbuddy_login', { request: { realm } });
            if (mounted.current) { setNow(Date.now()); setLogin(session); }
            else await request('cancel_workbuddy_login', { request: { id: session.id } });
        } catch (e) { if (mounted.current) setError(String(e)); }
        finally { if (mounted.current) setBusy(''); }
    }
    async function importAccount() {
        let value: unknown;
        try { value = JSON.parse(auth); } catch { setError(t('workbuddy.accounts.invalid_json')); return; }
        setBusy('import'); setError('');
        try {
            const account = await request<Account>('import_workbuddy_account', { request: { auth: value } });
            setAuth(''); setMode(null); if (account.last_error) setError(account.last_error);
            showToast(t('workbuddy.accounts.imported'), 'success'); await load();
        } catch (e) { setError(String(e)); }
        finally { setBusy(''); }
    }
    async function action(account: Account, changes: AccountChanges) {
        setBusy(account.id); setError('');
        try {
            const updated = await request<Account>('update_workbuddy_account', { request: { id: account.id, ...changes } });
            if (updated.last_error) setError(updated.last_error); await load();
        } catch (e) { setError(String(e)); }
        finally { setBusy(''); }
    }
    async function remove() {
        if (!deleting) return; setBusy(deleting.id); setError('');
        try { await request('delete_workbuddy_account', { request: { id: deleting.id } }); setDeleting(null); await load(); void loadEgress(); }
        catch (e) { setError(String(e)); } finally { setBusy(''); }
    }
    const remaining = login ? Math.max(0, Math.ceil((login.expires_at * 1000 - now) / 1000)) : 0;
    return <section className="mx-auto w-full max-w-7xl flex-1 min-h-0 overflow-y-auto space-y-5 px-5 pb-8 text-gray-900 dark:text-gray-100">
        <div className="flex flex-wrap items-start justify-between gap-4">
            <div><h1 className="text-xl font-semibold">{t('workbuddy.accounts.title')}</h1>
                <p className="mt-2 max-w-2xl text-sm leading-relaxed text-gray-500 dark:text-gray-400">{t('workbuddy.accounts.description')}</p></div>
            <div className="flex flex-wrap gap-2">
                <button className={button} disabled={!!busy} onClick={() => { setError(''); void load(); void loadEgress(); }} aria-label={t('common.refresh')}><RefreshCw size={16} /></button>
                <button className={button} disabled={!!busy || !!login} onClick={() => { setMode('import'); setError(''); }}><Upload size={16} />{t('workbuddy.accounts.import')}</button>
                <button className={`${button} !border-blue-600 !bg-blue-600 text-white hover:!bg-blue-700`} disabled={!!busy || !!login} onClick={() => { setMode('login'); setError(''); }}><Plus size={16} />{t('workbuddy.accounts.add')}</button>
            </div>
        </div>
        <WorkBuddyEgressPanel view={egress} onChange={setEgress} />
        {error && <div role="alert" className="rounded-xl border border-red-200 bg-red-50 p-3 text-sm text-red-700 dark:border-red-900 dark:bg-red-950/30 dark:text-red-300">{error}</div>}
        {mode && <div className="rounded-2xl border border-blue-200 bg-white p-5 dark:border-blue-900 dark:bg-gray-900">
            <div className="mb-4 flex items-center justify-between"><h2 className="font-medium">{t(mode === 'login' ? 'workbuddy.accounts.add' : 'workbuddy.accounts.import')}</h2>
                <button className={button} disabled={!!busy} aria-label={t('common.close')} onClick={() => { setLogin(null); setMode(null); setAuth(''); }}><X size={16} /></button></div>
            {mode === 'login' ? <div className="space-y-4">
                <p className="text-sm text-gray-500">{t('workbuddy.accounts.login_hint')}</p>
                {!login ? <div className="flex flex-wrap items-end gap-3">
                    <label className="space-y-1 text-sm"><span>{t('workbuddy.accounts.region')}</span><select className={input} value={realm} disabled={!!busy} onChange={e => setRealm(e.target.value as 'cn' | 'global')}><option value="cn">{t('workbuddy.accounts.cn')}</option><option value="global">{t('workbuddy.accounts.global')}</option></select></label>
                    <button className={button} disabled={!!busy} onClick={start}>{busy === 'login' ? t('common.loading') : t('workbuddy.accounts.get_link')}</button>
                </div> : <div className="flex flex-wrap items-center gap-4">
                    <a href={login.auth_url} onClick={async e => {
                        if (isTauri()) { e.preventDefault(); try { const { openUrl } = await import('@tauri-apps/plugin-opener'); await openUrl(login.auth_url); } catch (error) { setError(String(error)); } }
                    }} target="_blank" rel="noopener noreferrer" className={`${button} !bg-blue-600 text-white`}><ExternalLink size={16} />{t('workbuddy.accounts.open_login')}</a>
                    <span role="status" className="text-sm text-gray-500">{t('workbuddy.accounts.waiting', { time: `${Math.floor(remaining / 60)}:${String(remaining % 60).padStart(2, '0')}` })}</span>
                    <button className={button} onClick={() => setLogin(null)}>{t('common.cancel')}</button>
                </div>}
            </div> : <div className="space-y-3">
                <label className="block space-y-2 text-sm"><span>{t('workbuddy.accounts.import_hint')}</span><textarea className={`${input} min-h-40 font-mono text-xs`} value={auth} autoComplete="off" spellCheck={false} disabled={!!busy} onChange={e => setAuth(e.target.value)} placeholder={'{"account":{"uid":"…"},"auth":{"accessToken":"…","refreshToken":"…","realm":"cn"}}'} /></label>
                <button className={button} disabled={!!busy || !auth.trim()} onClick={importAccount}>{busy === 'import' ? t('common.loading') : t('workbuddy.accounts.import_save')}</button>
            </div>}
        </div>}
        {loading ? <p className="py-12 text-center text-sm text-gray-500">{t('common.loading')}</p> : accounts.length === 0 ?
            <div className="rounded-2xl border border-dashed border-gray-300 p-12 text-center dark:border-gray-700"><h2 className="font-medium">{t('workbuddy.accounts.empty')}</h2><p className="mt-2 text-sm text-gray-500">{t('workbuddy.accounts.empty_hint')}</p></div> :
            <div className="overflow-x-auto rounded-xl border border-gray-200 dark:border-gray-800" role="region" aria-label={t('workbuddy.accounts.title')} tabIndex={0}>
                <table className="w-full min-w-[1320px] text-left">
                    <thead className="border-b border-gray-200 bg-gray-50 text-xs font-medium text-gray-500 dark:border-gray-800 dark:bg-gray-800 dark:text-gray-400"><tr>
                        <th scope="col" className="whitespace-nowrap px-4 py-3 font-medium">{t('workbuddy.accounts.account_column')}</th>
                        <th scope="col" className="whitespace-nowrap px-4 py-3 font-medium">{t('workbuddy.egress.column')}</th>
                        {['credits', 'health', 'expires', 'maintenance_column', 'automation_column'].map(key => <th key={key} scope="col" className="whitespace-nowrap px-4 py-3 font-medium">{t(`workbuddy.accounts.${key}`)}</th>)}
                        <th scope="col" className="sticky right-0 bg-gray-50 px-4 py-3 text-right font-medium shadow-[-12px_0_12px_-12px_rgba(0,0,0,0.12)] dark:bg-gray-800">{t('common.actions')}</th>
                    </tr></thead>
                    <tbody>{visibleAccounts.map(account => <WorkBuddyAccountRow key={account.id} account={account} now={now} busy={!!busy} onChange={changes => void action(account, changes)} onDelete={() => setDeleting(account)}
                        egress={<WorkBuddyEgressCell accountId={account.id} view={egress} disabled={!!busy} onChange={setEgress} />} />)}</tbody>
                </table>
            </div>}
        {!loading && <Pagination currentPage={currentPage} totalPages={totalPages} onPageChange={setPage} totalItems={accounts.length} itemsPerPage={PAGE_SIZE} />}
        {accounts.length > 0 && <p className="text-xs leading-relaxed text-gray-500">{t('workbuddy.accounts.managed_hint')}</p>}
        <p className="text-xs leading-relaxed text-gray-500">{t('workbuddy.accounts.routing_hint')} <Link to="/api-proxy?section=workbuddy" className="text-blue-600 underline">{t('workbuddy.accounts.routing_link')}</Link></p>
        <ModalDialog isOpen={!!deleting} title={t('workbuddy.accounts.delete_title')} message={t('workbuddy.accounts.delete_hint', { name: deleting?.nickname || deleting?.uid })} type="confirm" isDestructive isLoading={!!busy} onConfirm={remove} onCancel={() => setDeleting(null)} />
    </section>;
}
