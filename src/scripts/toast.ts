export type ToastKind = 'ok' | 'err' | 'info';

export function toast(message: string, kind: ToastKind = 'info', ms = 4000): void {
  const host = document.getElementById('toasts');
  if (!host) return;
  const el = document.createElement('div');
  const color =
    kind === 'ok' ? 'border-success/40 text-fg' : kind === 'err' ? 'border-danger/40 text-danger' : 'border-border text-fg';
  el.className = `animate-pop-in pointer-events-auto max-w-sm rounded-lg border bg-surface px-4 py-2.5 text-sm shadow-[0_12px_32px_rgb(0_0_0/0.55)] ${color}`;
  el.setAttribute('role', 'status');
  el.textContent = message;
  host.appendChild(el);
  setTimeout(() => {
    el.style.transition = 'opacity .2s';
    el.style.opacity = '0';
    setTimeout(() => el.remove(), 220);
  }, ms);
}
