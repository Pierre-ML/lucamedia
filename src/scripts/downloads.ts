// Écran Téléchargements : ajout de liens et liste unique des téléchargements (une carte chacun).
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
  onDetectAssist,
  errMsg,
  type Download,
  type DetectAssist,
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
  resolving: 'Recherche de la vidéo…',
  downloading: 'Téléchargement…',
  verifying: 'Vérification…',
  repairing: 'Réparation des coupures…',
  assembling: 'Assemblage de la vidéo…',
};
const VIA = { direct: 'Lien direct', ytdlp: 'Page analysée (yt-dlp)', browser: 'Navigateur' } as const;
const TRACK = { video: 'vidéo', audio: 'audio' } as const;
const TONE = { run: 'text-fg', wait: 'text-muted', ok: 'text-success', warn: 'text-warning', err: 'text-danger' } as const;
type Tone = keyof typeof TONE;

const BTN = 'button-ghost button-sm';
const BTN_DANGER = 'button-danger button-sm';
const BTN_TEXT = 'button-text';
const BTN_TEXT_DANGER = 'button-text button-text-danger';

const name = (d: Download): string => d.filename || d.url;

type Progress = Pick<
  ProgressEvent,
  'phase' | 'progressPercent' | 'downloadedBytes' | 'totalBytes' | 'speedBps' | 'etaSeconds' | 'segmentsDone' | 'segmentsTotal'
>;

type Card = {
  root: HTMLElement;
  sig: string;
  /** Carte « en cours » : repeint phase, barre, débit et segments. */
  paint?: (p: Progress) => void;
  /** Carte « en cours » : affiche ou masque l'encart d'aide à la détection. */
  setAssist?: (on: boolean) => void;
};

const cards = new Map<number, Card>();
// Panneaux repliables ouverts (conservés d'un rendu à l'autre) : « id:report », « id:details ».
const openPanels = new Set<string>();
// Téléchargements pour lesquels une fenêtre de navigateur attend que l'utilisateur lance la lecture.
const assist = new Set<number>();

