import { useEffect, useId, useRef, useState } from 'react';
import { KeyRound, Loader2, Plus, Save, Trash2 } from 'lucide-react';
import { request } from '../../../utils/request';
import type { CustomUpstream, CustomUpstreamSettings as Settings, CustomUpstreamUpdate } from '../../../types/modelRegistry';
import { registryError } from './registryLogic';

interface Draft {
    key: string;
    id: string;
    isNew: boolean;
    name: string;
    base_url: string;
    enabled: boolean;
    models: string;
    api_key: string;
    clear_key: boolean;
    saved?: CustomUpstream;
}

const toDraft = (u: CustomUpstream): Draft => ({ key: u.id, id: u.id, isNew: false, name: u.name, base_url: u.base_url, enabled: u.enabled, models: u.models.join('\n'), api_key: '', clear_key: false, saved: u });
const splitModels = (value: string) => value.split(/[\s,]+/).map(m => m.trim()).filter(Boolean);
// IDs are permanent registry source identities; derive one from the first name and never change it.
function newId(name: string, taken: Set<string>) {
    const base = name.toLowerCase().normalize('NFKD').replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 24) || 'upstream';
    let id = base;
    for (let n = 2; taken.has(id); n++) id = `${base}-${n}`;
    taken.add(id);
    return id;
}

