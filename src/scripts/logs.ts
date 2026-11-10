// Écran Journal : console des lignes de log (historique puis événement `log`).
import { logsRecent, onLog, errMsg, type LogLine } from './api';
import { $, el } from './dom';
import { toast } from './toast';
import { state } from './state';

const MAX_LINES = 1000;
// Contraste d'au moins 4,5:1 sur le fond du journal ; le niveau est aussi écrit, pas seulement coloré.
const LEVEL: Record<LogLine['level'], { cls: string; tag: string }> = {
  info: { cls: 'text-muted', tag: '' },
  warn: { cls: 'text-warning', tag: 'avert. ' },
  error: { cls: 'text-danger', tag: 'erreur ' },
};

function updateEmpty(): void {
  $('log-empty').hidden = $('log-list').childElementCount > 0;
}

function addLine(l: LogLine): void {
  const box = $('log-list');
  const pinned = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
  const level = LEVEL[l.level] ?? LEVEL.info;
  const row = el('div', `${level.cls} break-words`);
  const ts = el('span', 'mr-2 text-faint', new Date(l.ts).toLocaleTimeString('fr-FR'));
  row.append(ts);
  if (l.downloadId != null) row.append(el('span', 'mr-2 text-faint', `#${l.downloadId}`));
  if (level.tag) row.append(el('span', 'font-bold', level.tag));
  row.append(document.createTextNode(l.message));
  box.append(row);
  while (box.childElementCount > MAX_LINES) box.firstElementChild?.remove();
  if (pinned) box.scrollTop = box.scrollHeight;
  updateEmpty();
}

export async function initLogs(): Promise<void> {
  $('log-clear').addEventListener('click', () => {
    $('log-list').replaceChildren();
    updateEmpty();
  });
  document.addEventListener('screen-shown', (e) => {
    if ((e as CustomEvent<string>).detail === 'logs') {
      const box = $('log-list');
      box.scrollTop = box.scrollHeight;
    }
  });
  updateEmpty();
  if (!state.tauri) return;
  // L'écoute démarre d'abord ; les lignes reçues pendant le chargement de l'historique sont mises en tampon.
  const buffer: LogLine[] = [];
  let loaded = false;
  await onLog((l) => (loaded ? addLine(l) : buffer.push(l)));
  try {
    const recent = await logsRecent();
    recent.forEach(addLine);
    const last = recent.at(-1);
    for (const l of buffer) if (!last || l.ts > last.ts) addLine(l);
  } catch (e) {
    toast(errMsg(e), 'err');
    buffer.forEach(addLine);
  }
  loaded = true;
}
