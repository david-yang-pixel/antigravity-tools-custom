import { useState } from 'react';
import { Copy, ExternalLink } from 'lucide-react';
import { useTranslation } from 'react-i18next';

type Guide = 'pi_desktop' | 'cc_switch' | 'cursor' | 'other' | 'kelivo' | 'chatbox';
type Client = 'claude' | 'codex' | 'opencode';
type Protocol = 'chat' | 'responses' | 'anthropic';

const protocols = {
    chat: { name: 'OpenAI Chat Completions', path: '/v1/chat/completions' },
    responses: { name: 'OpenAI Responses', path: '/v1/responses' },
    anthropic: { name: 'Anthropic Messages', path: '/v1/messages' },
};
const clients = { claude: 'Claude Code', codex: 'Codex', opencode: 'OpenCode' };
const guideLinks: Record<Exclude<Guide, 'other'>, string> = {
    pi_desktop: 'https://github.com/vastsa/PI-Desktop/releases/latest',
    cc_switch: 'https://github.com/farion1231/cc-switch',
    cursor: 'https://cursor.com/downloads',
    kelivo: 'https://github.com/Chevey339/kelivo',
    chatbox: 'https://chatboxai.app',
};
const docs: Record<Exclude<Guide, 'other'>, string> = {
    pi_desktop: 'https://pi-docs.aiuo.net/guide/',
    cc_switch: 'https://github.com/farion1231/cc-switch/blob/main/docs/user-manual/en/2-providers/2.1-add.md',
    cursor: 'https://cursor.com/help/models-and-usage/api-keys',
    kelivo: 'https://github.com/Chevey339/kelivo#-getting-started',
    chatbox: 'https://docs.chatboxai.app/en/guides/providers',
};
const guideGroups: { title: string; items: Guide[] }[] = [
    { title: 'coding', items: ['pi_desktop', 'cc_switch', 'cursor', 'other'] },
    { title: 'chatting', items: ['kelivo', 'chatbox'] },
];
const recommended: Guide = 'pi_desktop';
const recommendedBadge = 'shrink-0 rounded-full bg-blue-600 px-2 py-0.5 text-[11px] font-medium leading-4 text-white';
const controlClass = 'rounded-lg border border-slate-200 bg-white px-3 py-2.5 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500';

