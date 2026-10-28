import { useState } from 'react';
import { AlertTriangle, Megaphone, X } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import type { PortalAnnouncementData } from './portalApi';

const DISMISS_STORAGE = 'abv_portal_announcement_dismissed';

/** Short stable fingerprint, so a dismissed notice comes back once its text changes. */
function fingerprint({ level, content }: PortalAnnouncementData): string {
    let hash = 0;
    for (const char of `${level}:${content}`) hash = (Math.imul(hash, 31) + char.codePointAt(0)!) | 0;
    return String(hash >>> 0);
}

function dismissedFingerprint(): string | null {
    try { return localStorage.getItem(DISMISS_STORAGE); } catch { return null; }
}

/** Admin notice on the usage portal's sign-in page and dashboard. Plain text only. */
export default function PortalAnnouncement({ announcement }: { announcement?: PortalAnnouncementData | null }) {
    const { t } = useTranslation();
    const [dismissed, setDismissed] = useState(dismissedFingerprint);
    if (!announcement?.content) return null;
    const id = fingerprint(announcement);
    if (dismissed === id) return null;
    const warning = announcement.level === 'warning';
    const Icon = warning ? AlertTriangle : Megaphone;

    function dismiss() {
        setDismissed(id);
        try { localStorage.setItem(DISMISS_STORAGE, id); } catch { /* storage unavailable */ }
    }

    return (
        <section aria-label={t('token_portal.announcement')} role={warning ? 'alert' : 'status'}
            className={`flex items-start gap-3 rounded-lg border px-4 py-3 text-sm ${warning ? 'border-amber-200 bg-amber-50 text-amber-900' : 'border-blue-100 bg-white text-slate-700'}`}>
            <Icon size={18} className={`mt-0.5 shrink-0 ${warning ? 'text-amber-600' : 'text-blue-600'}`} aria-hidden="true" />
            <div className="min-w-0 flex-1">
                <p className="font-medium mb-1">{t('token_portal.announcement')}</p>
                <p className="whitespace-pre-line break-words leading-6">{announcement.content}</p>
            </div>
            <button type="button" onClick={dismiss} aria-label={t('token_portal.announcement_dismiss')}
                className="shrink-0 rounded p-1 text-slate-400 hover:bg-slate-100 hover:text-slate-600 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500">
                <X size={16} />
            </button>
        </section>
    );
}
