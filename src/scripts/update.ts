// Mises à jour : vérification silencieuse au démarrage (bandeau), section « Mises à jour » des Réglages
// et installation via l'updater (progression, boutons désactivés, redémarrage automatique).
import { appCheckUpdate, appInstallUpdate, onUpdateProgress, errMsg, type UpdateInfo } from './api';
import { toast } from './toast';
import { state } from './state';

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

let known: UpdateInfo | null = null;
let checking = false;
let installing = false;

const mb = (n: number) => (n / (1024 * 1024)).toFixed(1).replace('.', ',');

function setStatus(text: string, tone: 'muted' | 'ok' | 'accent' | 'err' = 'muted'): void {
  const el = $('upd-status');
  el.textContent = text;
  el.className = `text-sm ${tone === 'ok' ? 'text-success' : tone === 'accent' ? 'text-fg' : tone === 'err' ? 'text-danger' : 'text-muted'}`;
}

function render(): void {
  const available = !!known?.available;
  $('upd-banner').hidden = !available || bannerDismissed || installing;
  if (available && known) $('upd-banner-text').textContent = `Mise à jour v${known.latest} disponible`;
  $('upd-notes').hidden = !known?.url;
  $<HTMLButtonElement>('upd-install').hidden = !available;
  $<HTMLButtonElement>('upd-banner-notes').hidden = !known?.url;
  const busy = checking || installing;
  for (const id of ['upd-check', 'upd-install', 'upd-banner-install', 'upd-banner-later']) {
    $<HTMLButtonElement>(id).disabled = busy || !state.tauri;
  }
}

let bannerDismissed = false;

async function check(silent: boolean): Promise<void> {
  if (checking || installing) return;
  checking = true;
  if (!silent) setStatus('Vérification…');
  render();
  try {
    known = await appCheckUpdate();
    $('upd-current').textContent = `v${known.current}`;
    if (known.available) setStatus(`Version v${known.latest} disponible`, 'accent');
    else if (!silent) setStatus('Vous utilisez la dernière version.', 'ok');
    else setStatus('');
  } catch (e) {
    if (!silent) {
      setStatus('');
      toast(errMsg(e), 'err');
    }
  } finally {
    checking = false;
    render();
  }
}

async function install(): Promise<void> {
  if (installing || !known?.available) return;
  installing = true;
  render();
  const box = $('upd-progress');
  const bar = $<HTMLProgressElement>('upd-progress-bar');
  const text = $('upd-progress-text');
  box.hidden = false;
  bar.removeAttribute('value');
  text.textContent = 'Téléchargement de la mise à jour…';
  const unlisten = await onUpdateProgress(({ downloaded, total }) => {
    if (total) {
      const ratio = Math.min(1, downloaded / total);
      bar.value = ratio;
      text.textContent = `Téléchargement… ${Math.round(ratio * 100)} % (${mb(downloaded)} / ${mb(total)} Mo)`;
    } else {
      bar.removeAttribute('value');
      text.textContent = `Téléchargement… ${mb(downloaded)} Mo`;
    }
  });
  try {
    await appInstallUpdate();
    bar.value = 1;
    text.textContent = "L'application va redémarrer…";
    toast("L'application va redémarrer…", 'info', 10000);
    // Succès : l'app se ferme d'elle-même ; les boutons restent désactivés.
  } catch (e) {
    installing = false;
    box.hidden = true;
    toast(errMsg(e), 'err', 7000);
  } finally {
    unlisten();
    render();
  }
}

async function openNotes(): Promise<void> {
  if (!known?.url) return;
  try {
    const { openUrl } = await import('@tauri-apps/plugin-opener');
    await openUrl(known.url);
  } catch (e) {
    toast(errMsg(e), 'err');
  }
}

export function initUpdate(): void {
  $('upd-check').addEventListener('click', () => void check(false));
  for (const id of ['upd-install', 'upd-banner-install']) $(id).addEventListener('click', () => void install());
  for (const id of ['upd-notes', 'upd-banner-notes']) $(id).addEventListener('click', () => void openNotes());
  $('upd-banner-later').addEventListener('click', () => {
    bannerDismissed = true;
    render();
  });
  render();
  if (!state.tauri) {
    setStatus('Indisponible hors de l’application.');
    return;
  }
  if (!import.meta.env.DEV) void check(true);
}