export default function PortalConnectionGuide({ token, models, onOpenOverview }: {
    token: string;
    models: string[];
    onOpenOverview: () => void;
}) {
    const { t } = useTranslation();
    const [guide, setGuide] = useState<Guide>(recommended);
    const [client, setClient] = useState<Client>('claude');
    const [otherProtocol, setOtherProtocol] = useState<Protocol>('chat');
    const [selectedModel, setSelectedModel] = useState('');
    const [feedback, setFeedback] = useState('');
    // PI Desktop exposes the same protocol choice as "Other" via its API format field.
    const pickProtocol = guide === 'other' || guide === 'pi_desktop';
    const protocol: Protocol = guide === 'cc_switch'
        ? client === 'claude' ? 'anthropic' : client === 'codex' ? 'responses' : 'chat'
        : pickProtocol ? otherProtocol : 'chat';
    const origin = window.location.origin;
    // Chatbox appends its API path to API Host; the other clients expect a base URL.
    const baseUrl = guide === 'chatbox' || protocol === 'anthropic' ? origin : `${origin}/v1`;
    const modelOptions = [...new Set(models)].sort((a, b) => a.localeCompare(b));
    const model = modelOptions.includes(selectedModel) ? selectedModel : '';
    const steps = guide === 'cc_switch' ? `cc_${client}` : guide;
    const links = guide !== 'other' ? { download: guideLinks[guide], docs: docs[guide] } : null;

    async function copy(value: string, label: string) {
        try {
            await navigator.clipboard.writeText(value);
            setFeedback(t('token_portal.copied_value', { label }));
        } catch {
            setFeedback(t('portal_guide.copy_failed'));
        }
    }

    function copyButton(value: string, label: string) {
        return <button type="button" disabled={!value} aria-label={t('portal_guide.copy', { label })}
            onClick={() => void copy(value, label)}
            className="shrink-0 rounded-md p-2 text-slate-500 hover:bg-slate-100 disabled:opacity-40 disabled:cursor-not-allowed focus-visible:ring-2 focus-visible:ring-blue-500">
            <Copy size={16} aria-hidden="true" />
        </button>;
    }

    return <section aria-labelledby="guide-title" className="space-y-5">
        <div>
            <h2 id="guide-title" className="text-lg font-semibold">{t('token_portal.menu_guide')}</h2>
            <p className="mt-2 text-sm text-slate-500">{t('portal_guide.intro')}</p>
        </div>
        <div className="rounded-xl border border-slate-200 bg-white md:flex">
            <nav aria-label={t('portal_guide.software')} className="border-b border-slate-200 p-4 md:w-48 md:shrink-0 md:border-b-0 md:border-r space-y-5">
                {guideGroups.map(group => <div key={group.title}>
                    <h3 className="px-3 text-xs font-medium text-slate-500 mb-2">{t(`portal_guide.${group.title}`)}</h3>
                    <div className="flex flex-wrap gap-1 md:flex-col">
                        {group.items.map(item => <button key={item} type="button" aria-pressed={guide === item} aria-controls="guide-content"
                            onClick={() => { setGuide(item); setFeedback(''); }}
                            className={`flex items-center justify-between gap-2 rounded-lg px-3 py-2.5 text-left text-sm font-medium focus-visible:ring-2 focus-visible:ring-blue-500 ${guide === item ? 'bg-blue-50 text-blue-600' : 'text-slate-600 hover:bg-slate-50'}`}>
                            {t(`portal_guide.names.${item}`)}
                            {item === recommended && <span className={recommendedBadge}>{t('portal_guide.recommended')}</span>}
                        </button>)}
                    </div>
                </div>)}
            </nav>
            <div id="guide-content" className="flex-1 min-w-0 p-5 sm:p-6 space-y-6">
                <div className="flex flex-wrap items-center justify-between gap-3">
                    <h3 className="flex items-center gap-2 font-semibold">
                        {t(`portal_guide.names.${guide}`)}
                        {guide === recommended && <span className={recommendedBadge}>{t('portal_guide.recommended')}</span>}
                    </h3>
                    {links && <div className="flex flex-wrap gap-4 text-xs text-blue-600">
                        <a href={links.download} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 rounded hover:underline focus-visible:ring-2 focus-visible:ring-blue-500">{t('portal_guide.download')}<ExternalLink size={13} aria-hidden="true" /></a>
                        <a href={links.docs} target="_blank" rel="noopener noreferrer" className="inline-flex items-center gap-1 rounded hover:underline focus-visible:ring-2 focus-visible:ring-blue-500">{t('portal_guide.docs')}<ExternalLink size={13} aria-hidden="true" /></a>
                    </div>}
                </div>
                {guide === recommended && <p className="rounded-lg border border-blue-100 bg-blue-50 px-4 py-3 text-sm text-blue-700 leading-6">
                    {t('portal_guide.recommend_hint')}
                </p>}
                {guide === 'cc_switch' && <div className="space-y-2">
                    <label htmlFor="guide-client" className="block text-sm font-medium">{t('portal_guide.target_client')}</label>
                    <select id="guide-client" value={client} onChange={event => { setClient(event.target.value as Client); setFeedback(''); }} className={`${controlClass} w-full sm:w-64`}>
                        {Object.entries(clients).map(([id, name]) => <option key={id} value={id}>{name}</option>)}
                    </select>
                    <p className="text-xs text-slate-500 leading-5">{t('portal_guide.cc_hint')}</p>
                </div>}
                {pickProtocol && <div className="space-y-2">
                    <label htmlFor="guide-protocol" className="block text-sm font-medium">{t(guide === 'pi_desktop' ? 'portal_guide.api_format' : 'portal_guide.protocol')}</label>
                    <select id="guide-protocol" value={otherProtocol} onChange={event => { setOtherProtocol(event.target.value as Protocol); setFeedback(''); }} className={`${controlClass} w-full sm:w-72`}>
                        {Object.entries(protocols).map(([id, item]) => <option key={id} value={id}>{item.name}</option>)}
                    </select>
                </div>}
                <div className="rounded-lg border border-slate-200 bg-slate-50/50 p-4 space-y-4">
                    <div className="flex flex-wrap justify-between gap-2 text-xs text-slate-500">
                        <span>{t('portal_guide.configuration')}</span><span>{protocols[protocol].name}</span>
                    </div>
                    <div>
                        <p className="text-xs text-slate-500">{guide === 'chatbox' ? 'API Host' : 'Base URL'}</p>
                        <div className="flex items-center gap-2"><code className="min-w-0 flex-1 break-all text-sm">{baseUrl}</code>{copyButton(baseUrl, guide === 'chatbox' ? 'API Host' : 'Base URL')}</div>
                        <p className="mt-1 text-xs text-slate-500 break-all">{t('token_portal.request_path')}: <code>{protocols[protocol].path}</code></p>
                    </div>
                    <div>
                        <p className="text-xs text-slate-500">API Key</p>
                        <div className="flex flex-wrap items-center gap-2">
                            <code className="min-w-0 flex-1 text-sm">{token ? `••••••••••••${token.slice(-4)}` : t('portal_guide.key_unavailable')}</code>
                            {copyButton(token, 'API Key')}
                            <button type="button" onClick={onOpenOverview} className="rounded p-2 text-xs text-blue-600 hover:underline focus-visible:ring-2 focus-visible:ring-blue-500">{t('portal_guide.view_key')}</button>
                        </div>
                    </div>
                    <div>
                        <label htmlFor="guide-model" className="block text-xs text-slate-500 mb-2">{t('portal_guide.model')}</label>
                        <div className="flex items-center gap-2">
                            <select id="guide-model" value={model} disabled={!modelOptions.length} onChange={event => { setSelectedModel(event.target.value); setFeedback(''); }} className={`${controlClass} w-full min-w-0 flex-1 font-mono disabled:opacity-60`}>
                                <option value="">{t(modelOptions.length ? 'portal_guide.choose_model' : 'portal_guide.no_models')}</option>
                                {modelOptions.map(id => <option key={id} value={id}>{id}</option>)}
                            </select>
                            {copyButton(model, t('token_portal.model'))}
                        </div>
                        <p className="mt-2 text-xs text-slate-500 leading-5">{t('portal_guide.model_hint')}</p>
                    </div>
                    <p role="status" className="text-xs text-slate-500 min-h-4">{feedback}</p>
                </div>
                <ol className="list-decimal pl-5 space-y-3 text-sm leading-6 text-slate-600 marker:text-blue-600 marker:font-semibold">
                    {[1, 2, 3, 4].map(step => <li key={step} className="pl-1">{t(`portal_guide.steps.${steps}_${step}`)}</li>)}
                </ol>
                {(guide === 'cursor' || guide === 'cc_switch') && <p className="rounded-lg bg-amber-50 px-4 py-3 text-xs text-amber-800 leading-5">
                    {t(`portal_guide.notes.${guide === 'cc_switch' ? client : guide}`)}
                </p>}
                <details className="border-t border-slate-200 pt-4">
                    <summary className="cursor-pointer rounded text-sm font-medium focus-visible:ring-2 focus-visible:ring-blue-500">{t('portal_guide.troubleshooting')}</summary>
                    <ul className="mt-3 list-disc pl-5 text-xs text-slate-500 leading-6 space-y-1">
                        {['auth', 'model', 'connection', 'balance'].map(item => <li key={item}>{t(`portal_guide.errors.${item}`)}</li>)}
                    </ul>
                </details>
            </div>
        </div>
    </section>;
}
