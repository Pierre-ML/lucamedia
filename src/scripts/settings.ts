// Écran Réglages : paramètres, état des outils, désinstallation.
import { getSettings, saveSettings, toolsStatus, appUninstall, errMsg, type Settings, type ToolsStatus } from './api';
import { $, el } from './dom';
import { toast } from './toast';
import { state } from './state';

function fill(s: Settings): void {
  $<HTMLInputElement>('set-dest').value = s.destDir;
  $<HTMLInputElement>('set-parallel').value = String(s.parallelSegments);
  $<HTMLInputElement>('set-retries').value = String(s.maxRetries);
  $<HTMLSelectElement>('set-browser').value = s.browser;
  $<HTMLInputElement>('set-show-browser').checked = s.showBrowser;
  $<HTMLInputElement>('set-verify').checked = s.verifyAfterDownload;
  $<HTMLInputElement>('set-repair').checked = s.autoRepair;
  syncRepair();
}

/** La réparation automatique n'a de sens que si la vérification est active. */
function syncRepair(): void {
  const repair = $<HTMLInputElement>('set-repair');
  repair.disabled = !state.tauri || !$<HTMLInputElement>('set-verify').checked;
}

function read(): Settings {
  const clamp = (v: number, a: number, b: number, d: number) =>
    Number.isFinite(v) ? Math.min(b, Math.max(a, Math.round(v))) : d;
  return {
    destDir: $<HTMLInputElement>('set-dest').value.trim(),
    parallelSegments: clamp(parseInt($<HTMLInputElement>('set-parallel').value, 10), 1, 32, 8),
    maxRetries: clamp(parseInt($<HTMLInputElement>('set-retries').value, 10), 1, 10, 5),
    browser: $<HTMLSelectElement>('set-browser').value as Settings['browser'],
    showBrowser: $<HTMLInputElement>('set-show-browser').checked,
    verifyAfterDownload: $<HTMLInputElement>('set-verify').checked,
    autoRepair: $<HTMLInputElement>('set-repair').checked,
  };
}

async function save(): Promise<void> {
  const b = $<HTMLButtonElement>('set-save');
  b.disabled = true;
  try {
    fill(await saveSettings(read()));
    toast('Réglages enregistrés', 'ok');
    void loadTools();
  } catch (e) {
    toast(errMsg(e), 'err');
  } finally {
    b.disabled = !state.tauri;
  }
}

async function uninstall(): Promise<void> {
  const b = $<HTMLButtonElement>('app-uninstall');
  try {
    const { ask } = await import('@tauri-apps/plugin-dialog');
    const ok = await ask("Désinstaller Ursa ? L'application va se fermer. Les vidéos téléchargées sont conservées.", {
      title: 'Désinstaller',
      kind: 'warning',
      okLabel: 'Désinstaller',
      cancelLabel: 'Annuler',
    });
    if (!ok) return;
    b.disabled = true;
    await appUninstall();
  } catch (e) {
    toast(errMsg(e), 'err');
    b.disabled = !state.tauri;
  }
}

// ---------- Outils ----------

function toolRow(label: string, ok: boolean, detail: string, path: string | null): HTMLElement {
  const row = el('div', 'flex items-center justify-between gap-3 py-3');
  const left = el('div', 'min-w-0');
  left.append(el('div', 'text-sm font-medium text-fg', label));
  const d = el('div', 'truncate font-mono text-xs text-muted', detail);
  if (path) d.title = path;
  left.append(d);
  const pill = el('span', `pill shrink-0 ${ok ? 'bg-success/15 text-success' : 'bg-danger/15 text-danger'}`, ok ? 'Disponible' : 'Introuvable');
  row.append(left, pill);
  return row;
}

function paintTools(t: ToolsStatus): void {
  const list = $('tools-list');
  list.replaceChildren(
    toolRow('ffmpeg', t.ffmpeg.available, t.ffmpeg.version ?? t.ffmpeg.path ?? '—', t.ffmpeg.path),
    toolRow('ffprobe', t.ffprobe.available, t.ffprobe.version ?? t.ffprobe.path ?? '—', t.ffprobe.path),
    toolRow('yt-dlp', t.ytdlp.available, t.ytdlp.version ?? t.ytdlp.path ?? '—', t.ytdlp.path),
    toolRow('Navigateur', t.browser.available, t.browser.name ?? t.browser.path ?? '—', t.browser.path),
  );
  const all = t.ffmpeg.available && t.ffprobe.available && t.ytdlp.available;
  $('tools-dot').className = `size-2.5 shrink-0 rounded-full ${all ? 'bg-success' : 'bg-danger'}`;
  $('tools-text').textContent = all ? 'Outils prêts' : 'Outil manquant';
}

async function loadTools(): Promise<void> {
  if (!state.tauri) {
    $('tools-list').replaceChildren(el('p', 'py-3 text-sm text-muted', 'Indisponible hors de l’application.'));
    $('tools-text').textContent = 'Outils indisponibles';
    return;
  }
  try {
    paintTools(await toolsStatus());
  } catch (e) {
    $('tools-dot').className = 'size-2.5 shrink-0 rounded-full bg-danger';
    $('tools-text').textContent = 'Outils : erreur';
    toast(errMsg(e), 'err');
  }
}

export async function initSettings(): Promise<void> {
  $('set-save').addEventListener('click', () => void save());
  $('app-uninstall').addEventListener('click', () => void uninstall());
  $('set-verify').addEventListener('change', syncRepair);
  $('tools-refresh').addEventListener('click', () => void loadTools());
  $('set-choose').addEventListener('click', async () => {
    try {
      const { open } = await import('@tauri-apps/plugin-dialog');
      const dir = await open({ directory: true, multiple: false, defaultPath: $<HTMLInputElement>('set-dest').value || undefined });
      if (typeof dir === 'string') $<HTMLInputElement>('set-dest').value = dir;
    } catch (e) {
      toast(errMsg(e), 'err');
    }
  });

  for (const id of ['set-choose', 'set-save', 'app-uninstall', 'tools-refresh']) $<HTMLButtonElement>(id).disabled = !state.tauri;
  syncRepair();
  void loadTools();
  if (!state.tauri) return;
  try {
    fill(await getSettings());
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}