// OpenAI-compatible upstreams (base URL + API key). Connection settings only: models are
// fetched, tested and published through the registry like every other source.
export default function CustomUpstreamSettings() {
    const [settings, setSettings] = useState<Settings | null>(null);
    const [drafts, setDrafts] = useState<Draft[]>([]);
    const [busy, setBusy] = useState(true);
    const [dirty, setDirty] = useState(false);
    const [feedback, setFeedback] = useState<{ error?: boolean; message: string } | null>(null);
    const locked = useRef(false);
    const alive = useRef(true);
    const counter = useRef(0);
    const uid = useId();

    function apply(value: Settings) {
        setSettings(value); setDrafts(value.upstreams.map(toDraft)); setDirty(false);
    }
    async function load() {
        if (locked.current) return;
        locked.current = true; setBusy(true); setFeedback(null);
        try {
            const value = await request<Settings>('get_custom_upstreams');
            if (alive.current) apply(value);
        } catch (error) {
            if (alive.current) setFeedback({ error: true, message: registryError(error).message });
        } finally { locked.current = false; if (alive.current) setBusy(false); }
    }
    useEffect(() => {
        alive.current = true; void load();
        return () => { alive.current = false; };
    }, []);

    const edit = (key: string, patch: Partial<Draft>) => {
        setDrafts(list => list.map(d => d.key === key ? { ...d, ...patch } : d)); setDirty(true); setFeedback(null);
    };
    const add = () => {
        counter.current += 1;
        setDrafts(list => [...list, { key: `new-${counter.current}`, id: '', isNew: true, name: '', base_url: '', enabled: true, models: '', api_key: '', clear_key: false }]);
        setDirty(true); setFeedback(null);
    };
    const remove = (key: string) => { setDrafts(list => list.filter(d => d.key !== key)); setDirty(true); setFeedback(null); };

    async function save() {
        if (locked.current) return;
        const missing = drafts.find(d => !d.base_url.trim());
        if (missing) { setFeedback({ error: true, message: `请填写 Base URL。 / Enter a base URL for ${missing.name || 'the new upstream'}.` }); return; }
        locked.current = true; setBusy(true); setFeedback(null);
        try {
            const taken = new Set(drafts.filter(d => !d.isNew).map(d => d.id));
            const upstreams: CustomUpstreamUpdate[] = drafts.map(d => ({
                id: d.isNew ? newId(d.name, taken) : d.id,
                name: d.name.trim(),
                base_url: d.base_url.trim(),
                enabled: d.enabled,
                models: splitModels(d.models),
                ...(d.clear_key ? { api_key: '' } : d.api_key.trim() ? { api_key: d.api_key.trim() } : {}),
            }));
            const next = await request<Settings>('save_custom_upstreams', { request: { upstreams } });
            window.dispatchEvent(new Event('model-registry-changed'));
            if (!alive.current) return;
            apply(next);
            setFeedback({ message: '上游已保存。到「获取上游模型」中选择它来拉取、测试和上架模型。 / Saved. Use “Fetch upstream models” to list, test and publish its models.' });
        } catch (error) {
            if (alive.current) setFeedback({ error: true, message: `保存失败，草稿保留。 / Save failed; draft retained. ${registryError(error).message}` });
        } finally { locked.current = false; if (alive.current) setBusy(false); }
    }

    const readOnly = busy || !!settings?.env_locked;
    return <div className="rounded-lg border border-gray-200 p-3 space-y-3 dark:border-base-300" aria-busy={busy}>
        <div className="flex flex-wrap items-center justify-between gap-2">
            <h4 className="text-sm font-medium">OpenAI 兼容上游 / OpenAI-compatible upstreams</h4>
            <button type="button" className="registry-btn" disabled={readOnly} onClick={add}><Plus size={14} aria-hidden />添加上游 / Add upstream</button>
        </div>
        <p className="registry-muted">填写 Base URL（如 <code>https://api.example.com/v1</code>）和 API Key，请求发往 <code>/chat/completions</code>。每个上游是独立来源，可服务 OpenAI 与 Claude 接口。 / Base URL plus API key; requests go to <code>/chat/completions</code>. Each upstream is its own source for the OpenAI and Claude endpoints.</p>
        {settings?.env_locked && <p className="registry-notice registry-notice-warning">由环境变量 ABV_CUSTOM_UPSTREAMS 管理，此处只读。 / Managed by ABV_CUSTOM_UPSTREAMS; read-only here.</p>}
        {!settings && busy && <p className="registry-muted" role="status">正在读取 / Loading…</p>}
        {settings && !drafts.length && <p className="registry-muted">尚未添加。 / None configured.</p>}
        {drafts.map(d => {
            const field = `${uid}-${d.key}`;
            return <fieldset key={d.key} disabled={readOnly} className="min-w-0 space-y-2 rounded-md border border-gray-100 p-3 dark:border-base-200">
                <div className="flex flex-wrap items-center justify-between gap-2">
                    <span className="text-xs font-semibold break-all">{d.isNew ? '新上游 / New upstream' : <>{d.saved?.name || d.id} <code className="registry-muted">{d.saved?.source_id}</code></>}</span>
                    <button type="button" className="registry-link registry-danger" onClick={() => remove(d.key)}><Trash2 size={14} aria-hidden />移除 / Remove</button>
                </div>
                <div className="grid gap-2 sm:grid-cols-2">
                    <label className="block text-xs font-medium" htmlFor={`${field}-name`}>名称 / Name
                        <input id={`${field}-name`} className="registry-input mt-1" maxLength={64} value={d.name} placeholder="My relay" onChange={e => edit(d.key, { name: e.target.value })} /></label>
                    <label className="block text-xs font-medium" htmlFor={`${field}-url`}>Base URL
                        <input id={`${field}-url`} className="registry-input mt-1" inputMode="url" autoComplete="off" spellCheck={false} value={d.base_url} placeholder="https://api.example.com/v1" onChange={e => edit(d.key, { base_url: e.target.value })} /></label>
                </div>
                <label className="block text-xs font-medium" htmlFor={`${field}-key`}><KeyRound size={12} className="inline mr-1" aria-hidden />API Key
                    <input id={`${field}-key`} type="password" className="registry-input mt-1" autoComplete="new-password" spellCheck={false} value={d.api_key} disabled={d.clear_key}
                        placeholder={d.saved?.api_key_set ? `已保存 ${d.saved.api_key_hint} · 留空保持不变 / Saved — leave blank to keep` : '可选；本地服务可留空 / Optional for keyless local servers'}
                        onChange={e => edit(d.key, { api_key: e.target.value })} /></label>
                {d.saved?.api_key_set && <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={d.clear_key} onChange={e => edit(d.key, { clear_key: e.target.checked, api_key: '' })} />清除已保存的密钥 / Clear saved key</label>}
                <label className="block text-xs font-medium" htmlFor={`${field}-models`}>手动声明模型（可选） / Declared models (optional)
                    <textarea id={`${field}-models`} className="registry-input mt-1 min-h-[3.5rem] font-mono" spellCheck={false} value={d.models} placeholder="gpt-4o-mini, deepseek-chat" onChange={e => edit(d.key, { models: e.target.value })} /></label>
                <p className="registry-muted">上游没有 /models 列表时在此填写模型 ID（逗号或换行分隔）。{d.saved ? ` 已获取 ${d.saved.catalog_count} 个模型。 / ${d.saved.catalog_count} fetched.` : ''} / For upstreams without a /models list.</p>
                <label className="flex items-center gap-2 text-xs"><input type="checkbox" checked={d.enabled} onChange={e => edit(d.key, { enabled: e.target.checked })} />启用此来源 / Enable source</label>
            </fieldset>;
        })}
        {settings && !settings.env_locked && <button type="button" className="registry-btn registry-primary" disabled={busy || !dirty} onClick={() => void save()}>{busy ? <Loader2 size={14} className="motion-safe:animate-spin" aria-hidden /> : <Save size={14} aria-hidden />}保存上游 / Save upstreams</button>}
        {dirty && <p role="status" className="registry-muted">上游草稿尚未保存。 / Unsaved upstream changes.</p>}
        {feedback && <p role={feedback.error ? 'alert' : 'status'} className={`registry-notice ${feedback.error ? 'registry-notice-error' : ''}`}>{feedback.message}</p>}
    </div>;
}
