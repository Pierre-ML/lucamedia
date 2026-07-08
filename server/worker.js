import { spawn } from 'node:child_process';
import { join } from 'node:path';
import { config } from './config.js';
import { queries } from './db.js';
import { broadcast } from './sse.js';

let currentChild = null;
let currentId = null;
let retryTimer = null;

const PROGRESS_RE =
  /^\[download\]\s+(\d+(?:\.\d+)?)%\s+of\s+~?\s*([\d.]+)(KiB|MiB|GiB|TiB|B)\b(?:.*?ETA\s+([\d:]+))?/;

const SIZE_TO_MB = { B: 1 / 1024 ** 2, KiB: 1 / 1024, MiB: 1, GiB: 1024, TiB: 1024 ** 2 };

function etaToSeconds(eta) {
  if (!eta) return null;
  return eta.split(':').reduce((total, part) => total * 60 + Number(part), 0);
}

// ---- Console : tampon des dernières lignes de sortie yt-dlp ----
// stream : 'out' (stdout), 'err' (stderr), 'app' (messages du worker lui-même)

const LOG_LIMIT = 300;
const logs = [];
let lastWasProgress = false;

function pushLog(stream, line) {
  if (!line.trim()) return;
  const isProgress = PROGRESS_RE.test(line);
  const entry = { stream, line };
  // Les lignes de progression successives se remplacent (comme un vrai terminal avec \r)
  const replace = isProgress && lastWasProgress && logs.length > 0;
  if (replace) {
    logs[logs.length - 1] = entry;
  } else {
    logs.push(entry);
    if (logs.length > LOG_LIMIT) logs.shift();
  }
  lastWasProgress = isProgress;
  broadcast('log', { stream, line, replace });
}

export function getLogs() {
  return logs;
}

export function queueSnapshot() {
  const all = queries.getAllDownloads();
  return {
    state: queries.isPaused() ? 'pause' : 'play',
    outputDir: config.outputDir,
    current: all.find((d) => d.status === 'downloading') ?? null,
    pending: all.filter((d) => d.status === 'pending'),
    errors: all.filter((d) => d.status === 'error'),
    done: all.filter((d) => d.status === 'done').reverse(),
  };
}

export function broadcastQueue() {
  broadcast('queue', queueSnapshot());
}

export function isDownloading(id) {
  return currentId === Number(id);
}

function buildArgs(item) {
  const args = [
    '-o', join(config.outputDir, `${item.filename}.%(ext)s`),
    // Meilleure qualité possible : meilleure vidéo + meilleur audio, sinon meilleur flux combiné
    '-f', 'bv*+ba/b',
    // Les hébergeurs de streaming ont souvent des certificats TLS cassés (CERTIFICATE_VERIFY_FAILED)
    '--no-check-certificates',
    '--newline',
  ];
  if (item.is_uqload) args.push('--referer', config.uqloadReferer);
  args.push(item.url);
  return args;
}

function launch(item) {
  currentId = item.id;
  queries.setStatus(item.id, 'downloading');
  broadcastQueue();

  const args = buildArgs(item);
  pushLog('app', `$ ${config.ytdlpPath} ${args.map((a) => (/\s/.test(a) ? `"${a}"` : a)).join(' ')}`);

  const child = spawn(config.ytdlpPath, args, {
    windowsHide: true,
    env: { ...process.env, PYTHONIOENCODING: 'utf-8' },
  });
  currentChild = child;

  let stderrTail = '';
  let lastUpdate = 0;
  let stdoutBuffer = '';
  let stderrBuffer = '';

  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');

  child.stdout.on('data', (chunk) => {
    stdoutBuffer += chunk;
    const lines = stdoutBuffer.split(/\r?\n/);
    stdoutBuffer = lines.pop();
    for (const line of lines) {
      pushLog('out', line);
      const match = PROGRESS_RE.exec(line);
      if (!match) continue;
      const progress = {
        progress_percent: Number(match[1]),
        size_estimate_mb:
          Math.round(Number(match[2]) * SIZE_TO_MB[match[3]] * 100) / 100,
        eta_seconds: etaToSeconds(match[4]),
      };
      // On limite les écritures DB / événements SSE à 2 par seconde
      const now = Date.now();
      if (now - lastUpdate >= 500 || progress.progress_percent >= 100) {
        lastUpdate = now;
        queries.setProgress(item.id, progress);
        broadcast('progress', { id: item.id, retry_count: item.retry_count, ...progress });
      }
    }
  });

  child.stderr.on('data', (chunk) => {
    // yt-dlp écrit ses avertissements et erreurs sur stderr ; on garde la fin
    stderrTail = (stderrTail + chunk).slice(-2000);
    stderrBuffer += chunk;
    const lines = stderrBuffer.split(/\r?\n/);
    stderrBuffer = lines.pop();
    for (const line of lines) pushLog('err', line);
  });

  child.on('error', (err) => {
    // Échec du spawn lui-même (yt-dlp introuvable) : inutile de réessayer
    currentChild = null;
    currentId = null;
    const message = `Impossible de lancer yt-dlp (${config.ytdlpPath}) : ${err.message}`;
    pushLog('app', message);
    queries.markError(item.id, message);
    broadcastQueue();
    tick();
  });

  child.on('close', (code) => {
    if (currentChild !== child) return; // déjà géré par 'error'
    currentChild = null;
    currentId = null;

    if (code === 0) {
      queries.setProgress(item.id, {
        progress_percent: 100,
        eta_seconds: 0,
        size_estimate_mb: queries.getDownload(item.id)?.size_estimate_mb ?? null,
      });
      queries.setStatus(item.id, 'done');
      pushLog('app', `Terminé : ${item.filename}`);
      broadcastQueue();
      tick();
      return;
    }

    const message = stderrTail.trim().split('\n').slice(-5).join('\n') || `yt-dlp a quitté avec le code ${code}`;
    const fresh = queries.getDownload(item.id);
    if (!fresh) {
      // supprimé entre-temps
      tick();
      return;
    }

    queries.incrementRetry(item.id, message);

    if (fresh.retry_count + 1 < config.maxRetries) {
      // On relance la même entrée (elle ne repart pas en fin de file),
      // avec un court délai pour éviter de boucler à chaud
      pushLog(
        'app',
        `yt-dlp s'est arrêté (code ${code}) — nouvelle tentative ${fresh.retry_count + 2}/${config.maxRetries} dans 3 s`
      );
      broadcastQueue();
      retryTimer = setTimeout(() => {
        retryTimer = null;
        const again = queries.getDownload(item.id);
        if (again && again.status === 'downloading' && !currentChild) launch(again);
        else tick();
      }, 3000);
    } else {
      pushLog('app', `Échec définitif après ${config.maxRetries} tentatives : ${item.filename}`);
      queries.markError(item.id, message);
      broadcastQueue();
      tick(); // on passe immédiatement au prochain élément de la file
    }
  });
}

export function tick() {
  if (currentChild || retryTimer) return;
  if (queries.isPaused()) return;
  const next = queries.getNextPending();
  if (next) launch(next);
}

process.on('exit', () => {
  if (currentChild) currentChild.kill();
});
