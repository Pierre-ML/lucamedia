// Écran Téléchargements : formulaire d'ajout, carte « En cours », listes En attente / Erreurs / Terminés.
import {
  queueAdd,
  queueRemove,
  queueRetry,
  queueCancel,
  queueClearFinished,
  queueSetPaused,
  verifyAgain,
  revealDownload,
  openDestDir,
  queueState,
  onQueueUpdate,
  onDownloadProgress,
  errMsg,
  type Download,
  type GapReport,
  type NewItem,
  type Phase,
  type ProgressEvent,
  type QueueState,
} from './api';
import { $, el, btn } from './dom';
import { formatBytes, formatSpeed, formatDuration, formatTimecode, formatGap, formatDate } from './format';
import { toast } from './toast';
import { state } from './state';

const PHASES: Record<Phase, string> = {
  resolving: 'Recherche de la vidéo',
  downloading: 'Téléchargement',
  verifying: 'Vérification',
  repairing: 'Réparation',
  assembling: 'Assemblage',
};
const VIA = { direct: 'Lien direct', ytdlp: 'yt-dlp', browser: 'Navigateur' } as const;
const TRACK = { video: 'vidéo', audio: 'audio' } as const;

const BTN = 'button-ghost button-sm';
const BTN_DANGER = 'button-danger button-sm';

const name = (d: Download): string => d.filename || d.url;

// Détails de rapport ouverts (conservés d'un rendu à l'autre).
const openReports = new Set<number>();
// Signature de chaque liste : on ne reconstruit que si les données ont changé.
const signatures = new Map<string, string>();

