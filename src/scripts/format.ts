// Formatage des valeurs affichées (français).
const nf = (n: number, digits: number) => n.toFixed(digits).replace('.', ',');

export function formatBytes(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n < 1024) return `${Math.round(n)} o`;
  const units = ['Ko', 'Mo', 'Go', 'To'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${nf(v, v >= 100 ? 0 : v >= 10 ? 1 : 2)} ${units[i]}`;
}

export function formatSpeed(bps: number | null | undefined): string {
  return bps == null ? '—' : `${formatBytes(bps)}/s`;
}

/** Durée lisible : « 45 s », « 3 min 05 s », « 1 h 02 min ». */
export function formatDuration(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds)) return '—';
  const t = Math.max(0, Math.round(seconds));
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = t % 60;
  const pad = (n: number) => String(n).padStart(2, '0');
  if (h > 0) return `${h} h ${pad(m)} min`;
  if (m > 0) return `${m} min ${pad(s)} s`;
  return `${s} s`;
}

/** Timecode hh:mm:ss. */
export function formatTimecode(seconds: number): string {
  const t = Math.max(0, Math.round(seconds));
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(Math.floor(t / 3600))}:${pad(Math.floor((t % 3600) / 60))}:${pad(t % 60)}`;
}

/** Durée fine pour les écarts (en secondes, 2 décimales si < 10 s). */
export function formatGap(seconds: number): string {
  const a = Math.abs(seconds);
  if (a < 10) return `${nf(seconds, 2)} s`;
  return formatDuration(seconds);
}

export function formatDate(ms: number | null | undefined): string {
  if (!ms) return '';
  return new Date(ms).toLocaleString('fr-FR', { dateStyle: 'short', timeStyle: 'short' });
}
