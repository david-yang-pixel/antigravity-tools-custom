import { useEffect, useId, useRef, useState } from 'react';
import { Loader2, Save } from 'lucide-react';
import { request } from '../../../utils/request';
import { registryError } from './registryLogic';
import type { NativeAccount, NativeProvider, NativeSettings } from '../../../types/nativeProviders';

const labels: Record<NativeProvider, string> = { qoder: 'Qoder', chatgpt: 'ChatGPT' };

// Native provider (Qoder, ChatGPT) source switch and fixed account, next to WorkBuddy's.
// Automatic selection follows the global scheduling mode configured on this page.
export default function NativeSourceSettings({ provider }: { provider: NativeProvider }) {
    const label = labels[provider];
    const [settings, setSettings] = useState<NativeSettings | null>(null);
    const [accounts, setAccounts] = useState<NativeAccount[]>([]);
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
                request<NativeSettings>('get_native_provider_settings', { request: { provider } }),
                request<NativeAccount[]>('list_native_accounts', { request: { provider } }),
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
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [provider]);

    async function save() {
        if (!settings || locked.current) return;
        locked.current = true; setBusy(true); setFeedback(null);
        try {
            const next = await request<NativeSettings>('save_native_provider_settings', { request: { provider, enabled, preferred_account_id: preferred || null } });
            window.dispatchEvent(new Event('model-registry-changed'));
            if (!alive.current) return;
            setSettings(next); setEnabled(next.enabled); setPreferred(next.preferred_account_id ?? ''); setDirty(false);
            setFeedback({ message: '来源配置已保存；未修改任何上架绑定。 / Source settings saved; publications unchanged.' });
        } catch (error) {
            if (alive.current) setFeedback({ error: true, message: `保存失败，草稿保留。 / Save failed; draft retained. ${registryError(error).message}` });
        } finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    const fieldLocked = busy || !!settings?.env_locked;
    return <div className="rounded-lg border border-gray-200 p-3 space-y-3 dark:border-base-300">
        <h4 className="text-sm font-medium">{label}</h4>
        {settings && <>
            <p className="registry-muted">{accounts.length} 账号 / accounts · {accounts.filter(a => a.enabled).length} 参与轮换 / in rotation</p>
            <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={enabled} disabled={fieldLocked} onChange={e => { setEnabled(e.target.checked); setDirty(true); setFeedback(null); }} />启用此来源 / Enable source</label>
            <label className="block text-xs font-medium" htmlFor={`${id}-preferred`}>固定账号（仅 {label}） / Fixed account ({label} only)</label>
            <select id={`${id}-preferred`} className="registry-input" value={preferred} disabled={fieldLocked} onChange={e => { setPreferred(e.target.value); setDirty(true); setFeedback(null); }}>
                <option value="">自动选择 / Automatic</option>
                {preferred && !accounts.some(a => a.id === preferred) && <option value={preferred} disabled>当前账号不可用 / Current account unavailable</option>}
                {accounts.map(account => <option key={account.id} value={account.id} disabled={!account.enabled}>{account.nickname || account.uid}{account.enabled ? '' : ' · 已停用 / Disabled'}</option>)}
            </select>
            <p className="registry-muted">自动选择遵循上方调度模式；固定后只使用该账号，与 AG、WB 固定账号独立。停用来源不会删除上架记录。 / Automatic follows the scheduling mode above; a fixed account is used exclusively and is independent from AG/WB pinning. Disabling retains publications.</p>
            {settings.env_locked && <p className="registry-muted break-words">环境变量锁定 / Environment override: ABV_{provider.toUpperCase()}_ENABLED</p>}
            <button type="button" className="registry-btn registry-primary" disabled={busy || !dirty} onClick={() => void save()}>{busy ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <Save size={14} aria-hidden />}保存来源配置 / Save source</button>
        </>}
        {!settings && busy && <p className="registry-muted" role="status">正在读取来源配置 / Loading source…</p>}
        {dirty && <p role="status" className="registry-muted">来源配置草稿尚未保存。 / Unsaved source settings.</p>}
        {feedback && <p role={feedback.error ? 'alert' : 'status'} className={`registry-notice ${feedback.error ? 'registry-notice-error' : ''}`}>{feedback.message}</p>}
    </div>;
}
