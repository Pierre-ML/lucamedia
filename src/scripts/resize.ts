// Barre latérale redimensionnable à la souris (et au clavier), taille mémorisée dans localStorage.
const KEY = 'ursa.layout';
const DEFAULT = 256;
const MIN = 180;
const MAX = 420;
const STEP = 16;

const clamp = (v: number): number => Math.min(MAX, Math.max(MIN, v));

function load(): number {
  try {
    const raw = localStorage.getItem(KEY);
    const v = raw ? (JSON.parse(raw) as { sidebar?: unknown }).sidebar : undefined;
    return typeof v === 'number' && Number.isFinite(v) ? clamp(v) : DEFAULT;
  } catch {
    return DEFAULT;
  }
}

function save(px: number): void {
  try {
    localStorage.setItem(KEY, JSON.stringify({ sidebar: px }));
  } catch {
    /* stockage indisponible */
  }
}

export function initResize(): void {
  const sidebar = document.getElementById('sidebar');
  const h = document.querySelector<HTMLElement>('[data-resize="sidebar"]');
  if (!sidebar || !h) return;

  let width = load();
  const apply = (px: number, persist: boolean): void => {
    width = clamp(px);
    sidebar.style.width = `${Math.round(width)}px`;
    h.setAttribute('aria-valuenow', String(Math.round(width)));
    if (persist) save(width);
  };
  apply(width, false);

  let dragging = false;
  let startX = 0;
  let startW = 0;
  h.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    e.preventDefault();
    dragging = true;
    startX = e.clientX;
    startW = width;
    h.setPointerCapture(e.pointerId);
    h.dataset.dragging = 'true';
    document.body.style.userSelect = 'none';
    document.body.style.cursor = 'col-resize';
  });
  h.addEventListener('pointermove', (e) => {
    if (dragging) apply(startW + e.clientX - startX, false);
  });
  const end = (e: PointerEvent): void => {
    if (!dragging) return;
    dragging = false;
    apply(width, true);
    if (h.hasPointerCapture(e.pointerId)) h.releasePointerCapture(e.pointerId);
    delete h.dataset.dragging;
    document.body.style.userSelect = '';
    document.body.style.cursor = '';
  };
  h.addEventListener('pointerup', end);
  h.addEventListener('pointercancel', end);
  h.addEventListener('dblclick', () => apply(DEFAULT, true));
  h.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    e.preventDefault();
    apply(width + (e.key === 'ArrowRight' ? STEP : -STEP), true);
  });
}
