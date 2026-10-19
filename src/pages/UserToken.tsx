import React, { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Plus, Trash2, RefreshCw, Copy, Activity, User, Settings, Shield, Clock, Users, Mail, UserCog, Coins } from 'lucide-react';
import { motion, AnimatePresence } from 'framer-motion';
import { request as invoke } from '../utils/request';
import { showToast } from '../components/common/ToastContainer';
import ModelAccessFields from '../components/user-token/ModelAccessFields';
import TokenLimitFields from '../components/user-token/TokenLimitFields';
import { toQuotaMillions, validMaxConcurrency } from '../components/user-token/tokenQuota';
import { fromDateTimeLocal, nowSeconds, toDateTimeLocal } from '../components/user-token/tokenExpiry';
import { copyToClipboard } from '../utils/clipboard';
import ModalDialog from '../components/common/ModalDialog';
import PortalSettingsModal from '../components/user-token/PortalSettingsModal';
import ModelPricingModal from '../components/user-token/ModelPricingModal';
import BalanceFields from '../components/user-token/BalanceFields';
import { formatYuan, parseYuan, toYuan } from '../components/user-token/money';

interface UserToken {
    id: string;
    token: string;
    username: string;
    description?: string;
    enabled: boolean;
    expires_type: string;
    expires_at?: number;
    max_ips: number;
    max_concurrency: number;
    free_models: string[];
    curfew_start?: string;
    curfew_end?: string;
    created_at: number;
    updated_at: number;
    last_used_at?: number;
    total_requests: number;
    total_tokens_used: number;
    owner_user_id?: string | null;
    /** Prepaid balance in micro-yuan; legacy null values are displayed as zero. */
    balance_micros?: number | null;
    /** "admin" = created here; "registered" = self-registered usage-portal account. */
    source?: 'admin' | 'registered';
    owner_email?: string | null;
    account_status?: string | null;
}

interface UserTokenStats {
    total_tokens: number;
    active_tokens: number;
    total_users: number;
    today_requests: number;
    registered_tokens?: number;
}

const tokenFilters = ['all', 'admin', 'registered'] as const;
type TokenFilter = typeof tokenFilters[number];
const isRegistered = (token: UserToken) => token.source === 'registered';

// interface CreateTokenRequest omitted as it's not explicitly used for typing variables

