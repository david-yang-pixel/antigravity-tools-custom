import { useState } from 'react';
import { Copy, Eye, EyeOff } from 'lucide-react';
import { useTranslation } from 'react-i18next';

export default function TokenConnectionDetails({ token }: { token: string }) {
    const { t } = useTranslation();
    const [visible, setVisible] = useState(false);
    const [feedback, setFeedback] = useState('');
    const origin = window.location.origin;
    const protocols = [
        { name: 'OpenAI Chat', base: `${origin}/v1`, path: '/v1/chat/completions' },
        { name: 'OpenAI Responses', base: `${origin}/v1`, path: '/v1/responses' },
        { name: 'Anthropic / Claude', base: origin, path: '/v1/messages' },
        { name: 'Gemini', base: origin, path: '/v1beta/models/{model}:generateContent' },
    ];
    async function copy(value: string, label: string) {
        try {
            await navigator.clipboard.writeText(value);
            setFeedback(t('token_portal.copied_value', { label }));
        } catch { setFeedback(t('token_portal.copy_failed')); }
    }
    return <section className="bg-white border border-slate-200 rounded-xl p-5 sm:p-6 space-y-5" aria-labelledby="connection-title">
        <div><h2 id="connection-title" className="text-lg font-semibold">{t('token_portal.connection_title')}</h2><p className="text-xs text-slate-500 mt-2 leading-5">{t('token_portal.base_url_hint')}</p></div>
        <div className="space-y-2">
            <p className="text-sm font-medium">{t('token_portal.current_token')}</p>
            <div className="flex flex-wrap items-center gap-2 rounded-lg bg-slate-50 border border-slate-200 p-3">
                <code className="flex-1 min-w-0 break-all text-sm">{visible ? token : `••••••••••••${token.slice(-4)}`}</code>
                <button type="button" onClick={() => setVisible(!visible)} aria-pressed={visible} className="inline-flex items-center gap-1.5 rounded-md px-2 py-2 text-xs hover:bg-slate-200 focus-visible:ring-2 focus-visible:ring-blue-500">{visible ? <EyeOff size={15} /> : <Eye size={15} />}{t(visible ? 'token_portal.hide_token' : 'token_portal.show_token')}</button>
                <button type="button" onClick={() => void copy(token, 'Token')} className="inline-flex items-center gap-1.5 rounded-md px-2 py-2 text-xs hover:bg-slate-200 focus-visible:ring-2 focus-visible:ring-blue-500"><Copy size={15} />{t('token_portal.copy_token')}</button>
            </div>
        </div>
        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {protocols.map(protocol => <div key={protocol.name} className="rounded-lg border border-slate-200 p-4 space-y-2 min-w-0">
                <h3 className="font-medium text-sm">{protocol.name}</h3>
                <p className="text-xs text-slate-500">Base URL</p>
                <div className="flex items-start gap-2"><code className="flex-1 min-w-0 break-all text-sm">{protocol.base}</code><button type="button" aria-label={t('token_portal.copy_base', { protocol: protocol.name })} onClick={() => void copy(protocol.base, `${protocol.name} Base URL`)} className="shrink-0 rounded-md p-2 text-slate-500 hover:bg-slate-100 focus-visible:ring-2 focus-visible:ring-blue-500"><Copy size={16} /></button></div>
                <p className="text-xs text-slate-500 break-all">{t('token_portal.request_path')}: <code>{protocol.path}</code></p>
            </div>)}
        </div>
        <p role="status" className="text-xs text-slate-500 min-h-4">{feedback}</p>
    </section>;
}
