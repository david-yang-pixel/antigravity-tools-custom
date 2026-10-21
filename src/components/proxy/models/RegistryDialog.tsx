import { useId, useLayoutEffect, useRef, type ReactNode } from 'react';
import ModalDialog from '../../common/ModalDialog';

interface Props {
    title: string;
    children: ReactNode;
    confirmText?: string;
    onConfirm: () => void;
    onCancel?: () => void;
    busy?: boolean;
    blocked?: boolean;
    destructive?: boolean;
}

// Adapt the shared dialog locally: no changes to other callers or shared settings.
export default function RegistryDialog({ title, children, confirmText = '确认 / Confirm', onConfirm, onCancel, busy = false, blocked = false, destructive = false }: Props) {
    const content = useRef<HTMLDivElement>(null);
    const cancel = useRef(onCancel);
    const pending = useRef(busy);
    cancel.current = onCancel;
    pending.current = busy;
    const titleId = useId();
    useLayoutEffect(() => {
        const modal = content.current?.closest<HTMLElement>('.modal');
        const box = modal?.querySelector<HTMLElement>('.modal-box');
        if (!modal || !box) return;
        modal.classList.add('model-registry-dialog');
        const heading = box.querySelector('h3');
        if (heading) heading.id = titleId;
        box.setAttribute('role', 'dialog');
        box.setAttribute('aria-modal', 'true');
        box.setAttribute('aria-labelledby', titleId);
        box.tabIndex = -1;
        const previous = document.activeElement as HTMLElement | null;
        const controls = () => Array.from(box.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]')).filter(el => el.getClientRects().length > 0);
        (controls()[0] || box).focus();
        const keydown = (event: KeyboardEvent) => {
            if (event.key === 'Escape') {
                event.preventDefault();
                event.stopPropagation();
                if (!pending.current) cancel.current?.();
            }
            if (event.key === 'Tab') {
                const items = controls();
                if (!items.length) { event.preventDefault(); box.focus(); return; }
                const first = items[0], last = items[items.length - 1];
                if (event.shiftKey && (document.activeElement === first || !items.includes(document.activeElement as HTMLElement))) {
                    event.preventDefault(); last.focus();
                } else if (!event.shiftKey && (document.activeElement === last || !items.includes(document.activeElement as HTMLElement))) {
                    event.preventDefault(); first.focus();
                }
            }
        };
        document.addEventListener('keydown', keydown, true);
        return () => {
            document.removeEventListener('keydown', keydown, true);
            if (previous?.isConnected) previous.focus();
        };
    }, [titleId]);
    useLayoutEffect(() => {
        // ModalDialog has no confirm-disabled prop; scope adaptation to this instance.
        const box = content.current?.closest('.modal-box');
        const confirm = box?.querySelector<HTMLButtonElement>(':scope > div > div:last-child > button:last-child');
        if (confirm) confirm.disabled = busy || blocked;
    }, [busy, blocked]);
    return <ModalDialog isOpen title={title} type={onCancel ? 'confirm' : 'info'} confirmText={confirmText} cancelText="取消 / Cancel"
        onConfirm={() => { if (!busy && !blocked) onConfirm(); }} onCancel={onCancel ? () => { if (!busy) onCancel(); } : undefined}
        isLoading={busy} isDestructive={destructive}>
        <div ref={content} className="model-registry space-y-4 text-sm" aria-busy={busy}>{children}</div>
    </ModalDialog>;
}