async function run(action: () => Promise<QueueState>): Promise<void> {
  try {
    applyQueue(await action());
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}

function changed(key: string, data: unknown): boolean {
  const sig = JSON.stringify(data);
  if (signatures.get(key) === sig) return false;
  signatures.set(key, sig);
  return true;
}

// ---------- Carte « En cours » ----------

let runningId: number | null = null;

function setStat(id: string, value: string): void {
  $(id).textContent = value;
}

function paintRunning(d: Download | undefined): void {
  const card = $('cur-card');
  $('cur-empty').hidden = !!d;
  card.hidden = !d;
  if (!d) {
    runningId = null;
    return;
  }
  runningId = d.id;
  setStat('cur-name', name(d));
  $('cur-name').title = d.url;
  setStat('cur-url', d.url);
  paintProgress(d);
  const q = $('cur-quality');
  q.textContent = d.quality ?? '—';
  setStat('cur-via', d.resolvedVia ? VIA[d.resolvedVia] : '—');
  const retry = $('cur-try');
  retry.hidden = d.retryCount <= 0;
  retry.textContent = `Tentative ${d.retryCount + 1}`;
}

function paintProgress(p: Pick<ProgressEvent, 'phase' | 'progressPercent' | 'downloadedBytes' | 'totalBytes' | 'speedBps' | 'etaSeconds' | 'segmentsDone' | 'segmentsTotal'>): void {
  const pct = Math.max(0, Math.min(100, p.progressPercent || 0));
  setStat('cur-phase', p.phase ? PHASES[p.phase] : 'En préparation');
  const bar = $('cur-bar');
  bar.style.width = `${pct}%`;
  setStat('cur-percent', `${pct.toFixed(1).replace('.', ',')} %`);
  setStat(
    'cur-seg',
    p.segmentsTotal != null ? `${p.segmentsDone ?? 0} / ${p.segmentsTotal}` : '—',
  );
  setStat(
    'cur-size',
    p.totalBytes != null ? `${formatBytes(p.downloadedBytes)} / ${formatBytes(p.totalBytes)}` : formatBytes(p.downloadedBytes),
  );
  setStat('cur-speed', formatSpeed(p.speedBps));
  setStat('cur-eta', formatDuration(p.etaSeconds));
}

function onProgress(p: ProgressEvent): void {
  if (p.id !== runningId) return;
  const d = state.queue.items.find((x) => x.id === p.id);
  if (d) Object.assign(d, p);
  paintProgress(p);
}

// ---------- Listes ----------

function emptyState(text: string, bear = false): HTMLElement {
  if (!bear) return el('p', 'py-3 text-sm text-muted', text);
  const wrap = el('div', 'flex flex-col items-center py-6 text-center');
  const art = document.getElementById('ours-template')?.firstElementChild?.cloneNode(true);
  if (art instanceof SVGElement) {
    art.setAttribute('class', 'mb-3 size-14 shrink-0 text-fg/25');
    wrap.append(art);
  }
  wrap.append(el('p', 'text-sm text-muted', text));
  return wrap;
}

function rowBase(d: Download): { row: HTMLElement; info: HTMLElement } {
  const row = el('div', 'flex flex-col gap-2 rounded-lg border border-border bg-bg p-3');
  const top = el('div', 'flex items-center justify-between gap-3');
  const info = el('div', 'min-w-0 flex-1');
  info.append(el('div', 'truncate text-sm font-medium', name(d)), el('div', 'truncate font-mono text-[11px] text-muted', d.url));
  info.children[0]?.setAttribute('title', name(d));
  info.children[1]?.setAttribute('title', d.url);
  top.append(info);
  row.append(top);
  return { row, info };
}

function topOf(row: HTMLElement): HTMLElement {
  return row.firstElementChild as HTMLElement;
}

function renderPending(items: Download[]): void {
  $('pending-count').textContent = String(items.length);
  if (!changed('pending', items.map((d) => [d.id, d.filename, d.url]))) return;
  const list = $('pending-list');
  list.replaceChildren();
  if (!items.length) {
    list.append(emptyState('Aucun lien en attente.'));
    return;
  }
  items.forEach((d, i) => {
    const { row } = rowBase(d);
    const actions = el('div', 'flex shrink-0 items-center gap-2');
    actions.append(el('span', 'pill bg-fg/5 text-muted', `n° ${i + 1}`), btn('Supprimer', BTN_DANGER, () => run(() => queueRemove(d.id))));
    topOf(row).append(actions);
    list.append(row);
  });
}

function renderErrors(items: Download[]): void {
  $('error-count').textContent = String(items.length);
  if (!changed('errors', items.map((d) => [d.id, d.filename, d.url, d.error]))) return;
  const list = $('error-list');
  list.replaceChildren();
  if (!items.length) {
    list.append(emptyState('Aucune erreur.'));
    return;
  }
  for (const d of items) {
    const { row } = rowBase(d);
    const actions = el('div', 'flex shrink-0 items-center gap-2');
    actions.append(btn('Relancer', BTN, () => run(() => queueRetry(d.id))), btn('Supprimer', BTN_DANGER, () => run(() => queueRemove(d.id))));
    topOf(row).append(actions);
    row.append(el('div', 'break-words rounded-md bg-danger/10 px-2.5 py-1.5 text-xs text-danger', d.error ?? 'Erreur inconnue.'));
    list.append(row);
  }
}

function reportSection(d: Download, r: GapReport): HTMLElement {
  const box = el('details', 'rounded-md border border-border bg-code');
  if (openReports.has(d.id)) box.open = true;
  box.addEventListener('toggle', () => {
    if (box.open) openReports.add(d.id);
    else openReports.delete(d.id);
  });
  box.append(el('summary', 'cursor-pointer select-none px-3 py-2 text-xs text-muted hover:text-fg', 'Rapport de vérification'));
  const body = el('div', 'flex flex-col gap-2 border-t border-border px-3 py-3 text-xs');
  const head = el('div', 'flex items-start gap-2');
  head.append(el('span', 'size-5 shrink-0'), el('p', 'text-fg', r.summary));
  // Hibou décoratif : cloné depuis le gabarit rendu par Astro.
  const owl = document.getElementById('owl-template')?.firstElementChild?.cloneNode(true);
  if (owl instanceof SVGElement) {
    owl.setAttribute('class', 'size-5 shrink-0 text-fg/60');
    head.firstElementChild?.replaceWith(owl);
  }
  body.append(head);

  const dl = el('dl', 'grid grid-cols-[auto_1fr] gap-x-4 gap-y-1');
  const row = (k: string, v: string): void => {
    dl.append(el('dt', 'text-muted', k), el('dd', 'tabular-nums text-fg', v));
  };
  row('Durée attendue', r.expectedDuration != null ? formatDuration(r.expectedDuration) : 'inconnue');
  row('Durée réelle', formatDuration(r.actualDuration));
  if (r.avDrift != null) row('Écart audio/vidéo', formatGap(r.avDrift));
  body.append(dl);

  if (r.gaps.length) {
    body.append(el('p', 'mt-1 font-medium text-fg', `Coupures détectées (${r.gaps.length})`));
    const ul = el('ul', 'flex flex-col gap-1');
    for (const g of r.gaps) {
      const seg = g.segmentIndex != null ? ` — segment ${g.segmentIndex}` : '';
      ul.append(el('li', 'font-mono text-[11px] text-muted', `${formatTimecode(g.at)} : ${formatGap(g.missing)} manquantes, piste ${TRACK[g.stream]}${seg}`));
    }
    body.append(ul);
  }
  if (r.badSegments.length) {
    body.append(el('p', 'mt-1 font-medium text-fg', `Segments fautifs (${r.badSegments.length})`));
    const ul = el('ul', 'flex flex-col gap-1');
    for (const s of r.badSegments) {
      ul.append(el('li', 'font-mono text-[11px] text-muted', `${TRACK[s.track]} n° ${s.index} : ${s.reason}`));
    }
    body.append(ul);
  }
  box.append(body);
  return box;
}

function renderDone(items: Download[]): void {
  $('done-count').textContent = String(items.length);
  ($('done-clear') as HTMLButtonElement).disabled = items.length === 0;
  if (!changed('done', items)) return;
  const list = $('done-list');
  list.replaceChildren();
  if (!items.length) {
    list.append(emptyState('Aucun téléchargement terminé.', true));
    return;
  }
  for (const d of items) {
    const { row, info } = rowBase(d);
    const warn = d.status === 'warning';
    const stamp = el('div', 'mt-1 flex flex-wrap items-center gap-2');
    stamp.append(
      el('span', `pill ${warn ? 'bg-warning/15 text-warning' : 'bg-success/15 text-success'}`, warn ? 'Coupures détectées' : 'Complet'),
      el('span', 'text-[11px] text-muted', formatDate(d.finishedAt)),
    );
    if (d.quality) stamp.append(el('span', 'text-[11px] text-muted', d.quality));
    if (d.totalBytes ?? d.downloadedBytes) stamp.append(el('span', 'text-[11px] text-muted tabular-nums', formatBytes(d.totalBytes ?? d.downloadedBytes)));
    info.append(stamp);
    const actions = el('div', 'flex shrink-0 flex-wrap items-center justify-end gap-2');
    actions.append(
      btn('Afficher dans le dossier', BTN, async () => {
        try {
          await revealDownload(d.id);
        } catch (e) {
          toast(errMsg(e), 'err');
        }
      }),
      btn('Revérifier', BTN, async () => {
        toast('Vérification en cours…');
        await run(() => verifyAgain(d.id));
      }),
      btn('Retirer', BTN_DANGER, () => run(() => queueRemove(d.id))),
    );
    topOf(row).append(actions);
    if (d.report) row.append(reportSection(d, d.report));
    list.append(row);
  }
}

// ---------- État global ----------

const lastStatus = new Map<number, string>();

function notify(items: Download[]): void {
  for (const d of items) {
    const prev = lastStatus.get(d.id);
    if (prev && prev !== d.status) {
      if (d.status === 'done') toast(`${name(d)} : téléchargement terminé`, 'ok');
      else if (d.status === 'warning') toast(`${name(d)} : terminé, des coupures ont été détectées`, 'info', 7000);
      else if (d.status === 'error') toast(`${name(d)} : ${d.error ?? 'erreur'}`, 'err', 7000);
    }
    lastStatus.set(d.id, d.status);
  }
}

function paintPause(): void {
  const b = $<HTMLButtonElement>('q-pause');
  b.textContent = state.queue.paused ? 'Reprendre la file' : 'Mettre la file en pause';
  $('q-paused').hidden = !state.queue.paused;
}

function applyQueue(q: QueueState): void {
  state.queue = q;
  const items = q.items;
  notify(items);
  paintPause();
  paintRunning(items.find((d) => d.status === 'running'));
  renderPending(items.filter((d) => d.status === 'pending'));
  renderErrors(items.filter((d) => d.status === 'error'));
  const done = items
    .filter((d) => d.status === 'done' || d.status === 'warning')
    .sort((a, b) => (b.finishedAt ?? b.createdAt) - (a.finishedAt ?? a.createdAt));
  renderDone(done);
}

// ---------- Formulaire ----------

async function submit(e: Event): Promise<void> {
  e.preventDefault();
  const urls = $<HTMLTextAreaElement>('add-urls')
    .value.split('\n')
    .map((l) => l.trim())
    .filter(Boolean);
  if (!urls.length) {
    toast('Collez au moins un lien.', 'err');
    return;
  }
  const filename = $<HTMLInputElement>('add-filename').value.trim();
  const referer = $<HTMLInputElement>('add-referer').value.trim();
  const items: NewItem[] = urls.map((url) => {
    const it: NewItem = { url };
    if (urls.length === 1 && filename) it.filename = filename;
    if (referer) it.referer = referer;
    return it;
  });
  const submitBtn = $<HTMLButtonElement>('add-submit');
  submitBtn.disabled = true;
  try {
    applyQueue(await queueAdd(items));
    $<HTMLTextAreaElement>('add-urls').value = '';
    $<HTMLInputElement>('add-filename').value = '';
    toast(urls.length > 1 ? `${urls.length} liens ajoutés à la file` : 'Lien ajouté à la file', 'ok');
  } catch (err) {
    toast(errMsg(err), 'err');
  } finally {
    submitBtn.disabled = !state.tauri;
  }
}

export async function initDownloads(): Promise<void> {
  $('add-form').addEventListener('submit', (e) => void submit(e));
  const urlsBox = $<HTMLTextAreaElement>('add-urls');
  urlsBox.addEventListener('input', () => {
    const n = urlsBox.value.split('\n').filter((l) => l.trim()).length;
    const f = $<HTMLInputElement>('add-filename');
    f.disabled = n > 1;
    f.placeholder = n > 1 ? 'Disponible pour un seul lien' : 'Nom du fichier (sans extension)';
  });
  $('q-pause').addEventListener('click', () => void run(() => queueSetPaused(!state.queue.paused)));
  $('q-open').addEventListener('click', async () => {
    try {
      await openDestDir();
    } catch (e) {
      toast(errMsg(e), 'err');
    }
  });
  $('done-clear').addEventListener('click', () => void run(queueClearFinished));
  $('cur-cancel').addEventListener('click', () => {
    if (runningId != null) {
      const id = runningId;
      void run(() => queueCancel(id));
    }
  });

  for (const id of ['add-submit', 'q-pause', 'q-open']) $<HTMLButtonElement>(id).disabled = !state.tauri;
  applyQueue(state.queue);
  if (!state.tauri) return;
  await onQueueUpdate(applyQueue);
  await onDownloadProgress(onProgress);
  try {
    const q = await queueState();
    for (const d of q.items) lastStatus.set(d.id, d.status);
    applyQueue(q);
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}