async function run(action: () => Promise<QueueState>): Promise<void> {
  try {
    applyQueue(await action());
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}

// ---------- Briques de carte ----------

function svgIcon(path: string, cls: string): SVGSVGElement {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', '1.8');
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('class', cls);
  const p = document.createElementNS(ns, 'path');
  p.setAttribute('d', path);
  svg.append(p);
  return svg;
}

function statusLine(text: string, tone: Tone, dot: boolean): { line: HTMLElement; label: HTMLElement } {
  const line = el('div', 'flex flex-wrap items-center gap-x-3 gap-y-1 text-sm');
  const label = el('span', `inline-flex items-center gap-2 font-medium ${TONE[tone]}`);
  if (dot) label.append(el('span', 'size-2 shrink-0 rounded-full bg-current'));
  const t = el('span', '', text);
  label.append(t);
  line.append(label);
  return { line, label: t };
}

function makeBar(): { wrap: HTMLElement; set: (pct: number | null) => void } {
  const wrap = el('div', 'relative h-2 overflow-hidden rounded-full bg-white/15');
  wrap.setAttribute('role', 'progressbar');
  wrap.setAttribute('aria-label', 'Progression');
  wrap.setAttribute('aria-valuemin', '0');
  wrap.setAttribute('aria-valuemax', '100');
  const fill = el('div');
  wrap.append(fill);
  const set = (pct: number | null): void => {
    if (pct == null) {
      wrap.removeAttribute('aria-valuenow');
      fill.className = 'absolute inset-y-0 w-2/5 animate-indeterminate rounded-full bg-fg motion-reduce:left-0 motion-reduce:w-full motion-reduce:animate-none motion-reduce:opacity-50';
      fill.style.width = '';
      return;
    }
    wrap.setAttribute('aria-valuenow', String(Math.round(pct)));
    fill.className = 'h-full rounded-full bg-fg transition-[width] duration-200 motion-reduce:transition-none';
    fill.style.width = `${pct}%`;
  };
  return { wrap, set };
}

function assistBox(): HTMLElement {
  const box = el('div', 'flex items-start gap-3 rounded-lg border-2 border-fg bg-raised px-4 py-3 text-sm font-medium text-fg');
  box.setAttribute('role', 'status');
  box.append(
    svgIcon('M3 5h18v14H3zM3 9h18M7 7h.01M10 7h.01', 'mt-0.5 size-5 shrink-0'),
    el('p', '', "Une fenêtre de navigateur vient de s'ouvrir : lancez la lecture de la vidéo, Ursa la récupère automatiquement."),
  );
  return box;
}

function toggler(id: number, key: string, label: string, panel: HTMLElement): HTMLButtonElement {
  const k = `${id}:${key}`;
  const b = el('button', 'disclosure h-8 px-2', label);
  b.type = 'button';
  const sync = (): void => {
    const open = openPanels.has(k);
    panel.hidden = !open;
    b.setAttribute('aria-expanded', String(open));
  };
  b.addEventListener('click', () => {
    if (openPanels.has(k)) openPanels.delete(k);
    else openPanels.add(k);
    sync();
  });
  sync();
  return b;
}

function infoRows(rows: [string, string | HTMLElement][]): HTMLElement {
  const dl = el('dl', 'grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5');
  for (const [k, v] of rows) {
    const dd = el('dd', 'min-w-0 break-words tabular-nums text-fg');
    if (typeof v === 'string') dd.textContent = v;
    else dd.append(v);
    dl.append(el('dt', 'text-muted', k), dd);
  }
  return dl;
}

function reportPanel(r: GapReport): HTMLElement {
  const body = el('div', 'flex flex-col gap-3 rounded-lg border border-border bg-code p-4 text-sm');
  const head = el('div', 'flex items-start gap-2.5');
  // Hibou décoratif : cloné depuis le gabarit rendu par Astro.
  const owl = document.getElementById('owl-template')?.firstElementChild?.cloneNode(true);
  if (owl instanceof SVGElement) {
    owl.setAttribute('class', 'mt-0.5 size-5 shrink-0 text-muted');
    head.append(owl);
  }
  head.append(el('p', 'text-fg', r.summary));
  body.append(head);

  body.append(
    infoRows([
      ['Durée attendue', r.expectedDuration != null ? formatDuration(r.expectedDuration) : 'inconnue'],
      ['Durée réelle', formatDuration(r.actualDuration)],
      ...(r.avDrift != null ? ([['Écart audio/vidéo', formatGap(r.avDrift)]] as [string, string][]) : []),
    ]),
  );

  if (r.gaps.length) {
    body.append(el('p', 'font-medium text-fg', `Coupures détectées (${r.gaps.length})`));
    const ul = el('ul', 'flex flex-col gap-1');
    for (const g of r.gaps) {
      const seg = g.segmentIndex != null ? ` — segment ${g.segmentIndex}` : '';
      ul.append(el('li', 'font-mono text-xs text-muted', `${formatTimecode(g.at)} : ${formatGap(g.missing)} manquantes, piste ${TRACK[g.stream]}${seg}`));
    }
    body.append(ul);
  }
  if (r.badSegments.length) {
    body.append(el('p', 'font-medium text-fg', `Segments abîmés (${r.badSegments.length})`));
    const ul = el('ul', 'flex flex-col gap-1');
    for (const s of r.badSegments) {
      ul.append(el('li', 'font-mono text-xs text-muted', `${TRACK[s.track]} n° ${s.index} : ${s.reason}`));
    }
    body.append(ul);
  }
  return body;
}

/** Première ligne lisible d'une erreur, et texte complet s'il y a plus à lire. */
function splitError(raw: string | null): { first: string; full: string | null } {
  const text = (raw ?? '').trim() || 'Erreur inconnue.';
  const lines = text.split('\n').map((l) => l.trim()).filter(Boolean);
  const head = lines[0] ?? text;
  const first = head.length > 180 ? `${head.slice(0, 177)}…` : head;
  return { first, full: lines.length > 1 || head.length > 180 ? text : null };
}

// ---------- Carte d'un téléchargement ----------

function signature(d: Download, paused: boolean): string {
  const common = [d.status, d.filename, d.url, d.quality, d.resolvedVia, d.retryCount, d.mediaKind];
  if (d.status === 'running') return JSON.stringify(common);
  return JSON.stringify([...common, d.error, d.finishedAt, d.totalBytes, d.downloadedBytes, d.outputPath, d.report, d.status === 'pending' && paused]);
}

function buildCard(d: Download, paused: boolean): Card {
  const root = el('article', 'panel flex flex-col gap-3 !p-4');
  const card: Card = { root, sig: signature(d, paused) };
  const running = d.status === 'running';
  const finished = d.status === 'done' || d.status === 'warning';

  // Ligne 1 : nom et action principale.
  const head = el('div', 'flex items-center justify-between gap-3');
  const title = el('div', 'min-w-0 flex-1 truncate text-base font-medium', name(d));
  title.title = d.url;
  head.append(title);
  if (running) head.append(btn('Annuler', BTN_DANGER, () => run(() => queueCancel(d.id))));
  else if (d.status === 'pending') head.append(btn('Retirer', BTN, () => run(() => queueRemove(d.id))));
  else if (d.status === 'error') head.append(btn('Relancer', BTN, () => run(() => queueRetry(d.id))));
  else
    head.append(
      btn('Afficher dans le dossier', BTN, async () => {
        try {
          await revealDownload(d.id);
        } catch (e) {
          toast(errMsg(e), 'err');
        }
      }),
    );
  root.append(head);

  // Ligne 2 : état en clair (+ progression ou résumé).
  const details = el('div', 'flex flex-col gap-3 rounded-lg border border-border bg-code p-4 text-sm');
  let statusRow: HTMLElement;
  let segValue: HTMLElement | null = null;
  const detailRows: [string, string | HTMLElement][] = [['Lien', el('span', 'break-all font-mono text-xs', d.url)]];
  if (d.outputPath) detailRows.push(['Fichier', el('span', 'break-all font-mono text-xs', d.outputPath)]);
  if (d.quality) detailRows.push(['Qualité', d.quality]);
  if (d.resolvedVia) detailRows.push(['Origine', VIA[d.resolvedVia]]);
  if (running) {
    segValue = el('span', '', '—');
    detailRows.push(['Segments', segValue]);
  } else if (d.segmentsTotal != null) {
    detailRows.push(['Segments', `${d.segmentsDone ?? 0} / ${d.segmentsTotal}`]);
  }
  if (d.retryCount > 0) detailRows.push(['Tentative', String(d.retryCount + 1)]);

  if (running) {
    const { line, label } = statusLine('Préparation…', 'run', false);
    const meta = el('span', 'tabular-nums text-muted');
    line.append(meta);
    const bar = makeBar();
    const box = assistBox();
    box.hidden = !assist.has(d.id);
    statusRow = line;
    root.append(line, bar.wrap, box);
    card.setAssist = (on) => {
      box.hidden = !on;
    };
    card.paint = (p) => {
      const pct = Math.max(0, Math.min(100, p.progressPercent || 0));
      const phase = p.phase;
      label.textContent = phase === 'downloading' ? `Téléchargement… ${Math.round(pct)} %` : phase ? PHASES[phase] : 'Préparation…';
      const determinate = phase === 'downloading' || ((phase === 'verifying' || phase === 'repairing' || phase === 'assembling') && pct > 0);
      bar.set(determinate ? pct : null);
      if (phase === 'downloading') {
        const parts = [p.totalBytes != null ? `${formatBytes(p.downloadedBytes)} sur ${formatBytes(p.totalBytes)}` : formatBytes(p.downloadedBytes)];
        if (p.speedBps != null) parts.push(formatSpeed(p.speedBps));
        if (p.etaSeconds != null) parts.push(`encore ${formatDuration(p.etaSeconds)}`);
        meta.textContent = parts.join(' · ');
      } else {
        meta.textContent = '';
      }
      if (segValue) segValue.textContent = p.segmentsTotal != null ? `${p.segmentsDone ?? 0} / ${p.segmentsTotal}` : '—';
    };
    card.paint(d);
  } else if (d.status === 'pending') {
    statusRow = statusLine(paused ? 'En attente — file en pause' : 'En attente', 'wait', true).line;
    root.append(statusRow);
  } else if (d.status === 'error') {
    const { first, full } = splitError(d.error);
    statusRow = statusLine('Échec', 'err', true).line;
    root.append(statusRow);
    root.append(el('p', 'rounded-lg bg-danger/15 px-3 py-2 text-sm text-fg', first));
    if (full) {
      detailRows.push(['Message complet', el('span', 'whitespace-pre-wrap break-words font-mono text-xs', full)]);
    }
  } else {
    const warn = d.status === 'warning';
    const { line } = statusLine(warn ? 'Terminé — coupures détectées' : 'Terminé — vidéo complète', warn ? 'warn' : 'ok', true);
    const bytes = d.totalBytes ?? d.downloadedBytes;
    const meta = [formatDate(d.finishedAt), d.quality ?? '', bytes ? formatBytes(bytes) : ''].filter(Boolean);
    if (meta.length) line.append(el('span', 'tabular-nums text-muted', meta.join(' · ')));
    statusRow = line;
    root.append(line);
  }
  details.append(infoRows(detailRows));

  // Actions secondaires à gauche et panneaux repliables à droite : sur une ligne à part s'il y a des actions,
  // sinon à droite de la ligne d'état.
  const secondary = el('div', 'flex flex-wrap items-center gap-x-1');
  if (d.status === 'error') secondary.append(btn('Supprimer', BTN_TEXT_DANGER, () => run(() => queueRemove(d.id))));
  if (finished) {
    secondary.append(
      btn('Revérifier', BTN_TEXT, async () => {
        toast('Vérification en cours…');
        await run(() => verifyAgain(d.id));
      }),
      btn('Retirer de la liste', BTN_TEXT, () => run(() => queueRemove(d.id))),
    );
  }
  const toggles = el('div', 'ml-auto flex flex-wrap items-center');
  let reportEl: HTMLElement | null = null;
  if (d.report) {
    const rp = reportPanel(d.report);
    reportEl = rp;
    toggles.append(toggler(d.id, 'report', "Rapport d'intégrité", rp));
  }
  toggles.append(toggler(d.id, 'details', 'Détails', details));
  if (secondary.childElementCount) {
    const row = el('div', 'flex flex-wrap items-center gap-x-2 gap-y-1');
    row.append(secondary, toggles);
    root.append(row);
  } else {
    statusRow.append(toggles);
  }
  // Panneaux repliables : masqués (donc sans espace) tant qu'ils sont fermés.
  if (reportEl) root.append(reportEl);
  root.append(details);
  return card;
}

// ---------- Liste ----------

function order(items: Download[]): Download[] {
  const finishedAt = (d: Download): number => d.finishedAt ?? d.createdAt;
  const rank = (d: Download): number => (d.status === 'running' ? 0 : d.status === 'error' ? 1 : d.status === 'pending' ? 2 : 3);
  return items
    .filter((d) => d.status !== 'cancelled')
    .sort((a, b) => rank(a) - rank(b) || (rank(a) === 3 ? finishedAt(b) - finishedAt(a) : 0));
}

function renderList(items: Download[], paused: boolean): void {
  const shown = order(items);
  const wanted: HTMLElement[] = [];
  const seen = new Set<number>();
  for (const d of shown) {
    seen.add(d.id);
    // Une aide devenue sans objet (l'élément n'est plus en recherche) est retirée.
    if (assist.has(d.id) && (d.status !== 'running' || (d.phase != null && d.phase !== 'resolving'))) assist.delete(d.id);
    let card = cards.get(d.id);
    if (!card || card.sig !== signature(d, paused)) {
      card = buildCard(d, paused);
      cards.set(d.id, card);
    }
    card.paint?.(d);
    card.setAssist?.(assist.has(d.id));
    wanted.push(card.root);
  }
  for (const id of [...cards.keys()]) {
    if (seen.has(id)) continue;
    cards.delete(id);
    assist.delete(id);
    for (const k of [...openPanels]) if (k.startsWith(`${id}:`)) openPanels.delete(k);
  }
  const list = $('dl-list');
  const current = Array.from(list.children);
  if (current.length !== wanted.length || current.some((c, i) => c !== wanted[i])) list.replaceChildren(...wanted);
  $('dl-empty').hidden = wanted.length > 0;
}

function paintToolbar(items: Download[], paused: boolean): void {
  const active = items.some((d) => d.status === 'running' || d.status === 'pending');
  const pause = $('q-pause');
  pause.hidden = !active && !paused;
  pause.textContent = paused ? 'Reprendre' : 'Mettre en pause';
  $('q-paused').hidden = !paused;
  $('done-clear').hidden = !items.some((d) => d.status === 'done' || d.status === 'warning');
}

// ---------- État global ----------

const lastStatus = new Map<number, string>();

function notify(items: Download[]): void {
  for (const d of items) {
    const prev = lastStatus.get(d.id);
    if (prev && prev !== d.status) {
      if (d.status === 'done') toast(`${name(d)} : téléchargement terminé`, 'ok');
      else if (d.status === 'warning') toast(`${name(d)} : terminé, des coupures ont été détectées`, 'info', 7000);
      else if (d.status === 'error') toast(`${name(d)} : échec du téléchargement`, 'err', 7000);
    }
    lastStatus.set(d.id, d.status);
  }
}

function applyQueue(q: QueueState): void {
  state.queue = q;
  notify(q.items);
  paintToolbar(q.items, q.paused);
  renderList(q.items, q.paused);
}

function onProgress(p: ProgressEvent): void {
  const d = state.queue.items.find((x) => x.id === p.id);
  if (!d) return;
  Object.assign(d, p);
  const card = cards.get(p.id);
  card?.paint?.(d);
  if (assist.has(p.id) && p.phase && p.phase !== 'resolving') {
    assist.delete(p.id);
    card?.setAssist?.(false);
  }
}

function onAssist(a: DetectAssist): void {
  if (a.active) assist.add(a.id);
  else assist.delete(a.id);
  cards.get(a.id)?.setAssist?.(a.active);
}

// ---------- Formulaire ----------

const linesOf = (): string[] =>
  $<HTMLTextAreaElement>('add-urls')
    .value.split('\n')
    .map((l) => l.trim())
    .filter(Boolean);

function refreshForm(): void {
  const box = $<HTMLTextAreaElement>('add-urls');
  const n = linesOf().length;
  box.style.height = 'auto';
  box.style.height = `${Math.min(Math.max(box.scrollHeight + 2, 80), 240)}px`;
  const hint = $('add-hint');
  hint.hidden = n === 0;
  hint.textContent = n > 1 ? `Ctrl + Entrée pour ajouter les ${n} liens` : 'Entrée pour lancer';
  const f = $<HTMLInputElement>('add-filename');
  f.disabled = n > 1;
  $('add-filename-help').textContent = n > 1 ? 'Disponible pour un seul lien.' : 'Sans extension. Pour un seul lien.';
}

async function submit(e: Event): Promise<void> {
  e.preventDefault();
  const urls = linesOf();
  if (!urls.length) {
    toast('Collez au moins un lien.', 'err');
    $('add-urls').focus();
    return;
  }
  if (!state.tauri) {
    toast("Le téléchargement n'est disponible que dans l'application Ursa.", 'err');
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
    refreshForm();
    toast(urls.length > 1 ? `${urls.length} liens ajoutés` : 'Lien ajouté', 'ok');
  } catch (err) {
    toast(errMsg(err), 'err');
  } finally {
    submitBtn.disabled = !state.tauri;
  }
}

export async function initDownloads(): Promise<void> {
  const form = $<HTMLFormElement>('add-form');
  form.addEventListener('submit', (e) => void submit(e));
  const urlsBox = $<HTMLTextAreaElement>('add-urls');
  urlsBox.addEventListener('input', refreshForm);
  // Entrée valide s'il n'y a qu'une ligne ; Ctrl+Entrée valide toujours ; Maj+Entrée ajoute une ligne.
  urlsBox.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
    const n = linesOf().length;
    if (e.ctrlKey || e.metaKey || n === 1) {
      e.preventDefault();
      form.requestSubmit();
    } else if (n === 0) {
      e.preventDefault();
    }
  });
  const optionsBtn = $('add-options-toggle');
  optionsBtn.addEventListener('click', () => {
    const open = optionsBtn.getAttribute('aria-expanded') !== 'true';
    optionsBtn.setAttribute('aria-expanded', String(open));
    $('add-options').hidden = !open;
  });
  refreshForm();

  $('q-pause').addEventListener('click', () => void run(() => queueSetPaused(!state.queue.paused)));
  $('q-open').addEventListener('click', async () => {
    try {
      await openDestDir();
    } catch (e) {
      toast(errMsg(e), 'err');
    }
  });
  $('done-clear').addEventListener('click', () => void run(queueClearFinished));

  for (const id of ['add-submit', 'q-pause', 'q-open', 'done-clear']) $<HTMLButtonElement>(id).disabled = !state.tauri;
  applyQueue(state.queue);
  if (!state.tauri) return;
  await onQueueUpdate(applyQueue);
  await onDownloadProgress(onProgress);
  await onDetectAssist(onAssist);
  try {
    const q = await queueState();
    for (const d of q.items) lastStatus.set(d.id, d.status);
    applyQueue(q);
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}
