import { useSearchParams } from 'react-router-dom';
import { useState, useEffect, useId, useRef, startTransition } from 'react';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../utils/request';
import { copyToClipboard } from '../utils/clipboard';
import {
    Power,
    Copy,
    RefreshCw,
    CheckCircle,
    Settings,
    Terminal,
    Trash2,
    BrainCircuit,
    Sparkles,
    Code,
    X,
    Edit2,
    Save,
    Share2,
    Bot
} from 'lucide-react';
import { AppConfig, ProxyConfig, StickySessionConfig, ExperimentalConfig } from '../types/config';
import HelpTooltip from '../components/common/HelpTooltip';
import ModalDialog from '../components/common/ModalDialog';
import { showToast } from '../components/common/ToastContainer';
import { cn } from '../utils/cn';
import { useProxyModels } from '../hooks/useProxyModels';
import ModelRegistryPanel from '../components/proxy/models/ModelRegistryPanel';
import { isTauri } from '../utils/env';
import { listAccounts } from '../services/accountService';
import CircuitBreaker from '../components/settings/CircuitBreaker';
import GlobalSystemPrompt from '../components/settings/GlobalSystemPrompt';
import ImageThinkingMode from '../components/settings/ImageThinkingMode';
import ThinkingBudget from '../components/settings/ThinkingBudget';
import MultimodalSettings from '../components/settings/MultimodalSettings';
import AgentSettings from '../components/settings/AgentSettings';
import SourceSettings from '../components/proxy/models/SourceSettings';
import ServerAgentSettings from '../components/proxy/models/ServerAgentSettings';
import { CircuitBreakerConfig } from '../types/config';

interface ProxyStatus {
    running: boolean;
    port: number;
    base_url: string;
    active_accounts: number;
}


interface CollapsibleCardProps {
    title: string;
    icon: React.ReactNode;
    enabled?: boolean;
    onToggle?: (enabled: boolean) => void;
    children: React.ReactNode;
    defaultExpanded?: boolean;
    rightElement?: React.ReactNode;
    allowInteractionWhenDisabled?: boolean;
}

function CollapsibleCard({
    title,
    icon,
    enabled,
    onToggle,
    children,
    defaultExpanded = false,
    rightElement,
    allowInteractionWhenDisabled = false,
}: CollapsibleCardProps) {
    const [isExpanded, setIsExpanded] = useState(defaultExpanded);
    useEffect(() => { if (defaultExpanded) setIsExpanded(true); }, [defaultExpanded]);
    const contentId = useId();
    const { t } = useTranslation();
    const toggle = () => setIsExpanded(value => !value);
    const disclosureProps = { 'aria-expanded': isExpanded, 'aria-controls': contentId };
    const onDisclosureKeyDown = (event: React.KeyboardEvent<HTMLButtonElement>) => {
        if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
            event.preventDefault(); setIsExpanded(event.key === 'ArrowRight');
        }
    };

    return (
        <div className="proxy-collapsible bg-white dark:bg-base-100 rounded-xl shadow-xs border border-gray-200/80 dark:border-base-200">
            <div className="px-4 py-3 flex flex-wrap items-center gap-2 bg-gray-50/70 dark:bg-base-200 rounded-t-xl">
                <button type="button" {...disclosureProps} onClick={toggle} onKeyDown={onDisclosureKeyDown}
                    className="flex flex-1 min-w-0 items-center gap-3 text-left rounded-lg py-1 text-sm font-semibold text-gray-900 dark:text-white hover:text-blue-600 dark:hover:text-blue-400 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-blue-500">
                    <span className="text-gray-500 dark:text-gray-400 shrink-0" aria-hidden>{icon}</span>
                    <span className="break-words">{title}</span>
                    {enabled !== undefined && <span className={cn('text-xs px-2 py-0.5 rounded-full border', enabled ? 'bg-emerald-50 text-emerald-700 border-emerald-200 dark:bg-emerald-950/50 dark:text-emerald-400 dark:border-emerald-800' : 'bg-gray-100 text-gray-600 border-gray-200 dark:bg-base-200 dark:text-gray-400 dark:border-base-300')}>
                        {enabled ? t('common.enabled') : t('common.disabled')}
                    </span>}
                </button>
                <div className="flex flex-wrap items-center gap-3">
                    {rightElement}
                    {enabled !== undefined && onToggle && <input type="checkbox" aria-label={`${title}：${t('common.enabled')}`}
                        className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-4 focus-visible:outline-blue-500"
                        checked={enabled} onChange={event => onToggle(event.target.checked)} />}
                    <button type="button" {...disclosureProps} aria-label={`${isExpanded ? '收起 / Collapse ' : '展开 / Expand '}${title}`} onClick={toggle} onKeyDown={onDisclosureKeyDown}
                        className="p-2 rounded-lg text-gray-500 hover:bg-gray-200/70 dark:text-gray-400 dark:hover:bg-base-300 focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blue-500">
                        <svg className={cn('motion-safe:transition-transform motion-safe:duration-150', isExpanded && 'rotate-180')} aria-hidden width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round"><path d="m6 9 6 6 6-6" /></svg>
                    </button>
                </div>
            </div>
            <div id={contentId} hidden={!isExpanded} className="border-t border-gray-200/80 dark:border-base-200 p-4">
                <div inert={enabled === false && !allowInteractionWhenDisabled} className={enabled === false && !allowInteractionWhenDisabled ? 'opacity-50' : ''}>{children}</div>
            </div>
        </div>
    );
}

