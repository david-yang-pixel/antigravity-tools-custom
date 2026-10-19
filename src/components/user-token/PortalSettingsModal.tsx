import { useEffect, useState } from 'react';
import { CheckCircle2, CreditCard, Mail, RefreshCw, AlertTriangle } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request as invoke } from '../../utils/request';
import { showToast } from '../common/ToastContainer';
import { parseYuan, toYuan } from './money';

type SmtpSecurity = 'tls' | 'starttls' | 'none';

interface PortalSettings {
    registration_enabled: boolean;
    allowed_email_domains: string[];
    default_max_concurrency: number;
    default_expires_type: string;
    smtp: {
        host: string;
        port: number;
        security: SmtpSecurity;
        username: string;
        password: string;
        from_address: string;
        from_name: string;
    };
    payment: {
        enabled: boolean;
        gateway: string;
        pid: string;
        pay_types: string[];
        public_url: string;
        min_amount_cents: number;
        max_amount_cents: number;
        subject: string;
    };
}

interface PortalSettingsView extends PortalSettings {
    smtp_password_set: boolean;
    smtp_ready: boolean;
    payment_key_set: boolean;
    payment_ready: boolean;
    env_locked: string[];
}

interface Props {
    open: boolean;
    onClose: () => void;
}

const field = 'input input-bordered w-full input-sm';
const ENV_NAMES: Record<string, string> = {
    registration_enabled: 'ABV_PORTAL_REGISTRATION',
    allowed_email_domains: 'ABV_PORTAL_EMAIL_DOMAINS',
    'smtp.host': 'ABV_SMTP_HOST',
    'smtp.port': 'ABV_SMTP_PORT',
    'smtp.security': 'ABV_SMTP_SECURITY',
    'smtp.username': 'ABV_SMTP_USERNAME',
    'smtp.password': 'ABV_SMTP_PASSWORD',
    'smtp.from_address': 'ABV_SMTP_FROM',
    'smtp.from_name': 'ABV_SMTP_FROM_NAME',
    'payment.enabled': 'ABV_ZPAY_ENABLED',
    'payment.gateway': 'ABV_ZPAY_GATEWAY',
    'payment.pid': 'ABV_ZPAY_PID',
    'payment.key': 'ABV_ZPAY_KEY',
    'payment.pay_types': 'ABV_ZPAY_PAY_TYPES',
    'payment.public_url': 'ABV_ZPAY_PUBLIC_URL',
    'payment.min_amount_cents': 'ABV_ZPAY_MIN_AMOUNT',
    'payment.max_amount_cents': 'ABV_ZPAY_MAX_AMOUNT',
    'payment.subject': 'ABV_ZPAY_SUBJECT',
};
const PAY_TYPES = ['alipay', 'wxpay'] as const;

