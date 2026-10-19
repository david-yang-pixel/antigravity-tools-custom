import { useState } from 'react';
import { Copy, Eye, EyeOff, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';

interface Props {
    token: string;
    onRegenerate: () => Promise<boolean>;
    embedded?: boolean;
}

/** The signed-in account's single API key: reveal, copy and regenerate. */
export default function PortalApiKeyPanel({ token, onRegenerate, embedded = false }: Props) {
    const { t } = useTranslation();
    const [visible, setVisible] = useState(false);
    const [confirming, setConfirming] = useState(false);
    const [working, setWorking] = useState(false);
    const [feedback, setFeedback] = useState('');

    async function copy() {
        try {
            await navigator.clipboard.writeText(token);
            setFeedback(t('token_portal.copied_value', { label: 'API Key' }));
        } catch { setFeedback(t('token_portal.copy_failed')); }
    }
    async function regenerate() {
        setWorking(true);
        const ok = await onRegenerate();
        setWorking(false);
        setConfirming(false);
        setVisible(ok);
        setFeedback(t(ok ? 'token_portal.key_regenerated' : 'token_portal.key_regenerate_failed'));
    }

    const button = 'inline-flex items-center gap-1.5 rounded-md px-2 py-2 text-xs hover:bg-slate-200 focus-visible:ring-2 focus-visible:ring-blue-500 disabled:opacity-50';
    return <section className={embedded ? 'space-y-3' : 'bg-white border border-slate-200 rounded-xl p-5 sm:p-6 space-y-5'} aria-labelledby="api-key-title">
        <div>
            {embedded ? <h3 id="api-key-title" className="text-sm font-medium">{t('token_portal.api_key_title')}</h3> : <h2 id="api-key-title" className="text-lg font-semibold">{t('token_portal.api_key_title')}</h2>}
            <p className="text-xs text-slate-500 mt-2 leading-5">{t('token_portal.api_key_hint')}</p>
        </div>
        <div className="flex flex-wrap items-center gap-2 rounded-lg bg-slate-50 border border-slate-200 p-3">
            <code className="flex-1 min-w-0 break-all text-sm">{token ? (visible ? token : `••••••••••••${token.slice(-4)}`) : '…'}</code>
            <button type="button" onClick={() => setVisible(!visible)} disabled={!token} aria-pressed={visible} className={button}>{visible ? <EyeOff size={15} /> : <Eye size={15} />}{t(visible ? 'token_portal.hide_token' : 'token_portal.show_token')}</button>
            <button type="button" onClick={() => void copy()} disabled={!token} className={button}><Copy size={15} />{t('token_portal.copy_token')}</button>
        </div>
        {confirming ? (
            <div className="rounded-lg border border-red-200 bg-red-50 p-4 space-y-3">
                <p className="text-sm text-red-700">{t('token_portal.regenerate_confirm')}</p>
                <div className="flex gap-2">
                    <button type="button" onClick={() => void regenerate()} disabled={working} className="inline-flex items-center gap-2 rounded-lg bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-700 disabled:opacity-50">{working && <RefreshCw size={14} className="animate-spin" />}{t('token_portal.regenerate_key')}</button>
                    <button type="button" onClick={() => setConfirming(false)} disabled={working} className="rounded-lg border border-slate-300 bg-white px-4 py-2 text-sm hover:bg-slate-50">{t('common.cancel')}</button>
                </div>
            </div>
        ) : (
            <button type="button" onClick={() => { setFeedback(''); setConfirming(true); }} disabled={!token} className="inline-flex items-center gap-2 rounded-lg border border-slate-300 bg-white px-4 py-2 text-sm hover:bg-slate-50 disabled:opacity-50"><RefreshCw size={14} />{t('token_portal.regenerate_key')}</button>
        )}
        <p role="status" className="text-xs text-slate-500 min-h-4">{feedback}</p>
    </section>;
}
