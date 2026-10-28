import { useEffect, useState } from 'react';
import { ArrowRight, Eye, EyeOff, KeyRound, Mail, RefreshCw, ShieldCheck } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { PortalConfig, PortalResult, portalCall } from './portalApi';
import PortalAnnouncement from './PortalAnnouncement';

type Tab = 'email' | 'key';
type EmailView = 'login' | 'register' | 'reset';

const TAB_STORAGE = 'abv_portal_login_tab';
const input = 'w-full rounded-lg border border-slate-300 bg-white px-3 py-3 text-sm outline-none focus:border-blue-500 focus:ring-2 focus:ring-blue-100 disabled:bg-slate-50';
const primary = 'w-full flex justify-center items-center gap-2 rounded-lg bg-blue-600 text-white text-sm font-medium py-3 hover:bg-blue-700 disabled:opacity-50';
const link = 'text-blue-600 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 rounded';

function storedTab(): Tab {
    try { return localStorage.getItem(TAB_STORAGE) === 'key' ? 'key' : 'email'; } catch { return 'email'; }
}

interface Props {
    config: PortalConfig | null;
    busy: boolean;
    keyError: string;
    accountError: string;
    notice: string;
    onKeyLogin: (token: string) => void;
    onAccountReady: () => void;
}

export default function PortalAuthForms({ config, busy, keyError, accountError, notice, onKeyLogin, onAccountReady }: Props) {
    const { t } = useTranslation();
    const [tab, setTab] = useState<Tab>(storedTab);
    const [view, setView] = useState<EmailView>('login');
    const [token, setToken] = useState('');
    const [email, setEmail] = useState('');
    const [password, setPassword] = useState('');
    const [confirm, setConfirm] = useState('');
    const [code, setCode] = useState('');
    const [showPassword, setShowPassword] = useState(false);
    const [working, setWorking] = useState(false);
    const [sending, setSending] = useState(false);
    const [cooldown, setCooldown] = useState(0);
    const [error, setError] = useState('');
    const [info, setInfo] = useState('');

    useEffect(() => {
        if (cooldown <= 0) return;
        const timer = window.setTimeout(() => setCooldown(c => c - 1), 1000);
        return () => window.clearTimeout(timer);
    }, [cooldown]);

    function switchTab(next: Tab) {
        setTab(next); setError(''); setInfo('');
        try { localStorage.setItem(TAB_STORAGE, next); } catch { /* storage unavailable */ }
    }
    function switchView(next: EmailView) {
        setView(next); setError(''); setInfo(''); setCode(''); setPassword(''); setConfirm('');
    }
    function describe(result: PortalResult): string {
        const key = `token_portal.auth_errors.${result.code ?? 'unknown'}`;
        const seconds = result.retryAfter ?? 0;
        const text = t(key, { seconds, minutes: Math.max(1, Math.ceil(seconds / 60)), defaultValue: '' });
        return text || t('token_portal.auth_errors.unknown');
    }

    async function sendCode() {
        setError(''); setInfo('');
        if (!email.trim()) { setError(t('token_portal.auth_errors.invalid_email')); return; }
        setSending(true);
        const result = await portalCall<{ resend_after?: number }>('auth/send-code', { email, purpose: view === 'register' ? 'register' : 'reset' });
        setSending(false);
        if (result.ok) {
            setCooldown(result.data.resend_after ?? 60);
            setInfo(t(view === 'register' ? 'token_portal.code_sent_register' : 'token_portal.code_sent_reset'));
        } else {
            if (result.retryAfter) setCooldown(result.retryAfter);
            setError(describe(result));
        }
    }

    async function submitEmail(event: React.FormEvent) {
        event.preventDefault();
        setError(''); setInfo('');
        if (view !== 'login' && password !== confirm) { setError(t('token_portal.password_mismatch')); return; }
        setWorking(true);
        const result = view === 'login'
            ? await portalCall('auth/login', { email, password })
            : view === 'register'
                ? await portalCall('auth/register', { email, code, password })
                : await portalCall('auth/reset-password', { email, code, password });
        setWorking(false);
        if (!result.ok) { setError(describe(result)); return; }
        if (view === 'reset') {
            switchView('login');
            setInfo(t('token_portal.reset_done'));
            return;
        }
        setPassword(''); setConfirm(''); setCode('');
        onAccountReady();
    }

    const disabled = busy || working;
    const title = tab === 'key' ? t('token_portal.login_title') : t(`token_portal.email_${view}_title`);
    const description = tab === 'key' ? t('token_portal.login_description') : t(`token_portal.email_${view}_description`);

    return (
        <main className="max-w-md mx-auto px-5 py-16 sm:py-24">
            {config?.announcement && <div className="mb-8"><PortalAnnouncement announcement={config.announcement} /></div>}
            <div className="mb-8">
                <div className="w-12 h-12 flex items-center justify-center rounded-xl bg-blue-50 text-blue-600 mb-6">{tab === 'key' ? <KeyRound size={24} /> : <Mail size={24} />}</div>
                <p className="text-sm text-blue-600 font-medium mb-3">{t('token_portal.kicker')}</p>
                <h1 className="text-3xl font-semibold tracking-tight mb-3">{title}</h1>
                <p className="text-sm leading-6 text-slate-500">{description}</p>
            </div>
            <div className="bg-white border border-slate-200 rounded-2xl shadow-sm">
                <div role="tablist" aria-label={t('token_portal.login_method')} className="grid grid-cols-2 border-b border-slate-200">
                    {(['email', 'key'] as Tab[]).map(item => <button key={item} type="button" role="tab" aria-selected={tab === item} onClick={() => switchTab(item)}
                        className={`py-3.5 text-sm font-medium border-b-2 -mb-px transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500 ${tab === item ? 'border-blue-600 text-blue-600' : 'border-transparent text-slate-500 hover:text-slate-900'}`}>
                        {t(item === 'email' ? 'token_portal.tab_email' : 'token_portal.tab_key')}
                    </button>)}
                </div>
                {tab === 'key' ? (
                    <form onSubmit={e => { e.preventDefault(); const value = token.trim(); if (value) onKeyLogin(value); }} className="p-6 space-y-4">
                        <label htmlFor="portal-token" className="block text-sm font-medium">API Token</label>
                        <input id="portal-token" type="password" autoComplete="off" spellCheck={false} value={token} onChange={e => setToken(e.target.value)} maxLength={256}
                            placeholder="sk-…" disabled={busy} required className={input} />
                        {keyError && <p role="alert" className="text-sm text-red-600">{keyError}</p>}
                        {notice && !keyError && <p role="status" className="text-sm text-emerald-700">{notice}</p>}
                        <button type="submit" disabled={busy || !token.trim()} className={primary}>
                            {busy ? <RefreshCw size={16} className="animate-spin" /> : <ArrowRight size={16} />}{t('token_portal.login')}
                        </button>
                        <p className="text-xs leading-5 text-slate-500 flex gap-2"><ShieldCheck size={16} className="shrink-0 mt-0.5" />{t('token_portal.memory_only')}</p>
                    </form>
                ) : (
                    <form onSubmit={e => void submitEmail(e)} className="p-6 space-y-4">
                        <div className="space-y-2">
                            <label htmlFor="portal-email" className="block text-sm font-medium">{t('token_portal.email')}</label>
                            <input id="portal-email" type="email" autoComplete="email" value={email} onChange={e => setEmail(e.target.value)} maxLength={254}
                                placeholder="name@example.com" disabled={disabled} required className={input} />
                        </div>
                        {view !== 'login' && <div className="space-y-2">
                            <label htmlFor="portal-code" className="block text-sm font-medium">{t('token_portal.code')}</label>
                            <div className="flex gap-2">
                                <input id="portal-code" inputMode="numeric" autoComplete="one-time-code" pattern="[0-9]{6}" maxLength={6} value={code}
                                    onChange={e => setCode(e.target.value.replace(/\D/g, ''))} placeholder="000000" disabled={disabled} required className={`${input} tracking-widest`} />
                                <button type="button" onClick={() => void sendCode()} disabled={disabled || sending || cooldown > 0}
                                    className="shrink-0 rounded-lg border border-slate-300 px-3 text-sm font-medium text-slate-700 hover:bg-slate-50 disabled:opacity-50 min-w-28">
                                    {sending ? <RefreshCw size={16} className="animate-spin mx-auto" /> : cooldown > 0 ? t('token_portal.resend_in', { seconds: cooldown }) : t('token_portal.send_code')}
                                </button>
                            </div>
                        </div>}
                        <div className="space-y-2">
                            <div className="flex items-center justify-between">
                                <label htmlFor="portal-password" className="block text-sm font-medium">{t(view === 'reset' ? 'token_portal.new_password' : 'token_portal.password')}</label>
                                {view === 'login' && config?.password_reset_enabled && <button type="button" onClick={() => switchView('reset')} className={`text-xs ${link}`}>{t('token_portal.forgot_password')}</button>}
                            </div>
                            <div className="relative">
                                <input id="portal-password" type={showPassword ? 'text' : 'password'} autoComplete={view === 'login' ? 'current-password' : 'new-password'}
                                    value={password} onChange={e => setPassword(e.target.value)} minLength={view === 'login' ? undefined : 8} maxLength={128}
                                    disabled={disabled} required className={`${input} pr-11`} />
                                <button type="button" onClick={() => setShowPassword(!showPassword)} aria-label={t(showPassword ? 'token_portal.hide_token' : 'token_portal.show_token')}
                                    className="absolute right-2 top-1/2 -translate-y-1/2 rounded-md p-1.5 text-slate-400 hover:text-slate-700">{showPassword ? <EyeOff size={16} /> : <Eye size={16} />}</button>
                            </div>
                            {view !== 'login' && <p className="text-xs text-slate-500">{t('token_portal.password_rule')}</p>}
                        </div>
                        {view !== 'login' && <div className="space-y-2">
                            <label htmlFor="portal-confirm" className="block text-sm font-medium">{t('token_portal.confirm_password')}</label>
                            <input id="portal-confirm" type={showPassword ? 'text' : 'password'} autoComplete="new-password" value={confirm} onChange={e => setConfirm(e.target.value)}
                                maxLength={128} disabled={disabled} required className={input} />
                        </div>}
                        {(error || accountError) && <p role="alert" className="text-sm text-red-600">{error || accountError}</p>}
                        {(info || notice) && !error && <p role="status" className="text-sm text-emerald-700">{info || notice}</p>}
                        <button type="submit" disabled={disabled} className={primary}>
                            {disabled ? <RefreshCw size={16} className="animate-spin" /> : <ArrowRight size={16} />}{t(`token_portal.email_${view}_submit`)}
                        </button>
                        <div className="text-sm text-slate-500 text-center">
                            {view === 'login'
                                ? config?.registration_enabled && <>{t('token_portal.no_account')} <button type="button" onClick={() => switchView('register')} className={link}>{t('token_portal.go_register')}</button></>
                                : <button type="button" onClick={() => switchView('login')} className={link}>{t('token_portal.back_to_login')}</button>}
                        </div>
                        {view === 'register' && <p className="text-xs leading-5 text-slate-500 flex gap-2"><ShieldCheck size={16} className="shrink-0 mt-0.5" />{t('token_portal.register_note')}</p>}
                    </form>
                )}
            </div>
        </main>
    );
}