const UserToken: React.FC = () => {
    const { t } = useTranslation();
    const [tokens, setTokens] = useState<UserToken[]>([]);
    const [stats, setStats] = useState<UserTokenStats | null>(null);
    const [modelChoices, setModelChoices] = useState<string[]>([]);
    const [modelLoadError, setModelLoadError] = useState(false);
    const [loading, setLoading] = useState(false);
    const [showCreateModal, setShowCreateModal] = useState(false);
    const [creating, setCreating] = useState(false);
    const [filter, setFilter] = useState<TokenFilter>('all');
    const [showPortalSettings, setShowPortalSettings] = useState(false);
    const [showPricing, setShowPricing] = useState(false);
    const [deleting, setDeleting] = useState<UserToken | null>(null);

    // Edit State
    const [showEditModal, setShowEditModal] = useState(false);
    const [editingToken, setEditingToken] = useState<UserToken | null>(null);
    const [editUsername, setEditUsername] = useState('');
    const [editDesc, setEditDesc] = useState('');
    const [editMaxIps, setEditMaxIps] = useState(0);
    const [editMaxConcurrency, setEditMaxConcurrency] = useState('0');
    const [editFreeModels, setEditFreeModels] = useState<string[]>([]);
    const [editCurfewStart, setEditCurfewStart] = useState('');
    const [editCurfewEnd, setEditCurfewEnd] = useState('');
    const [editExpiresType, setEditExpiresType] = useState('keep'); // keep, day, week, month, never, custom
    const [editCustomExpires, setEditCustomExpires] = useState(''); // datetime-local value
    const [editBalance, setEditBalance] = useState('0');
    const [updating, setUpdating] = useState(false);

    // Create Form State
    const [newUsername, setNewUsername] = useState('');
    const [newDesc, setNewDesc] = useState('');
    const [newExpiresType, setNewExpiresType] = useState('month'); // day, week, month, never, custom
    const [newMaxIps, setNewMaxIps] = useState(0);
    const [newMaxConcurrency, setNewMaxConcurrency] = useState('0');
    const [newFreeModels, setNewFreeModels] = useState<string[]>([]);
    const [newCurfewStart, setNewCurfewStart] = useState('');
    const [newCurfewEnd, setNewCurfewEnd] = useState('');
    const [newCustomExpires, setNewCustomExpires] = useState(''); // datetime-local value
    const [newBalance, setNewBalance] = useState('0');

    const loadData = async () => {
        setLoading(true);
        try {
            const [tokensData, statsData] = await Promise.all([
                invoke<UserToken[]>('list_user_tokens'),
                invoke<UserTokenStats>('get_user_token_summary')
            ]);
            setTokens(tokensData);
            setStats(statsData);
        } catch (e) {
            console.error('Failed to load user tokens', e);
            showToast(t('common.load_failed') || 'Failed to load data', 'error');
        } finally {
            setLoading(false);
        }
    };

    useEffect(() => {
        loadData();
        invoke<string[]>('get_user_token_models').then(setModelChoices).catch(() => setModelLoadError(true));
    }, []);

    const handleCreate = async () => {
        if (!newUsername) {
            showToast(t('user_token.username_required') || 'Username is required', 'error');
            return;
        }

        // 验证自定义时间
        if (newExpiresType === 'custom' && !newCustomExpires) {
            showToast(t('user_token.custom_expires_required') || 'Please select a custom expiration time', 'error');
            return;
        }
        if (newExpiresType === 'custom' && fromDateTimeLocal(newCustomExpires) <= nowSeconds()) {
            showToast(t('user_token.custom_expires_future'), 'error');
            return;
        }

        if (!validMaxConcurrency(newMaxConcurrency)) {
            showToast(t('user_token.invalid_concurrency'), 'error');
            return;
        }
        const newBalanceMicros = parseYuan(newBalance);
        if (newBalanceMicros === null || newBalanceMicros > 1_000_000_000_000_000) {
            showToast(t('billing.invalid_balance'), 'error');
            return;
        }

        setCreating(true);
        try {
            // 计算自定义过期时间戳
            const customExpiresAt = newExpiresType === 'custom' && newCustomExpires
                ? fromDateTimeLocal(newCustomExpires)
                : undefined;

            await invoke('create_user_token', {
                request: {
                    username: newUsername,
                    expires_type: newExpiresType,
                    description: newDesc || null,
                    max_ips: newMaxIps,
                    max_concurrency: Number(newMaxConcurrency),
                    free_models: newFreeModels,
                    curfew_start: newCurfewStart || null,
                    curfew_end: newCurfewEnd || null,
                    custom_expires_at: customExpiresAt || null,
                    balance_micros: newBalanceMicros
                }
            });
            showToast(t('common.create_success') || 'Created successfully', 'success');
            setShowCreateModal(false);
            setNewUsername('');
            setNewDesc('');
            setNewExpiresType('month');
            setNewMaxIps(0);
            setNewMaxConcurrency('0');
            setNewFreeModels([]);
            setNewCurfewStart('');
            setNewCurfewEnd('');
            setNewCustomExpires('');
            setNewBalance('0');
            loadData();
        } catch (e) {
            console.error('Failed to create token', e);
            showToast(String(e), 'error');
        } finally {
            setCreating(false);
        }
    };

    const handleDelete = async (id: string) => {
        try {
            await invoke('delete_user_token', { id });
            showToast(t('common.delete_success') || 'Deleted successfully', 'success');
            loadData();
        } catch (e) {
            showToast(String(e), 'error');
        }
    };

    // Registered keys delete their account too, so they ask first.
    const requestDelete = (token: UserToken) => isRegistered(token) ? setDeleting(token) : handleDelete(token.id);

    const accountAction = async (token: UserToken, action: 'enable' | 'disable' | 'revoke' | 'reset') => {
        const userId = token.owner_user_id;
        if (!userId) return;
        try {
            if (action === 'enable' || action === 'disable') {
                await invoke('set_portal_account_enabled', { userId, enabled: action === 'enable' });
            } else if (action === 'revoke') {
                await invoke('revoke_portal_sessions', { userId });
            } else {
                await invoke('send_portal_reset_email', { userId });
            }
            showToast(t(`user_token.account_${action}_done`, { email: token.owner_email ?? '' }), 'success');
            loadData();
        } catch (e) {
            showToast(String(e), 'error');
        }
    };

    const handleEdit = (token: UserToken) => {
        setEditingToken(token);
        setEditUsername(token.username);
        setEditDesc(token.description || '');
        setEditMaxIps(token.max_ips ?? 0);  // 使用 ?? 确保 null/undefined 变为 0
        setEditMaxConcurrency(String(token.max_concurrency ?? 0));
        setEditFreeModels(token.free_models ?? []);
        setEditCurfewStart(token.curfew_start ?? '');
        setEditCurfewEnd(token.curfew_end ?? '');
        setEditExpiresType('keep');
        setEditCustomExpires(token.expires_at && token.expires_at > nowSeconds() ? toDateTimeLocal(token.expires_at) : '');
        setEditBalance(toYuan(token.balance_micros ?? 0));
        setShowEditModal(true);
    };

    const handleUpdate = async () => {
        if (!editingToken) return;
        if (!editUsername) {
            showToast(t('user_token.username_required') || 'Username is required', 'error');
            return;
        }

        if (!validMaxConcurrency(editMaxConcurrency)) {
            showToast(t('user_token.invalid_concurrency'), 'error');
            return;
        }

        if (editExpiresType === 'custom' && !editCustomExpires) {
            showToast(t('user_token.custom_expires_required') || 'Please select a custom expiration time', 'error');
            return;
        }
        if (editExpiresType === 'custom' && fromDateTimeLocal(editCustomExpires) <= nowSeconds()) {
            showToast(t('user_token.custom_expires_future'), 'error');
            return;
        }

        // Only send the balance when it was edited, so charges made while the dialog
        // was open are not overwritten by the value loaded when it opened.
        const originalBalance = editingToken.balance_micros ?? 0;
        const balanceTextChanged = editBalance !== toYuan(originalBalance);
        const editBalanceMicros = balanceTextChanged ? parseYuan(editBalance) : originalBalance;
        const balanceEdited = balanceTextChanged && editBalanceMicros !== originalBalance;
        if (balanceEdited && (editBalanceMicros === null || editBalanceMicros > 1_000_000_000_000_000)) {
            showToast(t('billing.invalid_balance'), 'error');
            return;
        }

        setUpdating(true);
        try {
            await invoke('update_user_token', {
                id: editingToken.id,
                request: {
                    username: editUsername,
                    description: editDesc || undefined,
                    max_ips: editMaxIps,
                    max_concurrency: Number(editMaxConcurrency),
                    free_models: editFreeModels,
                    // 使用双层包装: undefined = 不更新, null = 清空, string = 设置值
                    curfew_start: editCurfewStart === '' ? null : editCurfewStart,
                    curfew_end: editCurfewEnd === '' ? null : editCurfewEnd,
                    // 省略 = 保持当前有效期；day/week/month 从保存时起算
                    ...(editExpiresType !== 'keep' && {
                        expires_type: editExpiresType,
                        custom_expires_at: editExpiresType === 'custom' ? fromDateTimeLocal(editCustomExpires) : null
                    }),
                    // Omission preserves the current balance, including charges and any debt.
                    ...(balanceEdited && { balance_micros: editBalanceMicros })
                }
            });
            showToast(t('common.update_success') || 'Updated successfully', 'success');
            setShowEditModal(false);
            setEditingToken(null);
            loadData();
        } catch (e) {
            console.error('Failed to update token', e);
            showToast(String(e), 'error');
        } finally {
            setUpdating(false);
        }
    };

    const handleRenew = async (id: string, type: string) => {
        try {
            await invoke('renew_user_token', { id, expiresType: type });
            showToast(t('user_token.renew_success') || 'Renewed successfully', 'success');
            loadData();
        } catch (e) {
            showToast(String(e), 'error');
        }
    };

    const handleCopyToken = async (text: string) => {
        const success = await copyToClipboard(text);
        if (success) {
            showToast(t('common.copied') || 'Copied to clipboard', 'success');
        } else {
            showToast(t('common.copy_failed') || 'Failed to copy to clipboard', 'error');
        }
    };

    const formatTime = (ts?: number) => {
        if (!ts) return '-';
        return new Date(ts * 1000).toLocaleString();
    };

    const getExpiresLabel = (type: string) => {
        switch (type) {
            case 'day': return t('user_token.expires_day', { defaultValue: '1 Day' });
            case 'week': return t('user_token.expires_week', { defaultValue: '1 Week' });
            case 'month': return t('user_token.expires_month', { defaultValue: '1 Month' });
            case 'never': return t('user_token.expires_never', { defaultValue: 'Never' });
            case 'custom': return t('user_token.expires_custom', { defaultValue: 'Custom' });
            default: return type;
        }
    };

    // Calculate expiration status style
    const getExpiresStatus = (expiresAt?: number) => {
        if (!expiresAt) return 'text-green-500';
        const now = Date.now() / 1000;
        if (expiresAt < now) return 'text-red-500 font-bold';
        if (expiresAt - now < 86400 * 3) return 'text-orange-500'; // Less than 3 days
        return 'text-green-500';
    };

    const matchesFilter = (token: UserToken, item: TokenFilter) =>
        item === 'all' || (token.source ?? 'admin') === item;
    const visibleTokens = tokens.filter(token => matchesFilter(token, filter));

    return (
        <motion.div
            initial={{ opacity: 0, y: 10 }}
            animate={{ opacity: 1, y: 0 }}
            className="h-full flex flex-col p-5 gap-5 max-w-7xl mx-auto w-full"
        >
            {/* Header */}
            <div className="flex justify-between items-center">
                <h1 className="text-2xl font-bold text-gray-900 dark:text-white flex items-center gap-2">
                    <div className="p-2 bg-purple-50 dark:bg-purple-900/20 rounded-lg">
                        <User className="text-purple-500 w-5 h-5" />
                    </div>
                    {t('user_token.title', { defaultValue: 'User Tokens' })}
                </h1>

                <div className="flex items-center gap-2">
                    <a href="/usage" target="_blank" rel="noopener noreferrer" className="text-sm text-blue-600 px-2">{t('user_token.portal_link')}</a>
                    <button
                        onClick={() => setShowPricing(true)}
                        className="px-3 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm text-gray-600 dark:text-gray-300 flex items-center gap-1.5 transition-colors"
                    >
                        <Coins size={16} />{t('billing.model_pricing')}
                    </button>
                    <button
                        onClick={() => setShowPortalSettings(true)}
                        className="px-3 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm text-gray-600 dark:text-gray-300 flex items-center gap-1.5 transition-colors"
                    >
                        <UserCog size={16} />{t('user_token.portal_settings')}
                    </button>
                    <button
                        onClick={() => loadData()}
                        className={`p-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg transition-colors ${loading ? 'text-blue-500' : 'text-gray-500'}`}
                        title={t('common.refresh') || 'Refresh'}
                    >
                        <RefreshCw size={18} className={loading ? 'animate-spin' : ''} />
                    </button>
                    <button
                        onClick={() => setShowCreateModal(true)}
                        className="px-4 py-2 bg-blue-500 hover:bg-blue-600 text-white text-sm font-medium rounded-lg transition-all flex items-center gap-2 shadow-sm shadow-blue-500/20"
                    >
                        <Plus size={16} />
                        <span>{t('user_token.create', { defaultValue: 'Create Token' })}</span>
                    </button>
                </div>
            </div>

            {/* Stats Cards Row */}
            <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                <motion.div
                    whileHover={{ y: -2 }}
                    className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200"
                >
                    <div className="flex items-center justify-between mb-2">
                        <div className="p-1.5 bg-blue-50 dark:bg-blue-900/20 rounded-md">
                            <Users className="w-4 h-4 text-blue-500" />
                        </div>
                    </div>
                    <div className="text-2xl font-bold text-gray-900 dark:text-base-content mb-0.5">{stats?.total_users || 0}</div>
                    <div className="text-xs text-gray-500 dark:text-gray-400">{t('user_token.total_users', { defaultValue: 'Total Users' })}</div>
                </motion.div>

                <motion.div
                    whileHover={{ y: -2 }}
                    className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200"
                >
                    <div className="flex items-center justify-between mb-2">
                        <div className="p-1.5 bg-green-50 dark:bg-green-900/20 rounded-md">
                            <Activity className="w-4 h-4 text-green-500" />
                        </div>
                    </div>
                    <div className="text-2xl font-bold text-gray-900 dark:text-base-content mb-0.5">{stats?.active_tokens || 0}</div>
                    <div className="text-xs text-gray-500 dark:text-gray-400">{t('user_token.active_tokens', { defaultValue: 'Active Tokens' })}</div>
                </motion.div>

                <motion.div
                    whileHover={{ y: -2 }}
                    className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200"
                >
                    <div className="flex items-center justify-between mb-2">
                        <div className="p-1.5 bg-purple-50 dark:bg-purple-900/20 rounded-md">
                            <Clock className="w-4 h-4 text-purple-500" />
                        </div>
                    </div>
                    <div className="text-2xl font-bold text-gray-900 dark:text-base-content mb-0.5">{stats?.total_tokens || 0}</div>
                    <div className="text-xs text-gray-500 dark:text-gray-400">{t('user_token.total_created', { defaultValue: 'Total Tokens' })}</div>
                </motion.div>

                <motion.div
                    whileHover={{ y: -2 }}
                    className="bg-white dark:bg-base-100 rounded-xl p-4 shadow-sm border border-gray-100 dark:border-base-200"
                >
                    <div className="flex items-center justify-between mb-2">
                        <div className="p-1.5 bg-orange-50 dark:bg-orange-900/20 rounded-md">
                            <Shield className="w-4 h-4 text-orange-500" />
                        </div>
                    </div>
                    <div className="text-2xl font-bold text-gray-900 dark:text-base-content mb-0.5">{stats?.today_requests || 0}</div>
                    <div className="text-xs text-gray-500 dark:text-gray-400">{t('user_token.today_requests', { defaultValue: 'Today Requests' })}</div>
                </motion.div>

            </div>

            {/* Source filter */}
            <div role="group" aria-label={t('user_token.filter_label')} className="flex flex-wrap gap-2 -mb-2">
                {tokenFilters.map(item => {
                    const count = item === 'all' ? tokens.length : tokens.filter(token => matchesFilter(token, item)).length;
                    return <button key={item} type="button" aria-pressed={filter === item} onClick={() => setFilter(item)}
                        className={`px-3 py-1.5 rounded-full text-xs font-medium border transition-colors ${filter === item ? 'bg-blue-500 border-blue-500 text-white' : 'bg-white dark:bg-base-100 border-gray-200 dark:border-base-300 text-gray-600 dark:text-gray-300 hover:border-blue-300'}`}>
                        {t(`user_token.filter_${item}`)} <span className="opacity-70">{count}</span>
                    </button>;
                })}
            </div>

            {/* Token List */}
            <div className="flex-1 overflow-auto bg-white dark:bg-base-100 rounded-2xl shadow-sm border border-gray-100 dark:border-base-200">
                <table className="table table-pin-rows">
                    <thead>
                        <tr className="bg-gray-50/50 dark:bg-base-200/50">
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.username', { defaultValue: 'Username' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.token', { defaultValue: 'Token' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.expires', { defaultValue: 'Expires' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.usage', { defaultValue: 'Usage' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.max_concurrency')}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.ip_limit', { defaultValue: 'IP Limit' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4">{t('user_token.created', { defaultValue: 'Created' })}</th>
                            <th className="bg-transparent text-gray-500 font-medium py-4 text-right">{t('common.actions', { defaultValue: 'Actions' })}</th>
                        </tr>
                    </thead>
                    <tbody className="divide-y divide-gray-50 dark:divide-base-200">
                        <AnimatePresence mode="popLayout">
                            {visibleTokens.map((token, index) => (
                                <motion.tr
                                    key={token.id}
                                    initial={{ opacity: 0, x: -10 }}
                                    animate={{ opacity: 1, x: 0 }}
                                    exit={{ opacity: 0, scale: 0.95 }}
                                    transition={{ delay: index * 0.03 }}
                                    className="hover:bg-gray-50/80 dark:hover:bg-base-200/50 transition-colors group"
                                >
                                    <td className="py-4">
                                        <div className="flex items-center gap-3">
                                            <div className="w-8 h-8 rounded-full bg-purple-50 dark:bg-purple-900/20 flex items-center justify-center text-purple-600 font-bold text-xs">
                                                {token.username.substring(0, 2).toUpperCase()}
                                            </div>
                                            <div>
                                                <div className="flex flex-wrap items-center gap-1.5">
                                                    <span className="font-semibold text-gray-900 dark:text-white uppercase tracking-wider text-xs">{token.username}</span>
                                                    {isRegistered(token)
                                                        ? <span className="px-1.5 py-0.5 rounded text-[10px] font-medium bg-blue-50 text-blue-600 dark:bg-blue-900/30 dark:text-blue-300">{t('user_token.source_registered')}</span>
                                                        : <span className="px-1.5 py-0.5 rounded text-[10px] font-medium bg-gray-100 text-gray-500 dark:bg-base-200 dark:text-gray-400">{t('user_token.source_admin')}</span>}
                                                    {isRegistered(token) && token.account_status === 'disabled' && <span className="px-1.5 py-0.5 rounded text-[10px] font-medium bg-red-50 text-red-600 dark:bg-red-900/30 dark:text-red-300">{t('user_token.account_disabled')}</span>}
                                                </div>
                                                {token.owner_email && <div className="text-[11px] text-gray-500 mt-0.5 flex items-center gap-1"><Mail size={10} />{token.owner_email}</div>}
                                                {!isRegistered(token) && <div className="text-[10px] text-gray-500">{token.description || '-'}</div>}
                                                <div className="text-[10px] text-blue-600 mt-1" title={(token.free_models ?? []).join(', ')}>
                                                    {t('user_token.free_models_selected', { count: token.free_models?.length ?? 0 })}
                                                </div>
                                            </div>
                                        </div>
                                    </td>
                                    <td>
                                        <div className="flex items-center gap-2 group/token">
                                            <code className="bg-gray-50 dark:bg-base-200 px-2 py-1 rounded border border-gray-100 dark:border-base-300 text-[11px] font-mono text-gray-600 dark:text-gray-400">
                                                {token.token.substring(0, 8)}••••••••
                                            </code>
                                            <button
                                                onClick={() => handleCopyToken(token.token)}
                                                className="p-1.5 hover:bg-gray-200 dark:hover:bg-base-300 rounded-md transition-all text-gray-400 hover:text-gray-600 dark:hover:text-white"
                                            >
                                                <Copy size={13} />
                                            </button>
                                        </div>
                                    </td>
                                    <td>
                                        <div className={`text-xs font-medium mb-1 ${getExpiresStatus(token.expires_at)}`}>
                                            {token.expires_at ? formatTime(token.expires_at) : t('user_token.never', { defaultValue: 'Never' })}
                                        </div>
                                        <div className="flex items-center gap-2">
                                            <span className="text-[10px] px-1.5 py-0.5 bg-gray-100 dark:bg-base-200 text-gray-500 rounded lowercase">
                                                {getExpiresLabel(token.expires_type)}
                                            </span>
                                            {token.expires_at && token.expires_at < Date.now() / 1000 && (
                                                <button
                                                    // A custom expiry needs a new date, so it is renewed through the edit dialog.
                                                    onClick={() => token.expires_type === 'custom' ? handleEdit(token) : handleRenew(token.id, token.expires_type)}
                                                    className="text-[10px] text-blue-500 hover:underline font-medium"
                                                >
                                                    {t('user_token.renew_button', { defaultValue: 'Renew' })}
                                                </button>
                                            )}
                                        </div>
                                    </td>
                                    <td>
                                        <div className="text-xs font-semibold text-gray-700 dark:text-gray-300">{token.total_requests} <span className="text-[10px] font-normal text-gray-400">reqs</span></div>
                                        <div className="text-[10px] text-gray-400 mt-0.5">
                                            {toQuotaMillions(token.total_tokens_used)} {t('user_token.million_tokens')}
                                        </div>
                                        <div className={`text-xs mt-1 font-medium ${(token.balance_micros ?? 0) <= 0 ? 'text-red-500' : 'text-emerald-600'}`}>
                                            {t('billing.balance_label', { amount: formatYuan(token.balance_micros ?? 0) })}
                                        </div>
                                    </td>
                                    <td>
                                        {token.max_concurrency > 0 ? token.max_concurrency : t('user_token.unlimited')}
                                    </td>
                                    <td>
                                        {token.max_ips === 0
                                            ? <span className="px-2 py-0.5 bg-gray-100 dark:bg-base-200 text-gray-500 text-[10px] rounded-full">{t('user_token.unlimited', { defaultValue: 'Unlimited' })}</span>
                                            : <span className="px-2 py-0.5 bg-orange-50 dark:bg-orange-900/20 text-orange-600 dark:text-orange-400 text-[10px] font-medium rounded-full border border-orange-100 dark:border-orange-900/30">{token.max_ips} IPs</span>
                                        }
                                        {token.curfew_start && token.curfew_end && (
                                            <div className="text-[10px] text-gray-400 mt-1.5 flex items-center gap-1 bg-gray-50 dark:bg-base-200 w-fit px-1.5 py-0.5 rounded">
                                                <Clock size={10} className="text-orange-500" />
                                                <span>{token.curfew_start} - {token.curfew_end}</span>
                                            </div>
                                        )}
                                    </td>
                                    <td className="text-[10px] text-gray-400 italic">
                                        {formatTime(token.created_at)}
                                    </td>
                                    <td className="text-right">
                                        <div className="flex justify-end items-center gap-1">
                                            <div className="flex gap-1 opacity-0 group-hover:opacity-100 focus-within:opacity-100 transition-opacity">
                                            <button
                                                onClick={() => handleEdit(token)}
                                                className="p-1.5 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-gray-500 hover:text-blue-500 transition-colors"
                                                title={t('common.edit', { defaultValue: 'Edit' })}
                                            >
                                                <Settings size={14} />
                                            </button>
                                            <div className="dropdown dropdown-end">
                                                <label tabIndex={0} className="p-1.5 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-gray-500 hover:text-green-500 transition-colors inline-block cursor-pointer">
                                                    <RefreshCw size={14} />
                                                </label>
                                                <ul tabIndex={0} className="dropdown-content z-[10] menu p-2 shadow-xl bg-white dark:bg-base-100 rounded-xl w-32 border border-gray-100 dark:border-base-200 mt-1">
                                                    <div className="px-3 py-1.5 text-[10px] font-bold text-gray-400 uppercase tracking-widest">{t('user_token.renew')}</div>
                                                    <li><a className="text-xs py-2" onClick={() => handleRenew(token.id, 'day')}>{t('user_token.expires_day', { defaultValue: '1 Day' })}</a></li>
                                                    <li><a className="text-xs py-2" onClick={() => handleRenew(token.id, 'week')}>{t('user_token.expires_week', { defaultValue: '1 Week' })}</a></li>
                                                    <li><a className="text-xs py-2" onClick={() => handleRenew(token.id, 'month')}>{t('user_token.expires_month', { defaultValue: '1 Month' })}</a></li>
                                                </ul>
                                            </div>
                                            {isRegistered(token) && <div className="dropdown dropdown-end">
                                                <label tabIndex={0} title={t('user_token.account_actions')} className="p-1.5 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-gray-500 hover:text-blue-500 transition-colors inline-block cursor-pointer">
                                                    <UserCog size={14} />
                                                </label>
                                                <ul tabIndex={0} className="dropdown-content z-[10] menu p-2 shadow-xl bg-white dark:bg-base-100 rounded-xl w-44 border border-gray-100 dark:border-base-200 mt-1">
                                                    <div className="px-3 py-1.5 text-[10px] font-bold text-gray-400 uppercase tracking-widest">{t('user_token.account_actions')}</div>
                                                    {token.account_status === 'disabled'
                                                        ? <li><a className="text-xs py-2" onClick={() => accountAction(token, 'enable')}>{t('user_token.account_enable')}</a></li>
                                                        : <li><a className="text-xs py-2" onClick={() => accountAction(token, 'disable')}>{t('user_token.account_disable')}</a></li>}
                                                    <li><a className="text-xs py-2" onClick={() => accountAction(token, 'revoke')}>{t('user_token.account_revoke')}</a></li>
                                                    {token.account_status !== 'disabled' && <li><a className="text-xs py-2" onClick={() => accountAction(token, 'reset')}>{t('user_token.account_reset')}</a></li>}
                                                </ul>
                                            </div>}
                                            <button
                                                onClick={() => requestDelete(token)}
                                                className="p-1.5 hover:bg-red-50 dark:hover:bg-red-900/20 rounded-lg text-gray-400 hover:text-red-500 transition-colors"
                                            >
                                                <Trash2 size={14} />
                                            </button>
                                            </div>
                                        </div>
                                    </td>
                                </motion.tr>
                            ))}
                        </AnimatePresence>
                        {tokens.length > 0 && visibleTokens.length === 0 && (
                            <tr><td colSpan={8} className="py-12 text-center text-sm text-gray-400">{t('user_token.filter_empty')}</td></tr>
                        )}
                        {tokens.length === 0 && !loading && (
                            <tr>
                                <td colSpan={8} className="py-20">
                                    <div className="flex flex-col items-center justify-center text-gray-400 gap-3">
                                        <div className="p-4 bg-gray-50 dark:bg-base-200 rounded-full">
                                            <Users size={40} className="opacity-20" />
                                        </div>
                                        <p className="text-sm">{t('user_token.no_data', { defaultValue: 'No tokens found' })}</p>
                                        <button
                                            onClick={() => setShowCreateModal(true)}
                                            className="text-xs text-blue-500 hover:underline"
                                        >
                                            {t('user_token.create', { defaultValue: 'Create your first token' })}
                                        </button>
                                    </div>
                                </td>
                            </tr>
                        )}
                    </tbody>
                </table>
            </div>

            {/* Create Modal */}
            {showCreateModal && (
                <div className="modal modal-open">
                    <div className="modal-box">
                        <h3 className="font-bold text-lg mb-4">{t('user_token.create_title', { defaultValue: 'Create New Token' })}</h3>

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.username', { defaultValue: 'Username' })} *</span>
                            </label>
                            <input
                                type="text"
                                className="input input-bordered w-full"
                                value={newUsername}
                                onChange={e => setNewUsername(e.target.value)}
                                placeholder={t('user_token.placeholder_username', { defaultValue: 'e.g. user1' })}
                            />
                        </div>

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.description', { defaultValue: 'Description' })}</span>
                            </label>
                            <input
                                type="text"
                                className="input input-bordered w-full"
                                value={newDesc}
                                onChange={e => setNewDesc(e.target.value)}
                                placeholder={t('user_token.placeholder_desc', { defaultValue: 'Optional notes' })}
                            />
                        </div>

                        <div className="grid grid-cols-2 gap-4 mb-3">
                            <div className="form-control w-full">
                                <label className="label">
                                    <span className="label-text">{t('user_token.expires', { defaultValue: 'Expires In' })}</span>
                                </label>
                                <select
                                    className="select select-bordered w-full"
                                    value={newExpiresType}
                                    onChange={e => setNewExpiresType(e.target.value)}
                                >
                                    <option value="day">{t('user_token.expires_day', { defaultValue: '1 Day' })}</option>
                                    <option value="week">{t('user_token.expires_week', { defaultValue: '1 Week' })}</option>
                                    <option value="month">{t('user_token.expires_month', { defaultValue: '1 Month' })}</option>
                                    <option value="custom">{t('user_token.expires_custom', { defaultValue: 'Custom' })}</option>
                                    <option value="never">{t('user_token.expires_never', { defaultValue: 'Never' })}</option>
                                </select>
                            </div>

                            <div className="form-control w-full">
                                <label className="label">
                                    <span className="label-text">{t('user_token.ip_limit', { defaultValue: 'Max IPs' })}</span>
                                </label>
                                <input
                                    type="number"
                                    className="input input-bordered w-full"
                                    value={newMaxIps}
                                    onChange={e => setNewMaxIps(parseInt(e.target.value) || 0)}
                                    min="0"
                                    placeholder={t('user_token.placeholder_max_ips', { defaultValue: '0 = Unlimited' })}
                                />
                                <label className="label">
                                    <span className="label-text-alt text-gray-500">{t('user_token.hint_max_ips', { defaultValue: '0 = Unlimited' })}</span>
                                </label>
                            </div>
                        </div>

                        <ModelAccessFields prefix="new" selected={newFreeModels}
                            models={modelChoices} loadError={modelLoadError} onSelectedChange={setNewFreeModels} />

                        <TokenLimitFields
                            prefix="create" maxConcurrency={newMaxConcurrency}
                            onMaxConcurrencyChange={setNewMaxConcurrency}
                        />

                        <BalanceFields prefix="create" balance={newBalance}
                            onBalanceChange={setNewBalance} />

                        {/* Custom Expiration Time Picker */}
                        {newExpiresType === 'custom' && (
                            <div className="form-control w-full mb-3">
                                <label className="label">
                                    <span className="label-text">{t('user_token.custom_expires_at', { defaultValue: 'Expiration Date & Time' })} *</span>
                                </label>
                                <input
                                    type="datetime-local"
                                    className="input input-bordered w-full"
                                    value={newCustomExpires}
                                    onChange={e => setNewCustomExpires(e.target.value)}
                                    min={toDateTimeLocal(nowSeconds())}
                                />
                                <label className="label">
                                    <span className="label-text-alt text-gray-500">{t('user_token.hint_custom_expires', { defaultValue: 'Select the exact date and hour when this token expires' })}</span>
                                </label>
                            </div>
                        )}

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.curfew', { defaultValue: 'Curfew (Service Unavailable Time)' })}</span>
                            </label>
                            <div className="flex gap-2 items-center">
                                <input
                                    type="time"
                                    className="input input-bordered w-full"
                                    value={newCurfewStart}
                                    onChange={e => setNewCurfewStart(e.target.value)}
                                />
                                <span className="text-gray-400">to</span>
                                <input
                                    type="time"
                                    className="input input-bordered w-full"
                                    value={newCurfewEnd}
                                    onChange={e => setNewCurfewEnd(e.target.value)}
                                />
                            </div>
                            <label className="label">
                                <span className="label-text-alt text-gray-500">{t('user_token.hint_curfew', { defaultValue: 'Leave empty to disable. Based on Beijing time (UTC+8).' })}</span>
                            </label>
                        </div>

                        <div className="modal-action">
                            <button className="px-4 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm transition-colors" onClick={() => setShowCreateModal(false)}>
                                {t('common.cancel', { defaultValue: 'Cancel' })}
                            </button>
                            <button
                                className={`px-4 py-2 bg-blue-500 hover:bg-blue-600 text-white text-sm font-medium rounded-lg transition-all shadow-sm shadow-blue-500/20 flex items-center gap-2 ${creating ? 'opacity-50 cursor-not-allowed' : ''}`}
                                onClick={handleCreate}
                                disabled={creating}
                            >
                                {creating && <RefreshCw size={14} className="animate-spin" />}
                                {t('common.create', { defaultValue: 'Create' })}
                            </button>
                        </div>
                    </div>
                </div>
            )}

            {/* Edit Modal */}
            {showEditModal && editingToken && (
                <div className="modal modal-open">
                    <div className="modal-box">
                        <h3 className="font-bold text-lg mb-4">{t('user_token.edit_title', { defaultValue: 'Edit Token' })}</h3>
                        {isRegistered(editingToken) && (
                            <div className="mb-4 rounded-lg px-3 py-2.5 text-xs bg-blue-50 text-blue-800 dark:bg-blue-900/20 dark:text-blue-200">
                                <div className="flex items-center gap-1.5 font-medium"><Mail size={12} />{editingToken.owner_email} · {t(editingToken.account_status === 'disabled' ? 'user_token.account_disabled' : 'user_token.source_registered')}</div>
                                <p className="mt-1">{t('billing.registered_edit_hint')}</p>
                            </div>
                        )}

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.username', { defaultValue: 'Username' })} *</span>
                            </label>
                            <input
                                type="text"
                                className="input input-bordered w-full"
                                value={editUsername}
                                onChange={e => setEditUsername(e.target.value)}
                                placeholder={t('user_token.placeholder_username', { defaultValue: 'e.g. user1' })}
                            />
                        </div>

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.description', { defaultValue: 'Description' })}</span>
                            </label>
                            <input
                                type="text"
                                className="input input-bordered w-full"
                                value={editDesc}
                                onChange={e => setEditDesc(e.target.value)}
                                placeholder={t('user_token.placeholder_desc', { defaultValue: 'Optional notes' })}
                            />
                        </div>

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.expires', { defaultValue: 'Expires' })}</span>
                            </label>
                            <select
                                className="select select-bordered w-full"
                                value={editExpiresType}
                                onChange={e => setEditExpiresType(e.target.value)}
                            >
                                <option value="keep">
                                    {t('user_token.expires_keep', {
                                        value: editingToken.expires_at ? formatTime(editingToken.expires_at) : t('user_token.never', { defaultValue: 'Never' }),
                                        defaultValue: 'Keep current ({{value}})'
                                    })}
                                </option>
                                <option value="day">{t('user_token.expires_day', { defaultValue: '1 Day' })}</option>
                                <option value="week">{t('user_token.expires_week', { defaultValue: '1 Week' })}</option>
                                <option value="month">{t('user_token.expires_month', { defaultValue: '1 Month' })}</option>
                                <option value="custom">{t('user_token.expires_custom', { defaultValue: 'Custom' })}</option>
                                <option value="never">{t('user_token.expires_never', { defaultValue: 'Never' })}</option>
                            </select>
                            {editExpiresType === 'custom' && (
                                <input
                                    type="datetime-local"
                                    className="input input-bordered w-full mt-2"
                                    value={editCustomExpires}
                                    onChange={e => setEditCustomExpires(e.target.value)}
                                    min={toDateTimeLocal(nowSeconds())}
                                />
                            )}
                            <label className="label">
                                <span className="label-text-alt text-gray-500">
                                    {editExpiresType === 'custom'
                                        ? t('user_token.hint_custom_expires', { defaultValue: 'Select the exact date and hour when this token expires' })
                                        : t('user_token.hint_edit_expires', { defaultValue: '1 Day / 1 Week / 1 Month count from when you save.' })}
                                </span>
                            </label>
                        </div>

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.ip_limit', { defaultValue: 'Max IPs' })}</span>
                            </label>
                            <input
                                type="number"
                                className="input input-bordered w-full"
                                value={editMaxIps}
                                onChange={e => setEditMaxIps(parseInt(e.target.value) || 0)}
                                min="0"
                                placeholder={t('user_token.placeholder_max_ips', { defaultValue: '0 = Unlimited' })}
                            />
                            <label className="label">
                                <span className="label-text-alt text-gray-500">{t('user_token.hint_max_ips', { defaultValue: '0 = Unlimited' })}</span>
                            </label>
                        </div>

                        <ModelAccessFields prefix="edit" selected={editFreeModels}
                            models={modelChoices} loadError={modelLoadError} onSelectedChange={setEditFreeModels} />

                        <TokenLimitFields
                            prefix="edit" maxConcurrency={editMaxConcurrency}
                            onMaxConcurrencyChange={setEditMaxConcurrency}
                            usedTokens={editingToken?.total_tokens_used}
                        />

                        <BalanceFields prefix="edit" balance={editBalance}
                            onBalanceChange={setEditBalance}
                            currentBalance={editingToken.balance_micros} />

                        <div className="form-control w-full mb-3">
                            <label className="label">
                                <span className="label-text">{t('user_token.curfew', { defaultValue: 'Curfew (Service Unavailable Time)' })}</span>
                            </label>
                            <div className="flex gap-2 items-center">
                                <input
                                    type="time"
                                    className="input input-bordered w-full"
                                    value={editCurfewStart}
                                    onChange={e => setEditCurfewStart(e.target.value)}
                                />
                                <span className="text-gray-400">to</span>
                                <input
                                    type="time"
                                    className="input input-bordered w-full"
                                    value={editCurfewEnd}
                                    onChange={e => setEditCurfewEnd(e.target.value)}
                                />
                            </div>
                            <label className="label">
                                <span className="label-text-alt text-gray-500">{t('user_token.hint_curfew', { defaultValue: 'Leave empty to disable. Based on Beijing time (UTC+8).' })}</span>
                            </label>
                        </div>

                        <div className="modal-action">
                            <button className="px-4 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm transition-colors" onClick={() => setShowEditModal(false)}>
                                {t('common.cancel', { defaultValue: 'Cancel' })}
                            </button>
                            <button
                                className={`px-4 py-2 bg-blue-500 hover:bg-blue-600 text-white text-sm font-medium rounded-lg transition-all shadow-sm shadow-blue-500/20 flex items-center gap-2 ${updating ? 'opacity-50 cursor-not-allowed' : ''}`}
                                onClick={handleUpdate}
                                disabled={updating}
                            >
                                {updating && <RefreshCw size={14} className="animate-spin" />}
                                {t('common.update', { defaultValue: 'Update' })}
                            </button>
                        </div>
                    </div>
                </div>
            )}
            <PortalSettingsModal open={showPortalSettings}
                onClose={() => { setShowPortalSettings(false); loadData(); }} />
            <ModelPricingModal open={showPricing} models={modelChoices} onClose={() => setShowPricing(false)} />
            <ModalDialog
                isOpen={deleting !== null}
                title={t('user_token.delete_registered_title')}
                message={t('user_token.delete_registered_message', { email: deleting?.owner_email ?? '' })}
                type="confirm"
                isDestructive
                confirmText={t('common.delete')}
                onCancel={() => setDeleting(null)}
                onConfirm={() => { const target = deleting; setDeleting(null); if (target) void handleDelete(target.id); }}
            />
        </motion.div>
    );
};
export default UserToken;