/** Registration switch, defaults for new accounts' keys, and SMTP for verification codes. */
export default function PortalSettingsModal({ open, onClose }: Props) {
    const { t } = useTranslation();
    const [view, setView] = useState<PortalSettingsView | null>(null);
    const [form, setForm] = useState<PortalSettings | null>(null);
    const [domains, setDomains] = useState('');
    const [password, setPassword] = useState('');
    const [paymentKey, setPaymentKey] = useState('');
    const [minAmount, setMinAmount] = useState('');
    const [maxAmount, setMaxAmount] = useState('');
    const [testTo, setTestTo] = useState('');
    const [saving, setSaving] = useState(false);
    const [testing, setTesting] = useState(false);

    function apply(next: PortalSettingsView) {
        setView(next);
        setForm(next);
        setDomains(next.allowed_email_domains.join(', '));
        setPassword('');
        setPaymentKey('');
        // Yuan in the form, cents on the wire.
        setMinAmount(toYuan(next.payment.min_amount_cents * 10_000));
        setMaxAmount(toYuan(next.payment.max_amount_cents * 10_000));
    }

    useEffect(() => {
        if (!open) return;
        invoke<PortalSettingsView>('get_portal_settings').then(apply).catch(e => showToast(String(e), 'error'));
    }, [open]);

    if (!open) return null;
    const locked = (name: string) => view?.env_locked.includes(name) ?? false;
    const lockHint = (name: string) => locked(name) && <span className="text-[10px] text-amber-600 ml-2">{t('user_token.portal_env_locked', { name: ENV_NAMES[name] })}</span>;
    const setSmtp = (patch: Partial<PortalSettings['smtp']>) => form && setForm({ ...form, smtp: { ...form.smtp, ...patch } });
    const setPayment = (patch: Partial<PortalSettings['payment']>) => form && setForm({ ...form, payment: { ...form.payment, ...patch } });
    // Judged from the form being edited, not the last save, so the warning tracks what is typed.
    const paymentComplete = !!form && !!form.payment.gateway.trim() && !!form.payment.pid.trim()
        && !!form.payment.public_url.trim() && form.payment.pay_types.length > 0 && (!!paymentKey || !!view?.payment_key_set);
    // Yuan string → cents; parseYuan reads micro-units, so scale down and require whole cents.
    const yuanToCents = (value: string) => { const micros = parseYuan(value.trim()); return micros !== null && micros % 10_000 === 0 ? micros / 10_000 : null; };

    async function save(): Promise<boolean> {
        if (!form) return false;
        const minCents = yuanToCents(minAmount);
        const maxCents = yuanToCents(maxAmount);
        if (minCents === null || maxCents === null || minCents < 1 || maxCents < minCents) {
            showToast(t('topup.admin_invalid_limits'), 'error');
            return false;
        }
        setSaving(true);
        try {
            const next = await invoke<PortalSettingsView>('save_portal_settings', {
                request: {
                    registration_enabled: form.registration_enabled,
                    default_max_concurrency: form.default_max_concurrency,
                    default_expires_type: form.default_expires_type,
                    smtp: form.smtp,
                    allowed_email_domains: domains.split(/[\s,]+/).filter(Boolean),
                    payment: { ...form.payment, min_amount_cents: minCents, max_amount_cents: maxCents },
                    ...(password ? { smtp_password: password } : {}),
                    ...(paymentKey ? { payment_key: paymentKey } : {}),
                },
            });
            apply(next);
            showToast(t('common.saved') || t('common.update_success'), 'success');
            return true;
        } catch (e) {
            showToast(String(e), 'error');
            return false;
        } finally {
            setSaving(false);
        }
    }

    async function sendTest() {
        if (!testTo.trim()) return;
        // Test what is on screen: unsaved changes are saved first.
        if (!(await save())) return;
        setTesting(true);
        try {
            await invoke('send_portal_test_email', { to: testTo.trim() });
            showToast(t('user_token.portal_test_sent', { to: testTo.trim() }), 'success');
        } catch (e) {
            showToast(String(e), 'error');
        } finally {
            setTesting(false);
        }
    }

    return (
        <div className="modal modal-open">
            <div className="modal-box max-w-2xl">
                <h3 className="font-bold text-lg">{t('user_token.portal_settings')}</h3>
                <p className="text-xs text-gray-500 mt-1 mb-4">{t('user_token.portal_settings_hint')}</p>
                {!form ? <div className="py-12 flex justify-center"><RefreshCw className="animate-spin text-gray-400" /></div> : <div className="space-y-5">
                    <section className="space-y-3">
                        <label className="flex items-center justify-between gap-4 cursor-pointer">
                            <span>
                                <span className="text-sm font-medium">{t('user_token.portal_registration')}</span>{lockHint('registration_enabled')}
                                <span className="block text-xs text-gray-500 mt-0.5">{t('user_token.portal_registration_hint')}</span>
                            </span>
                            <input type="checkbox" className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600 disabled:opacity-50 disabled:bg-gray-100 dark:disabled:bg-base-200" checked={form.registration_enabled} disabled={locked('registration_enabled')}
                                onChange={e => setForm({ ...form, registration_enabled: e.target.checked })} />
                        </label>
                        {form.registration_enabled && !view?.smtp_ready && <p className="flex gap-2 text-xs text-amber-700 bg-amber-50 dark:bg-amber-900/20 rounded-lg px-3 py-2"><AlertTriangle size={14} className="shrink-0 mt-0.5" />{t('user_token.portal_needs_smtp')}</p>}
                        <div className="form-control">
                            <label className="label py-1" htmlFor="portal-domains"><span className="label-text text-sm">{t('user_token.portal_domains')}</span>{lockHint('allowed_email_domains')}</label>
                            <input id="portal-domains" className={field} value={domains} disabled={locked('allowed_email_domains')} onChange={e => setDomains(e.target.value)} placeholder="example.com, company.cn" />
                            <p className="text-xs text-gray-500 mt-1">{t('user_token.portal_domains_hint')}</p>
                        </div>
                    </section>

                    <section className="space-y-3 border-t border-gray-100 dark:border-base-200 pt-4">
                        <h4 className="text-sm font-medium">{t('user_token.portal_defaults')}</h4>
                        <p className="text-xs text-gray-500">{t('user_token.portal_defaults_hint')}</p>
                        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                            <div className="form-control">
                                <label className="label py-1" htmlFor="portal-concurrency"><span className="label-text text-sm">{t('user_token.max_concurrency')}</span></label>
                                <input id="portal-concurrency" type="number" min="0" step="1" className={field} value={form.default_max_concurrency}
                                    onChange={e => setForm({ ...form, default_max_concurrency: Math.max(0, parseInt(e.target.value) || 0) })} />
                                <p className="text-xs text-gray-500 mt-1">{t('user_token.concurrency_hint')}</p>
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="portal-expiry"><span className="label-text text-sm">{t('user_token.expires')}</span></label>
                                <select id="portal-expiry" className="select select-bordered select-sm w-full" value={form.default_expires_type} onChange={e => setForm({ ...form, default_expires_type: e.target.value })}>
                                    {['never', 'month', 'week', 'day'].map(type => <option key={type} value={type}>{t(`user_token.expires_${type}`)}</option>)}
                                </select>
                            </div>
                        </div>
                    </section>

                    <section className="space-y-3 border-t border-gray-100 dark:border-base-200 pt-4">
                        <div className="flex items-center justify-between">
                            <h4 className="text-sm font-medium flex items-center gap-2"><Mail size={15} />{t('user_token.portal_smtp')}</h4>
                            <span className={`text-xs flex items-center gap-1 ${view?.smtp_ready ? 'text-green-600' : 'text-gray-400'}`}>{view?.smtp_ready && <CheckCircle2 size={13} />}{t(view?.smtp_ready ? 'user_token.portal_smtp_ready' : 'user_token.portal_smtp_missing')}</span>
                        </div>
                        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
                            <div className="form-control sm:col-span-2">
                                <label className="label py-1" htmlFor="smtp-host"><span className="label-text text-sm">{t('user_token.smtp_host')}</span>{lockHint('smtp.host')}</label>
                                <input id="smtp-host" className={field} value={form.smtp.host} disabled={locked('smtp.host')} onChange={e => setSmtp({ host: e.target.value })} placeholder="smtp.example.com" />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="smtp-port"><span className="label-text text-sm">{t('user_token.smtp_port')}</span>{lockHint('smtp.port')}</label>
                                <input id="smtp-port" type="number" min="1" max="65535" className={field} value={form.smtp.port} disabled={locked('smtp.port')} onChange={e => setSmtp({ port: parseInt(e.target.value) || 0 })} />
                            </div>
                        </div>
                        <div className="form-control">
                            <label className="label py-1" htmlFor="smtp-security"><span className="label-text text-sm">{t('user_token.smtp_security')}</span>{lockHint('smtp.security')}</label>
                            <select id="smtp-security" className="select select-bordered select-sm w-full" value={form.smtp.security} disabled={locked('smtp.security')}
                                onChange={e => { const security = e.target.value as SmtpSecurity; setSmtp({ security, port: security === 'tls' ? 465 : security === 'starttls' ? 587 : 25 }); }}>
                                <option value="tls">{t('user_token.smtp_tls')}</option>
                                <option value="starttls">{t('user_token.smtp_starttls')}</option>
                                <option value="none">{t('user_token.smtp_none')}</option>
                            </select>
                        </div>
                        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                            <div className="form-control">
                                <label className="label py-1" htmlFor="smtp-user"><span className="label-text text-sm">{t('user_token.smtp_username')}</span>{lockHint('smtp.username')}</label>
                                <input id="smtp-user" className={field} autoComplete="off" value={form.smtp.username} disabled={locked('smtp.username')} onChange={e => setSmtp({ username: e.target.value })} />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="smtp-password"><span className="label-text text-sm">{t('user_token.smtp_password')}</span>{lockHint('smtp.password')}</label>
                                <input id="smtp-password" type="password" autoComplete="new-password" className={field} value={password} disabled={locked('smtp.password')} onChange={e => setPassword(e.target.value)}
                                    placeholder={view?.smtp_password_set ? t('user_token.smtp_password_keep') : ''} />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="smtp-from"><span className="label-text text-sm">{t('user_token.smtp_from')}</span>{lockHint('smtp.from_address')}</label>
                                <input id="smtp-from" type="email" className={field} value={form.smtp.from_address} disabled={locked('smtp.from_address')} onChange={e => setSmtp({ from_address: e.target.value })} placeholder="noreply@example.com" />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="smtp-from-name"><span className="label-text text-sm">{t('user_token.smtp_from_name')}</span>{lockHint('smtp.from_name')}</label>
                                <input id="smtp-from-name" className={field} value={form.smtp.from_name} disabled={locked('smtp.from_name')} onChange={e => setSmtp({ from_name: e.target.value })} placeholder="Stellar AI" />
                            </div>
                        </div>
                        <p className="text-xs text-gray-500">{t('user_token.smtp_hint')}</p>
                        <div className="flex gap-2">
                            <input type="email" className={field} value={testTo} onChange={e => setTestTo(e.target.value)} placeholder={t('user_token.smtp_test_to')} />
                            <button type="button" onClick={() => void sendTest()} disabled={testing || saving || !testTo.trim()} className="btn btn-sm btn-outline shrink-0">
                                {testing && <RefreshCw size={13} className="animate-spin" />}{t('user_token.smtp_test')}
                            </button>
                        </div>
                    </section>

                    <section className="space-y-3 border-t border-gray-100 dark:border-base-200 pt-4">
                        <div className="flex items-center justify-between">
                            <h4 className="text-sm font-medium flex items-center gap-2"><CreditCard size={15} />{t('topup.admin_title')}</h4>
                            <span className={`text-xs flex items-center gap-1 ${view?.payment_ready ? 'text-green-600' : 'text-gray-400'}`}>{view?.payment_ready && <CheckCircle2 size={13} />}{t(view?.payment_ready ? 'topup.admin_ready' : 'topup.admin_not_ready')}</span>
                        </div>
                        <label className="flex items-center justify-between gap-4 cursor-pointer">
                            <span>
                                <span className="text-sm font-medium">{t('topup.admin_enabled')}</span>{lockHint('payment.enabled')}
                                <span className="block text-xs text-gray-500 mt-0.5">{t('topup.admin_enabled_hint')}</span>
                            </span>
                            <input type="checkbox" className="toggle toggle-sm bg-gray-200 dark:bg-base-300 border-gray-300 dark:border-base-300 checked:bg-blue-600 checked:border-blue-600 disabled:opacity-50 disabled:bg-gray-100 dark:disabled:bg-base-200" checked={form.payment.enabled} disabled={locked('payment.enabled')}
                                onChange={e => setPayment({ enabled: e.target.checked })} />
                        </label>
                        {form.payment.enabled && !paymentComplete && <p className="flex gap-2 text-xs text-amber-700 bg-amber-50 dark:bg-amber-900/20 rounded-lg px-3 py-2"><AlertTriangle size={14} className="shrink-0 mt-0.5" />{t('topup.admin_needs_config')}</p>}
                        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                            <div className="form-control">
                                <label className="label py-1" htmlFor="pay-gateway"><span className="label-text text-sm">{t('topup.admin_gateway')}</span>{lockHint('payment.gateway')}</label>
                                <input id="pay-gateway" className={field} value={form.payment.gateway} disabled={locked('payment.gateway')} onChange={e => setPayment({ gateway: e.target.value })} placeholder="https://zpayz.cn" />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="pay-pid"><span className="label-text text-sm">{t('topup.admin_pid')}</span>{lockHint('payment.pid')}</label>
                                <input id="pay-pid" className={field} autoComplete="off" value={form.payment.pid} disabled={locked('payment.pid')} onChange={e => setPayment({ pid: e.target.value })} />
                            </div>
                            <div className="form-control sm:col-span-2">
                                <label className="label py-1" htmlFor="pay-key"><span className="label-text text-sm">{t('topup.admin_key')}</span>{lockHint('payment.key')}</label>
                                <input id="pay-key" type="password" autoComplete="new-password" className={field} value={paymentKey} disabled={locked('payment.key')} onChange={e => setPaymentKey(e.target.value)}
                                    placeholder={view?.payment_key_set ? t('topup.admin_key_keep') : ''} />
                            </div>
                            <div className="form-control sm:col-span-2">
                                <label className="label py-1" htmlFor="pay-public-url"><span className="label-text text-sm">{t('topup.admin_public_url')}</span>{lockHint('payment.public_url')}</label>
                                <input id="pay-public-url" className={field} value={form.payment.public_url} disabled={locked('payment.public_url')} onChange={e => setPayment({ public_url: e.target.value })} placeholder="https://your-domain.com" />
                                <p className="text-xs text-gray-500 mt-1 break-all">{form.payment.public_url.trim()
                                    ? t('topup.admin_notify_preview', { url: `${form.payment.public_url.trim().replace(/\/+$/, '')}/api/pay/epay/notify` })
                                    : t('topup.admin_public_url_hint')}</p>
                            </div>
                        </div>
                        <div>
                            <span className="text-sm">{t('topup.admin_pay_types')}</span>{lockHint('payment.pay_types')}
                            <div className="flex gap-5 mt-1.5">
                                {PAY_TYPES.map(type => <label key={type} className="flex items-center gap-2 text-sm cursor-pointer">
                                    <input type="checkbox" className="checkbox checkbox-sm" disabled={locked('payment.pay_types')} checked={form.payment.pay_types.includes(type)}
                                        onChange={e => setPayment({ pay_types: e.target.checked ? [...form.payment.pay_types, type] : form.payment.pay_types.filter(item => item !== type) })} />
                                    {t(`topup.type_${type}`)}
                                </label>)}
                            </div>
                            <p className="text-xs text-gray-500 mt-1">{t('topup.admin_pay_types_hint')}</p>
                        </div>
                        <div className="grid grid-cols-1 sm:grid-cols-3 gap-3">
                            <div className="form-control">
                                <label className="label py-1" htmlFor="pay-min"><span className="label-text text-sm">{t('topup.admin_min')}</span>{lockHint('payment.min_amount_cents')}</label>
                                <input id="pay-min" inputMode="decimal" className={field} value={minAmount} disabled={locked('payment.min_amount_cents')} onChange={e => setMinAmount(e.target.value)} />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="pay-max"><span className="label-text text-sm">{t('topup.admin_max')}</span>{lockHint('payment.max_amount_cents')}</label>
                                <input id="pay-max" inputMode="decimal" className={field} value={maxAmount} disabled={locked('payment.max_amount_cents')} onChange={e => setMaxAmount(e.target.value)} />
                            </div>
                            <div className="form-control">
                                <label className="label py-1" htmlFor="pay-subject"><span className="label-text text-sm">{t('topup.admin_subject')}</span>{lockHint('payment.subject')}</label>
                                <input id="pay-subject" className={field} value={form.payment.subject} disabled={locked('payment.subject')} onChange={e => setPayment({ subject: e.target.value })} />
                            </div>
                        </div>
                        <p className="text-xs text-gray-500">{t('topup.admin_limits_hint')}</p>
                    </section>
                </div>}
                <div className="modal-action">
                    <button className="px-4 py-2 hover:bg-gray-100 dark:hover:bg-base-200 rounded-lg text-sm transition-colors" onClick={onClose}>{t('common.close', { defaultValue: 'Close' })}</button>
                    <button className="px-4 py-2 bg-blue-500 hover:bg-blue-600 text-white text-sm font-medium rounded-lg flex items-center gap-2 disabled:opacity-50" onClick={() => void save()} disabled={!form || saving}>
                        {saving && <RefreshCw size={14} className="animate-spin" />}{t('common.save', { defaultValue: 'Save' })}
                    </button>
                </div>
            </div>
        </div>
    );
}