export default function ApiProxy() {
    const { t } = useTranslation();

    const { models, loading: modelsLoading, error: modelsError, refresh: refreshModels } = useProxyModels();

    const [status, setStatus] = useState<ProxyStatus>({
        running: false,
        port: 0,
        base_url: '',
        active_accounts: 0,
    });

    const [appConfig, setAppConfig] = useState<AppConfig | null>(null);
    const [searchParams] = useSearchParams();
    const showWorkBuddy = searchParams.get('section') === 'workbuddy';
    const [activeMenuTab, setActiveMenuTab] = useState<'settings' | 'models' | 'protocols'>(showWorkBuddy ? 'models' : 'settings');
    useEffect(() => { if (showWorkBuddy) setActiveMenuTab('models'); }, [showWorkBuddy]);
    const [configLoading, setConfigLoading] = useState(true);
    const [configError, setConfigError] = useState<string | null>(null);
    const [loading, setLoading] = useState(false);
    const [advancedDirty, setAdvancedDirty] = useState(false);
    const [advancedSaving, setAdvancedSaving] = useState(false);
    const [advancedFeedback, setAdvancedFeedback] = useState<{ error?: boolean; message: string } | null>(null);
    const advancedDraftVersion = useRef(0);
    const advancedSaveInFlight = useRef<Promise<void> | null>(null);
    const latestConfig = useRef(appConfig);
    latestConfig.current = appConfig;
    const [copied, setCopied] = useState<string | null>(null);
    const [selectedProtocol, setSelectedProtocol] = useState<'openai' | 'anthropic' | 'gemini'>('openai');
    const [selectedModelId, setSelectedModelId] = useState('');

    useEffect(() => {
        const selectable = models.filter(model => model.protocols.includes(selectedProtocol));
        if (!selectable.some(model => model.id === selectedModelId)) {
            setSelectedModelId(selectable[0]?.id ?? '');
        }
    }, [models, selectedProtocol, selectedModelId]);

    // API Key editing states
    const [isEditingApiKey, setIsEditingApiKey] = useState(false);
    const [tempApiKey, setTempApiKey] = useState('');

    const [isEditingAdminPassword, setIsEditingAdminPassword] = useState(false);
    const [tempAdminPassword, setTempAdminPassword] = useState('');

    const [isRegenerateKeyConfirmOpen, setIsRegenerateKeyConfirmOpen] = useState(false);
    const [isClearBindingsConfirmOpen, setIsClearBindingsConfirmOpen] = useState(false);
    const [isClearRateLimitsConfirmOpen, setIsClearRateLimitsConfirmOpen] = useState(false);

    // [FIX #820] Fixed account mode states
    const [preferredAccountId, setPreferredAccountId] = useState<string | null>(null);
    const [availableAccounts, setAvailableAccounts] = useState<Array<{ id: string; email: string }>>([]);


    // 初始化加载
    useEffect(() => {
        loadConfig();
        loadStatus();
        loadAccounts();
        loadPreferredAccount();
        const interval = setInterval(loadStatus, 3000);
        return () => clearInterval(interval);
    }, []);



    // [FIX #820] Load available accounts for fixed account mode
    const loadAccounts = async () => {
        try {
            const accounts = await listAccounts();
            setAvailableAccounts(accounts.map(a => ({ id: a.id, email: a.email })));
        } catch (error) {
            console.error('Failed to load accounts:', error);
        }
    };

    // [FIX #820] Load current preferred account
    const loadPreferredAccount = async () => {
        try {
            const prefId = await invoke<string | null>('get_preferred_account');
            setPreferredAccountId(prefId);
        } catch (error) {
            // Service not running, ignore
        }
    };

    // [FIX #820] Set preferred account
    const handleSetPreferredAccount = async (accountId: string | null) => {
        try {
            const wasEnabled = preferredAccountId !== null;
            await invoke('set_preferred_account', { accountId });
            setPreferredAccountId(accountId);

            // Determine appropriate message
            let message: string;
            if (accountId === null) {
                message = t('proxy.config.scheduling.round_robin_set', { defaultValue: 'Round-robin mode enabled' });
            } else if (wasEnabled) {
                // Changed account while already in fixed mode
                const account = availableAccounts.find(a => a.id === accountId);
                message = t('proxy.config.scheduling.account_changed', {
                    defaultValue: `Switched to ${account?.email || accountId}`,
                    email: account?.email || accountId
                });
            } else {
                // Just enabled fixed mode
                message = t('proxy.config.scheduling.fixed_account_set', { defaultValue: 'Fixed account mode enabled' });
            }

            showToast(message, 'success');
        } catch (error) {
            showToast(String(error), 'error');
        }
    };

    const loadConfig = async () => {
        setConfigLoading(true);
        setConfigError(null);
        try {
            const config = await invoke<AppConfig>('load_config');
            setAppConfig(config);

        } catch (error) {
            console.error('加载配置失败:', error);
            setConfigError(String(error));
        } finally {
            setConfigLoading(false);
        }
    };

    const loadStatus = async () => {
        try {
            const s = await invoke<ProxyStatus>('get_proxy_status');
            // 如果后端返回 starting 或 busy，则在 UI 上表现为加载中
            if (s.base_url === 'starting' || s.base_url === 'busy') {
                // 如果当前已经是运行状态，不要被覆盖为 false
                setStatus(prev => ({ ...s, running: prev.running }));
            } else {
                setStatus(s);
            }
        } catch (error) {
            console.error('获取状态失败:', error);
        }
    };


    const saveConfig = async (newConfig: AppConfig) => {
        // 1. 立即更新 UI 状态，确保流畅
        setAppConfig(newConfig);
        try {
            await invoke('save_config', { config: newConfig });
            refreshModels();
        } catch (error) {
            console.error('保存配置失败:', error);
            showToast(`${t('common.error')}: ${error}`, 'error');
        }
    };

    const handleSaveProxySettings = async () => {
        if (!appConfig) return;
        try {
            await invoke('save_config', { config: appConfig });
            const refreshed = await invoke<AppConfig>('load_config');
            if (refreshed) {
                setAppConfig(refreshed);
            }
            showToast(t('common.saved'), 'success');
        } catch (error) {
            console.error('保存配置失败:', error);
            showToast(`${t('common.error')}: ${error}`, 'error');
        }
    };

    // Advanced controls are an explicit draft, not a stream of per-keystroke writes.
    const updateAdvancedProxyConfig = (updates: Partial<ProxyConfig>) => {
        const current = latestConfig.current;
        if (!current) return;
        const next = { ...current, proxy: { ...current.proxy, ...updates } };
        latestConfig.current = next; // ThinkingBudget can normalize then save in one event.
        advancedDraftVersion.current += 1;
        setAdvancedDirty(true); setAdvancedFeedback(null); setAppConfig(next);
    };

    const handleSaveAdvancedSettings = (): Promise<void> => {
        if (advancedSaveInFlight.current) return advancedSaveInFlight.current;
        const draft = latestConfig.current;
        if (!draft) return Promise.reject(new Error('配置未加载 / Configuration not loaded'));
        const draftVersion = advancedDraftVersion.current;
        setAdvancedSaving(true); setAdvancedFeedback(null);
        const pending = invoke('save_config', { config: draft }).then(() => {
            if (draftVersion === advancedDraftVersion.current) setAdvancedDirty(false);
            setAdvancedFeedback({ message: '高级设置已保存。仅作用于标注的请求链路，不作为能力验证证据。 / Advanced settings saved; applicability labels still apply, not capability evidence.' });
        }).catch(error => {
            setAdvancedFeedback({ error: true, message: `保存失败，草稿保留，可重试。 / Save failed; draft retained. ${String(error)}` });
            throw error; // Child controls must not display a false success.
        }).finally(() => { setAdvancedSaving(false); advancedSaveInFlight.current = null; });
        advancedSaveInFlight.current = pending;
        return pending;
    };


    const updateProxyConfig = (updates: Partial<ProxyConfig>) => {
        if (!appConfig) return;
        const newConfig = {
            ...appConfig,
            proxy: {
                ...appConfig.proxy,
                ...updates
            }
        };
        saveConfig(newConfig);
    };

    const updateSchedulingConfig = (updates: Partial<StickySessionConfig>) => {
        if (!appConfig) return;
        const currentScheduling = appConfig.proxy.scheduling || { mode: 'Balance', max_wait_seconds: 60 };
        const newScheduling = { ...currentScheduling, ...updates };

        const newAppConfig = {
            ...appConfig,
            proxy: {
                ...appConfig.proxy,
                scheduling: newScheduling
            }
        };
        saveConfig(newAppConfig);
    };

    const updateExperimentalConfig = (updates: Partial<ExperimentalConfig>) => {
        const current = latestConfig.current;
        if (!current) return;
        const next = { ...current, proxy: { ...current.proxy, experimental: { enable_usage_scaling: false, ...current.proxy.experimental, ...updates } } };
        latestConfig.current = next;
        advancedDraftVersion.current += 1;
        setAdvancedDirty(true); setAdvancedFeedback(null); setAppConfig(next);
    };

    const updateCircuitBreakerConfig = (newBreakerConfig: CircuitBreakerConfig) => {
        if (!appConfig) return;
        const newConfig = {
            ...appConfig,
            circuit_breaker: newBreakerConfig
        };
        saveConfig(newConfig);
    };

    const handleClearSessionBindings = () => {
        setIsClearBindingsConfirmOpen(true);
    };

    const executeClearSessionBindings = async () => {
        setIsClearBindingsConfirmOpen(false);
        try {
            await invoke('clear_proxy_session_bindings');
            showToast(t('common.success'), 'success');
        } catch (error) {
            console.error('Failed to clear session bindings:', error);
            showToast(`${t('common.error')}: ${error}`, 'error');
        }
    };

    const handleClearRateLimits = () => {
        setIsClearRateLimitsConfirmOpen(true);
    };

    const executeClearRateLimits = async () => {
        setIsClearRateLimitsConfirmOpen(false);
        try {
            await invoke('clear_all_proxy_rate_limits');
            showToast(t('common.success'), 'success');
        } catch (error) {
            console.error('Failed to clear rate limits:', error);
            showToast(`${t('common.error')}: ${error}`, 'error');
        }
    };

    const handleToggle = async () => {
        if (!appConfig) return;
        setLoading(true);
        try {
            if (status.running) {
                await invoke('stop_proxy_service');
                const newConfig = {
                    ...appConfig,
                    proxy: {
                        ...appConfig.proxy,
                        auto_start: false
                    }
                };
                setAppConfig(newConfig);
                await invoke('save_config', { config: newConfig });
            } else {
                const newConfig = {
                    ...appConfig,
                    proxy: {
                        ...appConfig.proxy,
                        auto_start: true
                    }
                };
                setAppConfig(newConfig);
                await invoke('save_config', { config: newConfig });
                // 使用当前的 appConfig.proxy 启动
                await invoke('start_proxy_service', { config: newConfig.proxy });
            }
            await loadStatus();
        } catch (error: any) {
            showToast(t('proxy.dialog.operate_failed', { error: error.toString() }), 'error');
        } finally {
            setLoading(false);
        }
    };

    const handleGenerateApiKey = () => {
        setIsRegenerateKeyConfirmOpen(true);
    };

    const executeGenerateApiKey = async () => {
        setIsRegenerateKeyConfirmOpen(false);
        try {
            const newKey = await invoke<string>('generate_api_key');
            updateProxyConfig({ api_key: newKey });
            showToast(t('common.success'), 'success');
        } catch (error: any) {
            console.error('生成 API Key 失败:', error);
            showToast(t('proxy.dialog.operate_failed', { error: error.toString() }), 'error');
        }
    };

    const copyToClipboardHandler = (text: string, label: string) => {
        copyToClipboard(text).then((success) => {
            if (success) {
                setCopied(label);
                setTimeout(() => setCopied(null), 2000);
            }
        });
    };

    // API Key editing functions
    const validateApiKey = (key: string): boolean => {
        // Must start with 'sk-' and be at least 10 characters long
        return key.startsWith('sk-') && key.length >= 10;
    };

    const handleEditApiKey = () => {
        setTempApiKey(appConfig?.proxy.api_key || '');
        setIsEditingApiKey(true);
    };

    const handleSaveApiKey = () => {
        if (!validateApiKey(tempApiKey)) {
            showToast(t('proxy.config.api_key_invalid'), 'error');
            return;
        }
        updateProxyConfig({ api_key: tempApiKey });
        setIsEditingApiKey(false);
        showToast(t('proxy.config.api_key_updated'), 'success');
    };

    const handleCancelEditApiKey = () => {
        setTempApiKey('');
        setIsEditingApiKey(false);
    };

    // Admin Password editing functions
    const handleEditAdminPassword = () => {
        setTempAdminPassword(appConfig?.proxy.admin_password || '');
        setIsEditingAdminPassword(true);
    };

    const handleSaveAdminPassword = () => {
        // Validation: can be empty (meaning fallback to api_key) or at least 4 chars
        if (tempAdminPassword && tempAdminPassword.length < 4) {
            showToast(t('proxy.config.admin_password_short', { defaultValue: 'Password is too short (min 4 chars)' }), 'error');
            return;
        }
        updateProxyConfig({ admin_password: tempAdminPassword || undefined });
        setIsEditingAdminPassword(false);
        showToast(t('proxy.config.admin_password_updated', { defaultValue: 'Web UI password updated' }), 'success');
    };

    const handleCancelEditAdminPassword = () => {
        setTempAdminPassword('');
        setIsEditingAdminPassword(false);
    };


    const getPythonExample = (modelId: string) => {
        const port = status.running ? status.port : (appConfig?.proxy.port || 8045);
        // 推荐使用 127.0.0.1 以避免部分环境 IPv6 解析延迟问题
        const baseUrl = `http://127.0.0.1:${port}/v1`;
        const apiKey = appConfig?.proxy.api_key || 'YOUR_API_KEY';

        // 1. Anthropic Protocol
        if (selectedProtocol === 'anthropic') {
            return `from anthropic import Anthropic

client = Anthropic(
    # Recommended: use 127.0.0.1
    base_url="${`http://127.0.0.1:${port}`}",
    api_key="${apiKey}"
)

# Only published models compatible with this gateway protocol are listed.
response = client.messages.create(
    model="${modelId}",
    max_tokens=1024,
    messages=[{"role": "user", "content": "Hello"}]
)

print(response.content[0].text)`;
        }

        // 2. Gemini Protocol (Native)
        if (selectedProtocol === 'gemini') {
            const rawBaseUrl = `http://127.0.0.1:${port}`;
            return `# Requires: pip install google-generativeai
import google.generativeai as genai

# Use the Antigravity proxy address (recommended: 127.0.0.1)
genai.configure(
    api_key="${apiKey}",
    transport='rest',
    client_options={'api_endpoint': '${rawBaseUrl}'}
)

model = genai.GenerativeModel('${modelId}')
response = model.generate_content("Hello")
print(response.text)`;
        }

        // 3. OpenAI Protocol — image generation models (any *-image model)
        if (models.find(model => model.id === modelId)?.image_generation) {
            return `from openai import OpenAI

client = OpenAI(
    base_url="${baseUrl}",
    api_key="${apiKey}"
)

# This public ID is bound to an Antigravity image target in the registry.
# Image capability and per-account quota are not proven by basic text checks.

response = client.chat.completions.create(
    model="${modelId}",

    # Aspect ratio — Option 1: the size parameter (recommended)
    #   "1024x1024" = 1:1   |   "1280x720" = 16:9
    #   "720x1280"  = 9:16  |   "1216x896" = 4:3
    extra_body={ "size": "1024x1024" },

    # Use the exact published public ID; unpublished suffix aliases are rejected.
    messages=[{
        "role": "user",
        "content": "Draw a futuristic city"
    }]
)

# The generated image is returned INSIDE the message content
# (as a base64 data URL / markdown image), not as a hosted URL.
# Extract the base64 payload from here to save the file.
print(response.choices[0].message.content)`;
        }

        return `from openai import OpenAI

client = OpenAI(
    base_url="${baseUrl}",
    api_key="${apiKey}"
)

response = client.chat.completions.create(
    model="${modelId}",
    messages=[{"role": "user", "content": "Hello"}]
)

print(response.choices[0].message.content)`;
    };

    // Compatibility follows the registered source/native target, never an alias.
    const filteredModels = models.filter(model => model.protocols.includes(selectedProtocol));

    const renderMultiProtocolSection = () => {
        if (!appConfig) return null;
        return (
            <div className="bg-white dark:bg-base-100 rounded-xl shadow-sm border border-gray-100 dark:border-base-200 overflow-hidden">
                <div className="p-3">
                    <div className="flex items-center gap-3 mb-3">
                        <div className="w-8 h-8 rounded-lg bg-gradient-to-br from-blue-500 to-purple-600 flex items-center justify-center shadow-md">
                            <Code size={16} className="text-white" />
                        </div>
                        <div>
                            <h3 className="text-base font-bold text-gray-900 dark:text-base-content">
                                🔗 {t('proxy.multi_protocol.title')}
                            </h3>
                            <p className="text-[10px] text-gray-500 dark:text-gray-400">
                                {t('proxy.multi_protocol.subtitle')}
                            </p>
                        </div>
                    </div>

                    <p className="text-xs text-gray-700 dark:text-gray-300 mb-4 leading-relaxed">
                        {t('proxy.multi_protocol.description')}
                    </p>

                    <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
                        {/* OpenAI Card */}
                        <div
                            className={`p-3 rounded-xl border-2 transition-all cursor-pointer ${selectedProtocol === 'openai' ? 'border-blue-500 bg-blue-50/30 dark:bg-blue-900/10' : 'border-gray-100 dark:border-base-200 hover:border-blue-200'}`}
                            onClick={() => startTransition(() => setSelectedProtocol('openai'))}
                        >
                            <div className="flex items-center justify-between mb-2">
                                <span className="text-xs font-bold text-blue-600">{t('proxy.multi_protocol.openai_label')}</span>
                                <button onClick={(e) => {
                                    e.stopPropagation();
                                    const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                    copyToClipboardHandler(`${baseUrl}/v1`, 'openai');
                                }} className="btn btn-ghost btn-xs">
                                    {copied === 'openai' ? <CheckCircle size={14} /> : <div className="flex items-center gap-1 text-[10px] uppercase font-bold tracking-tighter"><Copy size={12} /> {t('proxy.multi_protocol.copy_base', { defaultValue: 'Base' })}</div>}
                                </button>
                            </div>
                            <div className="space-y-1">
                                <div className="flex items-center justify-between hover:bg-black/5 dark:hover:bg-white/5 rounded p-0.5 group">
                                    <code className="text-[10px] opacity-70">/v1/chat/completions</code>
                                    <button onClick={(e) => {
                                        e.stopPropagation();
                                        const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                        copyToClipboardHandler(`${baseUrl}/v1/chat/completions`, 'openai-chat');
                                    }} className="opacity-0 group-hover:opacity-100 transition-opacity">
                                        {copied === 'openai-chat' ? <CheckCircle size={10} className="text-green-500" /> : <Copy size={10} />}
                                    </button>
                                </div>
                                <div className="flex items-center justify-between hover:bg-black/5 dark:hover:bg-white/5 rounded p-0.5 group">
                                    <code className="text-[10px] opacity-70">/v1/completions</code>
                                    <button onClick={(e) => {
                                        e.stopPropagation();
                                        const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                        copyToClipboardHandler(`${baseUrl}/v1/completions`, 'openai-compl');
                                    }} className="opacity-0 group-hover:opacity-100 transition-opacity">
                                        {copied === 'openai-compl' ? <CheckCircle size={10} className="text-green-500" /> : <Copy size={10} />}
                                    </button>
                                </div>
                                <div className="flex items-center justify-between hover:bg-black/5 dark:hover:bg-white/5 rounded p-0.5 group">
                                    <code className="text-[10px] opacity-70 font-bold text-blue-500">/v1/responses (Codex)</code>
                                    <button onClick={(e) => {
                                        e.stopPropagation();
                                        const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                        copyToClipboardHandler(`${baseUrl}/v1/responses`, 'openai-resp');
                                    }} className="opacity-0 group-hover:opacity-100 transition-opacity">
                                        {copied === 'openai-resp' ? <CheckCircle size={10} className="text-green-500" /> : <Copy size={10} />}
                                    </button>
                                </div>
                            </div>
                        </div>

                        {/* Anthropic Card */}
                        <div
                            className={`p-3 rounded-xl border-2 transition-all cursor-pointer ${selectedProtocol === 'anthropic' ? 'border-purple-500 bg-purple-50/30 dark:bg-purple-900/10' : 'border-gray-100 dark:border-base-200 hover:border-purple-200'}`}
                            onClick={() => startTransition(() => setSelectedProtocol('anthropic'))}
                        >
                            <div className="flex items-center justify-between mb-2">
                                <span className="text-xs font-bold text-purple-600">{t('proxy.multi_protocol.anthropic_label')}</span>
                                <button onClick={(e) => {
                                    e.stopPropagation();
                                    const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                    copyToClipboardHandler(`${baseUrl}/v1/messages`, 'anthropic');
                                }} className="btn btn-ghost btn-xs">
                                    {copied === 'anthropic' ? <CheckCircle size={14} /> : <Copy size={14} />}
                                </button>
                            </div>
                            <code className="text-[10px] block truncate bg-black/5 dark:bg-white/5 p-1 rounded">/v1/messages</code>
                        </div>

                        {/* Gemini Card */}
                        <div
                            className={`p-3 rounded-xl border-2 transition-all cursor-pointer ${selectedProtocol === 'gemini' ? 'border-green-500 bg-green-50/30 dark:bg-green-900/10' : 'border-gray-100 dark:border-base-200 hover:border-green-200'}`}
                            onClick={() => startTransition(() => setSelectedProtocol('gemini'))}
                        >
                            <div className="flex items-center justify-between mb-2">
                                <span className="text-xs font-bold text-green-600">{t('proxy.multi_protocol.gemini_label')}</span>
                                <button onClick={(e) => {
                                    e.stopPropagation();
                                    const baseUrl = status.running ? status.base_url : `http://127.0.0.1:${appConfig.proxy.port || 8045}`;
                                    copyToClipboardHandler(`${baseUrl}/v1beta/models`, 'gemini');
                                }} className="btn btn-ghost btn-xs">
                                    {copied === 'gemini' ? <CheckCircle size={14} /> : <Copy size={14} />}
                                </button>
                            </div>
                            <code className="text-[10px] block truncate bg-black/5 dark:bg-white/5 p-1 rounded">/v1beta/models/...</code>
                        </div>
                    </div>
                </div>
            </div>
        );
    };

    const renderSupportedModelsSection = () => {
        if (!appConfig) return null;
        return (
            <div className="bg-white dark:bg-base-100 rounded-xl shadow-sm border border-gray-100 dark:border-base-200 overflow-hidden">
                <div className="px-4 py-2.5 border-b border-gray-100 dark:border-base-200">
                    <h2 className="text-base font-bold text-gray-900 dark:text-base-content flex items-center gap-2">
                        <Terminal size={18} />
                        {t('proxy.supported_models.title')}
                    </h2>
                    <p className="text-xs text-gray-500 dark:text-gray-400 mt-1">仅已上架且启用的来源绑定；按当前协议筛选。WB 不提供 Gemini 协议。 / Published bindings compatible with the selected protocol only; WB has no Gemini endpoint.</p>
                </div>

                <div className="grid grid-cols-1 lg:grid-cols-3 gap-0 lg:divide-x dark:divide-gray-700">
                    {/* 左侧：模型列表 */}
                    <div className="col-span-2 p-0">
                        <div className="overflow-x-auto">
                            <table className="table w-full">
                                <thead className="bg-gray-50/50 dark:bg-gray-800/50 text-gray-500 dark:text-gray-400">
                                    <tr>
                                        <th className="w-10 pl-3"></th>
                                        <th className="text-[11px] font-medium">{t('proxy.supported_models.model_name')}</th>
                                        <th className="text-[11px] font-medium">{t('proxy.supported_models.model_id')}</th>
                                        <th className="text-[11px] hidden sm:table-cell font-medium">来源 / 原生目标 · Source / Target</th>
                                        <th className="text-[11px] w-20 text-center font-medium">{t('proxy.supported_models.action')}</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {filteredModels.length === 0 && <tr><td colSpan={5} className="text-center text-sm text-gray-500 py-8">{t(modelsLoading ? 'common.loading' : modelsError ? 'common.load_failed' : 'token_portal.empty_catalog')}</td></tr>}
                                    {filteredModels.map((m) => (
                                        <tr
                                            key={m.id}
                                            className={`hover:bg-blue-50/50 dark:hover:bg-blue-900/10 cursor-pointer transition-colors ${selectedModelId === m.id ? 'bg-blue-50/80 dark:bg-blue-900/20' : ''}`}
                                            onClick={() => setSelectedModelId(m.id)}
                                        >
                                            <td className="pl-4 text-blue-500">{m.icon}</td>
                                            <td className="font-bold text-xs"><button type="button" className="rounded text-left focus-visible:outline focus-visible:outline-2 focus-visible:outline-blue-500" aria-pressed={selectedModelId === m.id} onClick={() => setSelectedModelId(m.id)}>{m.name}</button><span className="block mt-1 font-normal text-[10px] text-gray-500 dark:text-gray-400 sm:hidden">{m.source_name} → {m.native_model}</span></td>
                                            <td className="font-mono text-[10px] text-gray-500">{m.id}</td>
                                            <td className="text-[10px] text-gray-400 hidden sm:table-cell">{m.desc}</td>
                                            <td className="text-center">
                                                <button
                                                    type="button" aria-label={`${t('common.copy')} ${m.id}`}
                                                    className="btn btn-ghost btn-xs text-blue-500"
                                                    onClick={(e) => {
                                                        e.stopPropagation();
                                                        copyToClipboardHandler(m.id, `model-${m.id}`);
                                                    }}
                                                >
                                                    {copied === `model-${m.id}` ? <CheckCircle size={14} /> : <div className="flex items-center gap-1 text-[10px] font-bold tracking-tight"><Copy size={12} /> {t('common.copy')}</div>}
                                                </button>
                                            </td>
                                        </tr>
                                    ))}
                                </tbody>
                            </table>
                        </div>
                    </div>

                    {/* 右侧：代码预览 */}
                    <div className="col-span-1 bg-gray-900 text-blue-100 flex flex-col h-[400px] lg:h-auto">
                        <div className="p-3 border-b border-gray-800 flex items-center justify-between">
                            <span className="text-xs font-bold text-gray-400 uppercase tracking-wider">{t('proxy.multi_protocol.quick_integration')}</span>
                            <div className="flex gap-2">
                                <span className="text-[10px] px-2 py-0.5 rounded bg-blue-500/20 text-blue-400 border border-blue-500/30">
                                    {selectedProtocol === 'anthropic' ? 'Python (Anthropic SDK)' : (selectedProtocol === 'gemini' ? 'Python (Google GenAI)' : 'Python (OpenAI SDK)')}
                                </span>
                            </div>
                        </div>
                        <div className="flex-1 relative overflow-hidden group">
                            <div className="absolute inset-0 overflow-auto scrollbar-thin scrollbar-thumb-gray-700 scrollbar-track-transparent">
                                <pre className="p-4 text-[10px] font-mono leading-relaxed">
                                    {selectedModelId ? getPythonExample(selectedModelId) : t('token_portal.empty_catalog')}
                                </pre>
                            </div>
                            <button
                                disabled={!selectedModelId}
                                onClick={() => copyToClipboardHandler(getPythonExample(selectedModelId), 'example-code')}
                                className="absolute top-4 right-4 p-2 bg-white/10 hover:bg-white/20 rounded-lg transition-colors text-white opacity-0 group-hover:opacity-100"
                            >
                                {copied === 'example-code' ? <CheckCircle size={16} /> : <Copy size={16} />}
                            </button>
                        </div>
                        <div className="p-3 bg-gray-800/50 border-t border-gray-800 text-[10px] text-gray-400">
                            {t('proxy.multi_protocol.click_tip')}
                        </div>
                    </div>
                </div>
            </div>
        );
    };

    return (
        <div className="h-full w-full overflow-y-auto overflow-x-hidden">
            <div className="p-5 space-y-4 max-w-7xl mx-auto">

                {/* Loading State */}
                {configLoading && (
                    <div className="flex items-center justify-center py-20">
                        <div className="flex flex-col items-center gap-4">
                            <RefreshCw size={32} className="animate-spin text-blue-500" />
                            <span className="text-sm text-gray-500 dark:text-gray-400">
                                {t('common.loading')}
                            </span>
                        </div>
                    </div>
                )}

                {/* Error State */}
                {!configLoading && configError && (
                    <div className="flex items-center justify-center py-20">
                        <div className="flex flex-col items-center gap-4 text-center">
                            <div className="w-16 h-16 rounded-full bg-red-100 dark:bg-red-900/30 flex items-center justify-center">
                                <Settings size={32} className="text-red-500" />
                            </div>
                            <div className="space-y-2">
                                <h3 className="text-lg font-semibold text-gray-900 dark:text-gray-100">
                                    {t('proxy.error.load_failed')}
                                </h3>
                                <p className="text-sm text-gray-500 dark:text-gray-400 max-w-md">
                                    {configError}
                                </p>
                            </div>
                            <button
                                onClick={loadConfig}
                                className="px-4 py-2 bg-blue-600 hover:bg-blue-700 text-white rounded-lg text-sm font-medium flex items-center gap-2 transition-colors"
                            >
                                <RefreshCw size={16} />
                                {t('common.retry')}
                            </button>
                        </div>
                    </div>
                )}

                {/* 配置区 */}
                {!configLoading && !configError && appConfig && (
                    <div className="bg-white dark:bg-base-100 rounded-xl shadow-xs border border-gray-200/80 dark:border-base-200">
                        <div className="px-4 sm:px-5 py-2.5 border-b border-gray-100 dark:border-base-200 bg-gray-50/60 dark:bg-base-200/80 flex flex-wrap items-center justify-between gap-3">
                            {/* 三个可选菜单栏：服务配置、CLI 一键配置、多协议支持 */}
                            <div className="flex items-center gap-1.5 bg-gray-200/70 dark:bg-base-300/60 p-1 rounded-xl">
                                <button
                                    type="button"
                                    onClick={() => setActiveMenuTab('settings')}
                                    className={`px-3 py-1.5 rounded-lg text-xs font-bold flex items-center gap-1.5 transition-all ${
                                        activeMenuTab === 'settings'
                                            ? 'bg-white dark:bg-base-100 text-blue-600 dark:text-blue-400 shadow-xs'
                                            : 'text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white'
                                    }`}
                                >
                                    <Settings size={14} className={activeMenuTab === 'settings' ? 'text-blue-600 dark:text-blue-400' : 'text-gray-400'} />
                                    {t('proxy.config.title')}
                                </button>
                                <button
                                    type="button"
                                    onClick={() => setActiveMenuTab('models')}
                                    className={`px-3 py-1.5 rounded-lg text-xs font-bold flex items-center gap-1.5 transition-all ${
                                        activeMenuTab === 'models'
                                            ? 'bg-white dark:bg-base-100 text-blue-600 dark:text-blue-400 shadow-xs'
                                            : 'text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white'
                                    }`}
                                >
                                    <BrainCircuit size={14} className={activeMenuTab === 'models' ? 'text-blue-600 dark:text-blue-400' : 'text-gray-400'} />
                                    {t('proxy.model_config.title', { defaultValue: '模型配置' })}
                                </button>
                                <button
                                    type="button"
                                    onClick={() => setActiveMenuTab('protocols')}
                                    className={`px-3 py-1.5 rounded-lg text-xs font-bold flex items-center gap-1.5 transition-all ${
                                        activeMenuTab === 'protocols'
                                            ? 'bg-white dark:bg-base-100 text-blue-600 dark:text-blue-400 shadow-xs'
                                            : 'text-gray-600 dark:text-gray-400 hover:text-gray-900 dark:hover:text-white'
                                    }`}
                                >
                                    <Share2 size={14} className={activeMenuTab === 'protocols' ? 'text-blue-600 dark:text-blue-400' : 'text-gray-400'} />
                                    {t('proxy.multi_protocol.title')}
                                </button>
                            </div>

                            {/* 服务操作不与模型目录的即时保存混用 */}
                            {activeMenuTab === 'settings' && <div className="flex items-center gap-2.5 ml-auto">
                                <button onClick={handleSaveProxySettings} disabled={!appConfig}
                                    className="px-3 py-1.5 rounded-lg text-xs font-semibold transition-colors flex items-center gap-1.5 bg-blue-600 hover:bg-blue-700 text-white shadow-xs focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blue-500">
                                    <Save size={14} />{t('settings.save')}
                                </button>
                                <button onClick={handleToggle} disabled={loading || !appConfig}
                                    className={`px-3 py-1.5 rounded-lg text-xs font-semibold transition-colors flex items-center gap-1.5 shadow-xs focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-blue-500 ${status.running ? 'bg-rose-600 hover:bg-rose-700 text-white' : 'bg-emerald-600 hover:bg-emerald-700 text-white'} ${(loading || !appConfig) ? 'opacity-50 cursor-not-allowed' : ''}`}>
                                    <Power size={14} />{loading ? t('proxy.status.processing') : (status.running ? t('proxy.action.stop') : t('proxy.action.start'))}
                                </button>
                            </div>}
                        </div>

                        {/* TAB 1: 服务配置 (settings) */}
                        {activeMenuTab === 'settings' && (
                            <div className="p-4 space-y-4">
                            {/* 监听端口、超时和自启动 */}
                            <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
                                <div>
                                    <label className="block text-xs font-semibold text-gray-700 dark:text-gray-200 mb-1.5">
                                        <span className="inline-flex items-center gap-1">
                                            {t('proxy.config.port')}
                                            <HelpTooltip
                                                text={t('proxy.config.port_tooltip')}
                                                ariaLabel={t('proxy.config.port')}
                                                placement="right"
                                            />
                                        </span>
                                    </label>
                                    <input
                                        type="number"
                                        value={appConfig.proxy.port}
                                        onChange={(e) => updateProxyConfig({ port: parseInt(e.target.value) })}
                                        min={8000}
                                        max={65535}
                                        disabled={status.running}
                                        className="w-full px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 text-xs font-mono text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500 disabled:opacity-50 disabled:cursor-not-allowed"
                                    />
                                    <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">
                                        {t('proxy.config.port_hint')}
                                    </p>

                                    {/* 监听端口下方空位：服务运行状态指示卡片 */}
                                    <div className="mt-2.5 p-2 rounded-lg border border-gray-200/80 dark:border-base-300/80 bg-gray-50/70 dark:bg-base-200/50 flex items-center justify-between">
                                        <div className="flex items-center gap-2">
                                            <div className={`w-2.5 h-2.5 rounded-full ${status.running ? 'bg-emerald-500 shadow-xs shadow-emerald-500/50 animate-pulse' : 'bg-gray-400 dark:bg-gray-500'}`} />
                                            <span className={`text-xs font-semibold ${status.running ? 'text-emerald-600 dark:text-emerald-400' : 'text-gray-500 dark:text-gray-400'}`}>
                                                {status.running
                                                    ? `${t('proxy.status.running')} (${status.active_accounts} ${t('common.accounts')})`
                                                    : t('proxy.status.stopped')}
                                            </span>
                                        </div>
                                        {status.running && (
                                            <span className="text-[10px] font-mono text-gray-400 dark:text-gray-500 truncate max-w-[130px]" title={status.base_url || `http://127.0.0.1:${appConfig.proxy.port || 8045}`}>
                                                {status.base_url || `http://127.0.0.1:${appConfig.proxy.port || 8045}`}
                                            </span>
                                        )}
                                    </div>
                                </div>
                                <div>
                                    <label className="block text-xs font-semibold text-gray-700 dark:text-gray-200 mb-1.5">
                                        <span className="inline-flex items-center gap-1">
                                            {t('proxy.config.request_timeout')}
                                            <HelpTooltip
                                                text={t('proxy.config.request_timeout_tooltip')}
                                                ariaLabel={t('proxy.config.request_timeout')}
                                                placement="top"
                                            />
                                        </span>
                                    </label>
                                    <input
                                        type="number"
                                        value={appConfig.proxy.request_timeout || 120}
                                        onChange={(e) => {
                                             const value = parseInt(e.target.value);
                                             const timeout = Math.max(30, Math.min(7200, value));
                                             updateProxyConfig({ request_timeout: timeout });
                                        }}
                                        min={30}
                                        max={7200}
                                        disabled={status.running}
                                        className="w-full px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 text-xs font-mono text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500 disabled:opacity-50 disabled:cursor-not-allowed"
                                    />
                                    <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">
                                        {t('proxy.config.request_timeout_hint')}
                                    </p>
                                </div>
                                <div className="flex items-center pt-5">
                                    <label className="flex items-center cursor-pointer gap-3 select-none">
                                        <input
                                            type="checkbox"
                                            className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600 disabled:opacity-50 disabled:bg-gray-100 dark:disabled:bg-base-200"
                                            checked={appConfig.proxy.auto_start}
                                            onChange={(e) => updateProxyConfig({ auto_start: e.target.checked })}
                                        />
                                        <span className="text-xs font-semibold text-gray-700 dark:text-gray-200 inline-flex items-center gap-1">
                                            {t('proxy.config.auto_start')}
                                            <HelpTooltip
                                                text={t('proxy.config.auto_start_tooltip')}
                                                ariaLabel={t('proxy.config.auto_start')}
                                                placement="right"
                                            />
                                        </span>
                                    </label>
                                </div>
                            </div>


                            {/* 局域网访问 & 访问授权 */}
                            <div className="border-t border-gray-100 dark:border-base-200 pt-3.5 mt-3.5">
                                <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
                                    {/* 允许局域网访问 */}
                                    <div className="space-y-1.5">
                                        <div className="flex items-center justify-between">
                                            <span className="text-xs font-semibold text-gray-700 dark:text-gray-200 inline-flex items-center gap-1">
                                                {t('proxy.config.allow_lan_access')}
                                                <HelpTooltip
                                                    text={t('proxy.config.allow_lan_access_tooltip')}
                                                    ariaLabel={t('proxy.config.allow_lan_access')}
                                                    placement="right"
                                                />
                                            </span>
                                            <input
                                                type="checkbox"
                                                className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600"
                                                checked={appConfig.proxy.allow_lan_access || false}
                                                onChange={(e) => updateProxyConfig({ allow_lan_access: e.target.checked })}
                                            />
                                        </div>
                                        <p className="text-xs text-gray-500 dark:text-gray-400">
                                            {(appConfig.proxy.allow_lan_access || false)
                                                ? t('proxy.config.allow_lan_access_hint_enabled')
                                                : t('proxy.config.allow_lan_access_hint_disabled')}
                                        </p>
                                        {(appConfig.proxy.allow_lan_access || false) && (
                                            <p className="text-xs text-amber-600 dark:text-amber-400 font-medium">
                                                {t('proxy.config.allow_lan_access_warning')}
                                            </p>
                                        )}
                                        {status.running && (
                                            <p className="text-xs text-blue-600 dark:text-blue-400 font-medium">
                                                {t('proxy.config.allow_lan_access_restart_hint')}
                                            </p>
                                        )}
                                    </div>

                                    {/* 访问授权 */}
                                    <div className="space-y-1.5">
                                        <div className="flex items-center justify-between">
                                            <label className="text-xs font-semibold text-gray-700 dark:text-gray-200">
                                                <span className="inline-flex items-center gap-1">
                                                    {t('proxy.config.auth.title')}
                                                    <HelpTooltip
                                                        text={t('proxy.config.auth.title_tooltip')}
                                                        ariaLabel={t('proxy.config.auth.title')}
                                                        placement="top"
                                                    />
                                                </span>
                                            </label>
                                            <label className="flex items-center cursor-pointer gap-2">
                                                <span className="text-xs text-gray-600 dark:text-gray-300 inline-flex items-center gap-1">
                                                    {(appConfig.proxy.auth_mode || 'off') !== 'off' ? t('proxy.config.auth.enabled') : t('common.disabled')}
                                                    <HelpTooltip
                                                        text={t('proxy.config.auth.enabled_tooltip')}
                                                        ariaLabel={t('proxy.config.auth.enabled')}
                                                        placement="left"
                                                    />
                                                </span>
                                                <input
                                                    type="checkbox"
                                                    className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600 disabled:opacity-50 disabled:bg-gray-100 dark:disabled:bg-base-200"
                                                    checked={(appConfig.proxy.auth_mode || 'off') !== 'off'}
                                                    onChange={(e) => {
                                                        const nextMode = e.target.checked ? 'all_except_health' : 'off';
                                                        updateProxyConfig({ auth_mode: nextMode });
                                                    }}
                                                />
                                            </label>
                                        </div>

                                        <div>
                                            <label className="block text-xs text-gray-600 dark:text-gray-400 mb-1">
                                                <span className="inline-flex items-center gap-1">
                                                    {t('proxy.config.auth.mode')}
                                                    <HelpTooltip
                                                        text={t('proxy.config.auth.mode_tooltip')}
                                                        ariaLabel={t('proxy.config.auth.mode')}
                                                        placement="top"
                                                    />
                                                </span>
                                            </label>
                                            <select
                                                value={appConfig.proxy.auth_mode || 'off'}
                                                onChange={(e) =>
                                                    updateProxyConfig({
                                                        auth_mode: e.target.value as ProxyConfig['auth_mode'],
                                                    })
                                                }
                                                className="w-full px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 text-xs text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500"
                                            >
                                                <option value="off">{t('proxy.config.auth.modes.off')}</option>
                                                <option value="strict">{t('proxy.config.auth.modes.strict')}</option>
                                                <option value="all_except_health">{t('proxy.config.auth.modes.all_except_health')}</option>
                                                <option value="auto">{t('proxy.config.auth.modes.auto')}</option>
                                            </select>
                                            <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">
                                                {t('proxy.config.auth.hint')}
                                            </p>
                                        </div>
                                    </div>
                                </div>
                            </div>

                            {/* API 密钥 */}
                            <div className="border-t border-gray-100 dark:border-base-200 pt-3.5 mt-3.5">
                                <label className="block text-xs font-semibold text-gray-700 dark:text-gray-200 mb-1.5">
                                    <span className="inline-flex items-center gap-1">
                                        {t('proxy.config.api_key')}
                                        <HelpTooltip
                                            text={t('proxy.config.api_key_tooltip')}
                                            ariaLabel={t('proxy.config.api_key')}
                                            placement="right"
                                        />
                                    </span>
                                </label>
                                <div className="flex gap-2">
                                    <input
                                        type="text"
                                        value={isEditingApiKey ? tempApiKey : (appConfig.proxy.api_key)}
                                        onChange={(e) => isEditingApiKey && setTempApiKey(e.target.value)}
                                        readOnly={!isEditingApiKey}
                                        className={`flex-1 px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg text-xs font-mono transition-colors ${isEditingApiKey
                                            ? 'bg-white dark:bg-base-100 text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500'
                                            : 'bg-gray-50 dark:bg-base-200 text-gray-700 dark:text-gray-300'
                                            }`}
                                    />
                                    {isEditingApiKey ? (
                                        <>
                                            <button
                                                onClick={handleSaveApiKey}
                                                className="px-2.5 py-1.5 border border-emerald-300 dark:border-emerald-700 rounded-lg bg-emerald-50 dark:bg-emerald-950/50 hover:bg-emerald-100 dark:hover:bg-emerald-900/50 transition-colors text-emerald-600 dark:text-emerald-400 shadow-2xs"
                                                title={t('proxy.config.btn_save')}
                                            >
                                                <CheckCircle size={15} />
                                            </button>
                                            <button
                                                onClick={handleCancelEditApiKey}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('common.cancel')}
                                            >
                                                <X size={15} />
                                            </button>
                                        </>
                                    ) : (
                                        <>
                                            <button
                                                onClick={handleEditApiKey}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('proxy.config.btn_edit')}
                                            >
                                                <Edit2 size={15} />
                                            </button>
                                            <button
                                                onClick={handleGenerateApiKey}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('proxy.config.btn_regenerate')}
                                            >
                                                <RefreshCw size={15} />
                                            </button>
                                            <button
                                                onClick={() => copyToClipboardHandler(appConfig.proxy.api_key, 'api_key')}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('proxy.config.btn_copy')}
                                            >
                                                {copied === 'api_key' ? (
                                                    <CheckCircle size={15} className="text-emerald-500" />
                                                ) : (
                                                    <Copy size={15} />
                                                )}
                                            </button>
                                        </>
                                    )}
                                </div>
                                <p className="mt-1 text-xs text-amber-600 dark:text-amber-400 font-medium">
                                    {t('proxy.config.warning_key')}
                                </p>
                            </div>

                            {/* Web UI 管理密码 */}
                            <div className="border-t border-gray-100 dark:border-base-200 pt-3.5 mt-3.5">
                                <label className="block text-xs font-semibold text-gray-700 dark:text-gray-200 mb-1.5">
                                    <span className="inline-flex items-center gap-1">
                                        {t('proxy.config.admin_password', { defaultValue: 'Web UI Login Password' })}
                                        <HelpTooltip
                                            text={t('proxy.config.admin_password_tooltip', { defaultValue: 'Used for logging into the Web Management Console. If empty, it defaults to the API Key.' })}
                                            ariaLabel={t('proxy.config.admin_password')}
                                            placement="right"
                                        />
                                    </span>
                                </label>
                                <div className="flex gap-2">
                                    <input
                                        type="text"
                                        value={isEditingAdminPassword ? tempAdminPassword : (appConfig.proxy.admin_password || t('proxy.config.admin_password_default', { defaultValue: '(Same as API Key)' }))}
                                        onChange={(e) => isEditingAdminPassword && setTempAdminPassword(e.target.value)}
                                        readOnly={!isEditingAdminPassword}
                                        placeholder={t('proxy.config.admin_password_placeholder', { defaultValue: 'Enter new password or leave empty to use API Key' })}
                                        className={`flex-1 px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg text-xs font-mono transition-colors ${isEditingAdminPassword
                                            ? 'bg-white dark:bg-base-100 text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500'
                                            : 'bg-gray-50 dark:bg-base-200 text-gray-700 dark:text-gray-300'
                                            }`}
                                    />
                                    {isEditingAdminPassword ? (
                                        <>
                                            <button
                                                onClick={handleSaveAdminPassword}
                                                className="px-2.5 py-1.5 border border-emerald-300 dark:border-emerald-700 rounded-lg bg-emerald-50 dark:bg-emerald-950/50 hover:bg-emerald-100 dark:hover:bg-emerald-900/50 transition-colors text-emerald-600 dark:text-emerald-400 shadow-2xs"
                                                title={t('proxy.config.btn_save')}
                                            >
                                                <CheckCircle size={15} />
                                            </button>
                                            <button
                                                onClick={handleCancelEditAdminPassword}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('common.cancel')}
                                            >
                                                <X size={15} />
                                            </button>
                                        </>
                                    ) : (
                                        <>
                                            <button
                                                onClick={handleEditAdminPassword}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('proxy.config.btn_edit')}
                                            >
                                                <Edit2 size={15} />
                                            </button>
                                            <button
                                                onClick={() => copyToClipboardHandler(appConfig.proxy.admin_password || appConfig.proxy.api_key, 'admin_password')}
                                                className="px-2.5 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 hover:bg-gray-50 dark:hover:bg-base-300 transition-colors text-gray-600 dark:text-gray-300 shadow-2xs"
                                                title={t('proxy.config.btn_copy')}
                                            >
                                                {copied === 'admin_password' ? (
                                                    <CheckCircle size={15} className="text-emerald-500" />
                                                ) : (
                                                    <Copy size={15} />
                                                )}
                                            </button>
                                        </>
                                    )}
                                </div>
                                <p className="mt-1 text-xs text-gray-500 dark:text-gray-400">
                                    {t('proxy.config.admin_password_hint', { defaultValue: 'For safety in Docker/Browser environments, you can set a separate login password from your API Key.' })}
                                </p>
                            </div>

                            {/* User-Agent Overrides */}
                            <div className="border-t border-gray-100 dark:border-base-200 pt-3.5 mt-3.5">
                                <div className="flex items-center justify-between mb-2">
                                    <label className="text-xs font-semibold text-gray-700 dark:text-gray-200 inline-flex items-center gap-1">
                                        {t('proxy.config.request.user_agent', { defaultValue: 'User-Agent Override' })}
                                        <HelpTooltip text={t('proxy.config.request.user_agent_tooltip', { defaultValue: 'Override the User-Agent header sent to upstream APIs.' })} />
                                    </label>
                                    <input
                                        type="checkbox"
                                        className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600"
                                        checked={!!appConfig.proxy.user_agent_override}
                                        onChange={(e) => {
                                            const enabled = e.target.checked;
                                            if (enabled) {
                                                // Restore saved override from config or use modern default
                                                const defaultUA = 'antigravity/4.3.0 darwin/arm64';
                                                let restoredValue = appConfig.proxy.saved_user_agent || defaultUA;
                                                const match = restoredValue.match(/(antigravity[/v\s]+)(\d+(?:\.\d+)+)/i);
                                                if (match) {
                                                    const parts = match[2].split('.').map(Number);
                                                    const major = parts[0] || 0;
                                                    const minor = parts[1] || 0;
                                                    if (major < 4 || (major === 4 && minor < 3)) {
                                                        restoredValue = restoredValue.replace(match[0], `${match[1]}4.3.0`);
                                                    }
                                                } else if (restoredValue.includes('1.15.8')) {
                                                    restoredValue = restoredValue.replace('1.15.8', '4.3.0');
                                                }
                                                updateProxyConfig({
                                                    user_agent_override: restoredValue,
                                                    saved_user_agent: restoredValue
                                                });
                                            } else {
                                                // Disable active override but keep saved value
                                                updateProxyConfig({ user_agent_override: undefined });
                                            }
                                        }}
                                    />
                                </div>

                                {!!appConfig.proxy.user_agent_override && (
                                    <div className="space-y-2 animate-in fade-in slide-in-from-top-1 duration-200">
                                        <input
                                            type="text"
                                            value={appConfig.proxy.user_agent_override}
                                            onChange={(e) => {
                                                const newValue = e.target.value;
                                                updateProxyConfig({
                                                    user_agent_override: newValue,
                                                    saved_user_agent: newValue
                                                });
                                            }}
                                            className="w-full px-3 py-1.5 border border-gray-300 dark:border-base-300 rounded-lg bg-white dark:bg-base-200 text-xs font-mono text-gray-900 dark:text-base-content focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500"
                                            placeholder={t('proxy.config.request.user_agent_placeholder', { defaultValue: 'Enter custom User-Agent string...' })}
                                        />
                                        <div className="bg-gray-50 dark:bg-base-200 border border-gray-200 dark:border-base-300 rounded-lg p-2.5 text-xs text-gray-600 dark:text-gray-400 font-mono break-all">
                                            <span className="font-bold text-gray-700 dark:text-gray-300 select-none mr-2">{t('common.example', { defaultValue: 'Example' })}:</span>
                                            antigravity/4.3.0 darwin/arm64
                                        </div>
                                    </div>
                                )}
                            </div>


                            </div>
                        )}

                        {/* TAB: 模型配置 (models) */}
                        {activeMenuTab === 'models' && (
                            <div className="p-4 min-w-0">
                                <ModelRegistryPanel initialTab={showWorkBuddy ? 'advanced' : undefined} advancedContent={<div className="space-y-3">
                                    <CollapsibleCard title="来源配置 / Source configuration" defaultExpanded={showWorkBuddy} icon={<Settings size={18} className="text-blue-500" />}>
                                        <SourceSettings />
                                    </CollapsibleCard>
                                    <div className="flex flex-wrap items-center justify-between gap-3 pt-3">
                                        <div><h3 className="text-sm font-semibold">AG 高级设置 / Advanced settings</h3><p className="text-xs text-gray-500 dark:text-gray-400">{advancedDirty ? '草稿尚未保存 / Unsaved draft' : '修改后请保存；与来源配置独立保存 / Save changes separately from source settings'}</p></div>
                                        <button type="button" className="registry-btn registry-primary" disabled={advancedSaving || !advancedDirty} onClick={() => void handleSaveAdvancedSettings().catch(() => {})}><Save size={14} aria-hidden />{advancedSaving ? '保存中 / Saving…' : '保存高级设置 / Save advanced'}</button>
                                    </div>
                                    {advancedFeedback && <p role={advancedFeedback.error ? 'alert' : 'status'} className={`registry-notice ${advancedFeedback.error ? 'registry-notice-error' : ''}`}>{advancedFeedback.message}</p>}
                                    <fieldset disabled={advancedSaving} className="min-w-0 space-y-3" aria-busy={advancedSaving}>
                                        <CollapsibleCard title="Antigravity · 思考预算与缓存 / Thinking budget & cache" icon={<BrainCircuit size={18} className="text-purple-500" />}>
                                            <p className="registry-muted mb-4">仅 AG 思考参数和缓存；实际推理能力取决于模型与请求。WB 尚未接入，不能重放 AG 思考签名。 / AG thinking pipeline only; model support required. Not integrated with WB; never replay AG signatures to WB.</p>
                                            <ThinkingBudget config={appConfig.proxy.thinking_budget} onChange={config => updateAdvancedProxyConfig({ thinking_budget: config })} onSave={handleSaveAdvancedSettings}
                                                thinkingStoreEnabled={appConfig.proxy.experimental?.thinking_store_enabled !== false} onThinkingStoreChange={enabled => updateExperimentalConfig({ thinking_store_enabled: enabled })}
                                                thinkingMaxMemoryTurns={appConfig.proxy.experimental?.thinking_max_memory_turns ?? 600} onThinkingMaxMemoryTurnsChange={turns => updateExperimentalConfig({ thinking_max_memory_turns: turns })}
                                                thinkingRetentionDays={appConfig.proxy.experimental?.thinking_retention_days ?? 15} onThinkingRetentionDaysChange={days => updateExperimentalConfig({ thinking_retention_days: days })} />
                                        </CollapsibleCard>
                                        <CollapsibleCard title="Antigravity · 多模态交互 / Multimodal" icon={<Sparkles size={18} className="text-pink-500" />}>
                                            <p className="registry-muted mb-4">AG 多模态处理链路；图片、音频或视频依赖目标模型和请求类型。WB 对应网关处理尚未接入，不代表原生不支持。 / AG pipeline only; modalities depend on the target. Equivalent WB gateway processing is not integrated, not proof of native absence.</p>
                                            <MultimodalSettings config={appConfig.proxy.multimodal} onChange={config => updateAdvancedProxyConfig({ multimodal: config })} onSave={handleSaveAdvancedSettings} />
                                        </CollapsibleCard>
                                        <CollapsibleCard title="Antigravity · Agent 与客户端 / Agent & client" icon={<Bot size={18} className="text-cyan-500" />}>
                                            <p className="registry-muted mb-4">压缩仅针对服务端识别的 Cowork 会话与 AG 请求路径；WB 尚未接入。 / Compression applies to recognized Cowork traffic on the AG path; WB not integrated.</p>
                                            {isTauri() ? <AgentSettings experimentalConfig={appConfig.proxy.experimental} onChange={updateExperimentalConfig} onSave={handleSaveAdvancedSettings} /> : <ServerAgentSettings config={appConfig.proxy.experimental} onChange={updateExperimentalConfig} />}
                                        </CollapsibleCard>
                                        <CollapsibleCard title="Antigravity · 图像思考与全局提示词 / Image thinking & global prompt" icon={<Sparkles size={18} className="text-pink-500" />}>
                                            <p className="registry-muted mb-4">「全局」指 AG 链路适用请求，不会自动作用于 WB。图像思考需目标图像模型和请求路径支持，不证明原生能力。 / Global within applicable AG requests, not WB. Image thinking requires a compatible image target and request path; not capability evidence.</p>
                                            <div className="space-y-4">
                                                <ImageThinkingMode value={appConfig.proxy.image_thinking_mode || 'enabled'} onChange={value => updateAdvancedProxyConfig({ image_thinking_mode: value })} />
                                                <div className="pt-4 border-t border-gray-100 dark:border-base-300"><GlobalSystemPrompt config={appConfig.proxy.global_system_prompt || { enabled: false, content: '' }} onChange={config => updateAdvancedProxyConfig({ global_system_prompt: config })} /></div>
                                            </div>
                                        </CollapsibleCard>
                                    </fieldset>
                                </div>} />
                            </div>
                        )}

                        {/* TAB 3: 多协议支持 (protocols) */}
                        {activeMenuTab === 'protocols' && (
                            <div className="p-4 space-y-6">
                                {renderMultiProtocolSection()}
                                {renderSupportedModelsSection()}
                            </div>
                        )}
                    </div>
                )}

                {/* External Providers Integration - 仅在服务配置 Tab 下展示 */}
                {
                    !configLoading && !configError && appConfig && activeMenuTab === 'settings' && (
                        <div className="space-y-4">
                            {/* Account Scheduling & Rotation */}
                            <CollapsibleCard
                                title={t('proxy.config.scheduling.title')}
                                icon={<RefreshCw size={18} className="text-indigo-500" />}
                            >
                                <div className="space-y-4">
                                    <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                                        <div className="space-y-3">
                                            <div className="flex items-center justify-between">
                                                <label className="text-xs font-medium text-gray-700 dark:text-gray-300 inline-flex items-center gap-1">
                                                    {t('proxy.config.scheduling.mode')}
                                                    <HelpTooltip
                                                        text={t('proxy.config.scheduling.mode_tooltip')}
                                                        placement="right"
                                                    />
                                                </label>
                                                <div className="flex items-center gap-3">
                                                    {/* [MOVED] Clear Rate Limit button moved to CircuitBreaker component */}
                                                    <button
                                                        onClick={handleClearSessionBindings}
                                                        className="text-[10px] text-indigo-500 hover:text-indigo-600 transition-colors flex items-center gap-1"
                                                        title={t('proxy.config.scheduling.clear_bindings_tooltip')}
                                                    >
                                                        <Trash2 size={12} />
                                                        {t('proxy.config.scheduling.clear_bindings')}
                                                    </button>
                                                </div>
                                            </div>
                                            <div className="grid grid-cols-1 gap-2">
                                                {(['CacheFirst', 'Balance', 'PerformanceFirst'] as const).map(mode => (
                                                    <label
                                                        key={mode}
                                                        className={`flex items-start gap-3 p-3 rounded-xl border cursor-pointer transition-all duration-200 ${(appConfig.proxy.scheduling?.mode || 'Balance') === mode
                                                            ? 'border-indigo-500 bg-indigo-50/30 dark:bg-indigo-900/10'
                                                            : 'border-gray-100 dark:border-base-200 hover:border-indigo-200'
                                                            }`}
                                                    >
                                                        <input
                                                            type="radio"
                                                            className="radio radio-xs radio-primary mt-1"
                                                            checked={(appConfig.proxy.scheduling?.mode || 'Balance') === mode}
                                                            onChange={() => updateSchedulingConfig({ mode })}
                                                        />
                                                        <div className="space-y-1">
                                                            <div className="text-xs font-bold text-gray-900 dark:text-base-content">
                                                                {t(`proxy.config.scheduling.modes.${mode}`)}
                                                            </div>
                                                            <div className="text-[10px] text-gray-500 line-clamp-2">
                                                                {t(`proxy.config.scheduling.modes_desc.${mode}`, {
                                                                    defaultValue: mode === 'CacheFirst' ? 'Binds session to account, waits precisely if limited (Maximizes Prompt Cache hits).' :
                                                                        mode === 'Balance' ? 'Binds session, auto-switches to available account if limited (Balanced cache & availability).' :
                                                                            'No session binding, pure round-robin rotation (Best for high concurrency).'
                                                                })}
                                                            </div>
                                                        </div>
                                                    </label>
                                                ))}
                                            </div>
                                        </div>

                                        <div className="space-y-4 pt-1">
                                            <div className="bg-gray-50/70 dark:bg-base-200 rounded-xl p-4 border border-gray-100 dark:border-base-300">
                                                <div className="flex items-center justify-between mb-2">
                                                    <label className="text-xs font-medium text-gray-700 dark:text-gray-300 inline-flex items-center gap-1">
                                                        {t('proxy.config.scheduling.max_wait')}
                                                        <HelpTooltip text={t('proxy.config.scheduling.max_wait_tooltip')} />
                                                    </label>
                                                    <span className="text-xs font-mono text-indigo-600 font-bold">
                                                        {appConfig.proxy.scheduling?.max_wait_seconds ?? 60}s
                                                    </span>
                                                </div>
                                                <input
                                                    type="range"
                                                    min="0"
                                                    max="300"
                                                    step="10"
                                                    disabled={(appConfig.proxy.scheduling?.mode || 'Balance') !== 'CacheFirst'}
                                                    className="range range-indigo range-xs"
                                                    value={appConfig.proxy.scheduling?.max_wait_seconds ?? 60}
                                                    onChange={(e) => updateSchedulingConfig({ max_wait_seconds: parseInt(e.target.value) })}
                                                />
                                                <div className="flex justify-between px-1 mt-1 text-[10px] text-gray-400 font-mono">
                                                    <span>0s</span>
                                                    <span>300s</span>
                                                </div>
                                            </div>

                                            <div className="p-3 bg-amber-50 dark:bg-amber-900/10 border border-amber-100 dark:border-amber-900/20 rounded-xl">
                                                <p className="text-[10px] text-amber-700 dark:text-amber-500 leading-relaxed">
                                                    <strong>{t('common.info')}:</strong> {t('proxy.config.scheduling.subtitle')}
                                                </p>
                                            </div>

                                            {/* [FIX #820] Fixed Account Mode */}
                                            <div className="bg-indigo-50 dark:bg-indigo-900/20 rounded-xl p-4 border border-indigo-200 dark:border-indigo-800">
                                                <div className="flex items-center justify-between mb-3">
                                                    <label className="text-xs font-medium text-gray-700 dark:text-gray-300 inline-flex items-center gap-1">
                                                        🔒 Antigravity · {t('proxy.config.scheduling.fixed_account', { defaultValue: 'Fixed Account Mode' })}
                                                        <HelpTooltip text={t('proxy.config.scheduling.fixed_account_tooltip', { defaultValue: 'When enabled, all API requests will use only the selected account instead of rotating between accounts.' })} />
                                                    </label>
                                                    <input
                                                        type="checkbox"
                                                        className="toggle toggle-sm toggle-primary"
                                                        checked={preferredAccountId !== null}
                                                        onChange={(e) => {
                                                            if (e.target.checked) {
                                                                // Enable fixed mode with first available account
                                                                if (availableAccounts.length > 0) {
                                                                    handleSetPreferredAccount(availableAccounts[0].id);
                                                                }
                                                            } else {
                                                                // Disable fixed mode
                                                                handleSetPreferredAccount(null);
                                                            }
                                                        }}
                                                        disabled={!status.running}
                                                    />
                                                </div>
                                                {preferredAccountId !== null && (
                                                    <select
                                                        className="select select-bordered select-sm w-full text-xs"
                                                        value={preferredAccountId || ''}
                                                        onChange={(e) => handleSetPreferredAccount(e.target.value || null)}
                                                        disabled={!status.running}
                                                    >
                                                        {availableAccounts.map(account => (
                                                            <option key={account.id} value={account.id}>
                                                                {account.email}
                                                            </option>
                                                        ))}
                                                    </select>
                                                )}
                                                {!status.running && (
                                                    <p className="text-[10px] text-gray-500 mt-2">
                                                        {t('proxy.config.scheduling.start_proxy_first', { defaultValue: 'Start the proxy service to configure fixed account mode.' })}
                                                    </p>
                                                )}
                                            </div>
                                        </div>
                                    </div>

                                    {/* Circuit Breaker Section */}
                                    {appConfig.circuit_breaker && (
                                        <div className="pt-4 border-t border-gray-100 dark:border-gray-700/50">
                                            <div className="flex items-center justify-between mb-4">
                                                <label className="text-xs font-medium text-gray-700 dark:text-gray-300 inline-flex items-center gap-1">
                                                    {t('proxy.config.circuit_breaker.title', { defaultValue: 'Adaptive Circuit Breaker' })}
                                                    <HelpTooltip text={t('proxy.config.circuit_breaker.tooltip', { defaultValue: 'Prevent continuous failures by exponentially backing off when quota is exhausted.' })} />
                                                </label>
                                                <input
                                                    type="checkbox"
                                                    className="toggle toggle-sm toggle-warning"
                                                    checked={appConfig.circuit_breaker.enabled}
                                                    onChange={(e) => updateCircuitBreakerConfig({ ...appConfig.circuit_breaker, enabled: e.target.checked })}
                                                />
                                            </div>

                                            {appConfig.circuit_breaker.enabled && (
                                                <CircuitBreaker
                                                    config={appConfig.circuit_breaker}
                                                    onChange={updateCircuitBreakerConfig}
                                                    onClearRateLimits={handleClearRateLimits}
                                                />
                                            )}
                                        </div>
                                    )}
                                </div>
                            </CollapsibleCard>

                        </div>
                    )
                }


                {/* 各种对话框 */}

                <ModalDialog
                    isOpen={isRegenerateKeyConfirmOpen}
                    title={t('proxy.dialog.regenerate_key_title') || t('proxy.dialog.confirm_regenerate')}
                    message={t('proxy.dialog.regenerate_key_msg') || t('proxy.dialog.confirm_regenerate')}
                    type="confirm"
                    isDestructive={true}
                    onConfirm={executeGenerateApiKey}
                    onCancel={() => setIsRegenerateKeyConfirmOpen(false)}
                />

                <ModalDialog
                    isOpen={isClearBindingsConfirmOpen}
                    title={t('proxy.dialog.clear_bindings_title') || '清除会话绑定'}
                    message={t('proxy.dialog.clear_bindings_msg') || '确定要清除所有会话与账号的绑定映射吗？'}
                    type="confirm"
                    isDestructive={true}
                    onConfirm={executeClearSessionBindings}
                    onCancel={() => setIsClearBindingsConfirmOpen(false)}
                />

                <ModalDialog
                    isOpen={isClearRateLimitsConfirmOpen}
                    title={t('proxy.dialog.clear_rate_limits_title') || '清除限流记录'}
                    message={t('proxy.dialog.clear_rate_limits_confirm') || '确定要清除所有本地限流记录吗？'}
                    type="confirm"
                    isDestructive={true}
                    onConfirm={executeClearRateLimits}
                    onCancel={() => setIsClearRateLimitsConfirmOpen(false)}
                />

            </div >
        </div >
    );
}
