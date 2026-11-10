type ToastKind = 'ok' | 'err' | 'info';

export function toast(message: string, kind: ToastKind = 'info', ms = 4000): void {
  const host = document.getElementById('toasts');
  if (!host) return;
  const el = document.createElement('div');
  const color =
    kind === 'ok' ? 'border-success text-fg' : kind === 'err' ? 'border-accent text-danger' : 'border-control text-fg';
  el.className = `animate-pop-in pointer-events-auto max-w-sm rounded-lg border bg-raised px-4 py-2.5 text-sm shadow-[0_12px_32px_rgb(0_0_0/0.55)] ${color}`;
  el.setAttribute('role', 'status');
  el.textContent = message;
  host.appendChild(el);
  setTimeout(() => {
    el.style.transition = 'opacity .2s';
    el.style.opacity = '0';
    setTimeout(() => el.remove(), 220);
  }, ms);
}
