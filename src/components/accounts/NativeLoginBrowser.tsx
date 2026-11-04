import { useEffect, useRef, useState } from 'react';
import { Loader2, RefreshCw } from 'lucide-react';
import { useTranslation } from 'react-i18next';
import { request } from '../../utils/request';
import type { NativeBrowserInput, NativeBrowserSnapshot, NativeProvider } from '../../types/nativeProviders';

export function nativeLoginError(error: unknown, translate: (key: string) => string): string {
    const message = String(error);
    if (message.includes('TRAE_CALLBACK_PORT_UNAVAILABLE')) return translate('nativeProviders.callbackPortUnavailable');
    for (const code of ['REGION_BLOCKED', 'START_FAILED', 'STOPPED', 'BUSY', 'PAGE_UNAVAILABLE', 'UNAVAILABLE', 'COMMAND_FAILED']) {
        if (message.includes(`TRAE_BROWSER_${code}`)) return translate(`nativeProviders.browserError_${code}`);
    }
    return message;
}

export default function NativeLoginBrowser({ provider, id }: { provider: NativeProvider; id: string }) {
    const { t } = useTranslation();
    const [frame, setFrame] = useState<NativeBrowserSnapshot | null>(null);
    const [error, setError] = useState('');
    const [text, setText] = useState('');
    const alive = useRef(true);
    const inputQueue = useRef<Promise<void>>(Promise.resolve());
    const pending = useRef(0);
    const translate = useRef(t); translate.current = t;
    const canvas = useRef<HTMLDivElement>(null);

    useEffect(() => {
        alive.current = true;
        const controller = new AbortController();
        let timer: number | undefined;
        const update = async () => {
            try {
                const snapshot = await request<NativeBrowserSnapshot>('native_login_browser_snapshot', { request: { provider, id } }, { signal: controller.signal });
                if (alive.current) { setFrame(snapshot); setError(''); }
            } catch (error) {
                if (!controller.signal.aborted && alive.current) setError(nativeLoginError(error, translate.current));
            }
            if (alive.current) timer = window.setTimeout(update, 1200);
        };
        void update();
        return () => { alive.current = false; controller.abort(); window.clearTimeout(timer); };
    }, [provider, id]);

    function send(action: NativeBrowserInput) {
        if (pending.current >= 32) return;
        pending.current++;
        inputQueue.current = inputQueue.current.then(async () => {
            if (!alive.current) return;
            try {
                await request('native_login_browser_input', { request: { provider, id, action } });
            } catch (error) { if (alive.current) setError(nativeLoginError(error, translate.current)); }
        }).finally(() => { pending.current--; });
    }

    return <div className="space-y-3">
        <p className="text-sm leading-relaxed text-gray-500">{t('nativeProviders.browserHint')}</p>
        <div className="flex flex-wrap items-center justify-between gap-2 text-xs">
            <span className="max-w-full truncate text-gray-500">{frame?.location || t('nativeProviders.browserStarting')}</span>
            <button type="button" className="inline-flex items-center gap-2 rounded-lg border px-3 py-2 hover:bg-gray-50 dark:hover:bg-gray-800" onClick={() => send({ type: 'reload' })}><RefreshCw size={14} />{t('nativeProviders.browserReload')}</button>
        </div>
        {error && <p role="alert" className="text-sm text-red-600 dark:text-red-400">{error}</p>}
        <div ref={canvas} tabIndex={0} role="application" aria-label={t('nativeProviders.browserLabel')}
            className="relative mx-auto w-full max-w-[1000px] overflow-hidden rounded-lg border border-gray-200 bg-white outline-none focus:ring-2 focus:ring-blue-500 dark:border-gray-700"
            style={{ aspectRatio: '1000 / 720' }}
            onClick={event => {
                if (!frame) return;
                canvas.current?.focus();
                const rect = event.currentTarget.getBoundingClientRect();
                send({ type: 'click', x: (event.clientX - rect.left) * 1000 / rect.width, y: (event.clientY - rect.top) * 720 / rect.height });
            }}
            onKeyDown={event => {
                if (event.nativeEvent.isComposing) return;
                if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'v') return;
                if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'a') { event.preventDefault(); send({ type: 'key', key: 'SelectAll' }); return; }
                if (event.ctrlKey || event.metaKey || event.altKey) return;
                if (event.key.length === 1) { event.preventDefault(); send({ type: 'text', text: event.key }); }
                else if (['Enter', 'Tab', 'Backspace', 'Delete', 'Escape', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) {
                    event.preventDefault(); send({ type: 'key', key: event.key === 'Tab' && event.shiftKey ? 'ShiftTab' : event.key });
                }
            }}
            onPaste={event => { event.preventDefault(); const value = event.clipboardData.getData('text'); if (value && new TextEncoder().encode(value).length <= 2048) send({ type: 'text', text: value }); }}
            onWheel={event => { send({ type: 'scroll', delta_y: Math.max(-1200, Math.min(1200, event.deltaY)) }); }}>
            {frame ? <img src={frame.image} alt={t('nativeProviders.browserLabel')} draggable={false} className="h-full w-full select-none object-contain" /> : <div role="status" className="absolute inset-0 flex items-center justify-center gap-2 text-sm text-gray-500"><Loader2 size={18} className="animate-spin" />{t('nativeProviders.browserStarting')}</div>}
        </div>
        <form className="flex flex-wrap items-end gap-2" onSubmit={event => { event.preventDefault(); if (!text || new TextEncoder().encode(text).length > 2048) return; send({ type: 'text', text }); setText(''); canvas.current?.focus(); }}>
            <label className="min-w-48 flex-1 space-y-1 text-xs text-gray-500"><span>{t('nativeProviders.browserTextHint')}</span><input type="password" autoComplete="off" value={text} onChange={event => setText(event.target.value)} maxLength={2048} className="w-full rounded-lg border bg-white px-3 py-2 text-sm dark:border-gray-600 dark:bg-gray-800" /></label>
            <button type="submit" disabled={!text} className="rounded-lg border px-3 py-2 text-sm disabled:opacity-50">{t('nativeProviders.browserType')}</button>
            <button type="button" className="rounded-lg border px-3 py-2 text-sm" onClick={() => send({ type: 'key', key: 'Tab' })}>Tab</button>
            <button type="button" className="rounded-lg border px-3 py-2 text-sm" onClick={() => send({ type: 'key', key: 'Enter' })}>Enter</button>
        </form>
    </div>;
}
