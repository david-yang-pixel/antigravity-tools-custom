import { useEffect, useId, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { Loader2, RefreshCw, Save } from 'lucide-react';
import { request } from '../../../utils/request';
import { registryError } from './registryLogic';

interface Settings {
    enabled: boolean;
    preferred_account_id: string | null;
    account_count: number;
    eligible_account_count: number;
    env_locked: string[];
}
interface Account { id: string; nickname: string; uid: string; enabled: boolean }

// Provider connection controls only. Never reads/writes legacy model routes or
// starts the old unbounded provider verification/publication workflow.
export default function SourceSettings() {
    const [settings, setSettings] = useState<Settings | null>(null);
    const [accounts, setAccounts] = useState<Account[]>([]);
    const [enabled, setEnabled] = useState(false);
    const [preferred, setPreferred] = useState('');
    const [busy, setBusy] = useState(true);
    const [dirty, setDirty] = useState(false);
    const [feedback, setFeedback] = useState<{ error?: boolean; message: string } | null>(null);
    const locked = useRef(false);
    const alive = useRef(true);
    const id = useId();

    async function load() {
        if (locked.current) return;
        locked.current = true; setBusy(true); setFeedback(null);
        try {
            const [value, list] = await Promise.all([
                request<Settings>('get_workbuddy_settings'),
                request<Account[]>('list_workbuddy_accounts'),
            ]);
            if (!alive.current) return;
            setSettings(value); setAccounts(list); setEnabled(value.enabled);
            setPreferred(value.preferred_account_id ?? ''); setDirty(false);
        } catch (error) {
            if (alive.current) setFeedback({ error: true, message: registryError(error).message });
        } finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    useEffect(() => {
        alive.current = true; void load();
        return () => { alive.current = false; };
    }, []);

    async function save() {
        if (!settings || locked.current) return;
        locked.current = true; setBusy(true); setFeedback(null);
        try {
            const update = {
                ...(!settings.env_locked.includes('enabled') ? { enabled } : {}),
                ...(!settings.env_locked.includes('preferred_account_id') ? { preferred_account_id: preferred || null } : {}),
            };
            const next = await request<Settings>('save_workbuddy_settings', { request: update });
            window.dispatchEvent(new Event('model-registry-changed'));
            if (!alive.current) return;
            setSettings(next); setEnabled(next.enabled); setPreferred(next.preferred_account_id ?? ''); setDirty(false);
            setFeedback({ message: '来源配置已保存；未修改任何上架绑定。 / Source settings saved; publications unchanged.' });
        } catch (error) {
            if (alive.current) setFeedback({ error: true, message: `保存失败，草稿保留。 / Save failed; draft retained. ${registryError(error).message}` });
        } finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    const fieldLocked = (field: string) => busy || !!settings?.env_locked.includes(field);
    return <section className="space-y-3" aria-label="来源连接配置 / Source connections" aria-busy={busy}>
        <div className="flex flex-wrap items-center justify-between gap-2">
            <h3 className="text-sm font-semibold">来源连接配置 / Source connections</h3>
            <button type="button" className="registry-btn" disabled={busy || dirty} onClick={() => void load()}><RefreshCw size={14} aria-hidden />重新读取 / Reload</button>
        </div>
        <p className="registry-muted">连接与账号在来源账号页管理；上架、别名和验证统一在目录中操作。 / Manage account connections here; publish and verify only through the registry.</p>
        <div className="flex flex-wrap gap-3 text-xs">
            <Link className="registry-link" to="/accounts">Antigravity · 连接与账号 / Accounts</Link>
            <Link className="registry-link" to="/accounts?provider=workbuddy">WorkBuddy · 连接与账号 / Accounts</Link>
        </div>
        <div className="rounded-lg border border-gray-200 p-3 space-y-3 dark:border-base-300">
            <h4 className="text-sm font-medium">WorkBuddy</h4>
            {settings && <>
                <p className="registry-muted">{settings.account_count} 账号 / accounts · {settings.eligible_account_count} 符合来源选择条件 / eligible (not proof of availability)</p>
                <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={enabled} disabled={fieldLocked('enabled')} onChange={e => { setEnabled(e.target.checked); setDirty(true); setFeedback(null); }} />启用此来源 / Enable source</label>
                <label className="block text-xs font-medium" htmlFor={`${id}-preferred`}>首选账号（仅 WB） / Preferred account (WB only)</label>
                <select id={`${id}-preferred`} className="registry-input" value={preferred} disabled={fieldLocked('preferred_account_id')} onChange={e => { setPreferred(e.target.value); setDirty(true); setFeedback(null); }}>
                    <option value="">自动选择 / Automatic</option>
                    {preferred && !accounts.some(a => a.id === preferred) && <option value={preferred} disabled>当前账号不可用 / Current account unavailable</option>}
                    {accounts.map(account => <option key={account.id} value={account.id} disabled={!account.enabled}>{account.nickname || account.uid}{account.enabled ? '' : ' · 已停用 / Disabled'}</option>)}
                </select>
                <p className="registry-muted">与 AG 固定账号独立；不执行连通性探测或推理。停用来源不会删除上架记录。 / Independent from AG pinning; no inference or connectivity probe. Disabling retains publications.</p>
                {!!settings.env_locked.length && <p className="registry-muted break-words">环境变量锁定 / Environment overrides: {settings.env_locked.join(', ')}</p>}
                <button type="button" className="registry-btn registry-primary" disabled={busy || !dirty} onClick={() => void save()}>{busy ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <Save size={14} aria-hidden />}保存来源配置 / Save source</button>
            </>}
            {!settings && busy && <p className="registry-muted" role="status">正在读取来源配置 / Loading source…</p>}
        </div>
        {dirty && <p role="status" className="registry-muted">来源配置草稿尚未保存。 / Unsaved source settings.</p>}
        {feedback && <p role={feedback.error ? 'alert' : 'status'} className={`registry-notice ${feedback.error ? 'registry-notice-error' : ''}`}>{feedback.message}</p>}
    </section>;
}
