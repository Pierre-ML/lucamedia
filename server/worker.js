import { spawn, execFile } from 'node:child_process';
import { join } from 'node:path';
import { config } from './config.js';
import { queries } from './db.js';
import { broadcast } from './sse.js';
import { makeProxyUrls } from './hls-proxy.js';

const { playlistProxyUrl } = makeProxyUrls(`http://localhost:${config.port}`);

let currentChild = null;
let currentId = null;
let retryTimer = null;

const PROGRESS_RE =
  /^\[download\]\s+(\d+(?:\.\d+)?)%\s+of\s+~?\s*([\d.]+)(KiB|MiB|GiB|TiB|B)\b(?:.*?ETA\s+([\d:]+))?/;

// Progression ffmpeg (téléchargeur HLS) : "size= 12345KiB time=00:01:23.45 bitrate=... speed=1.5x"
const FF_DURATION_RE = /\bDuration:\s*(\d+):(\d{2}):(\d{2})/;
const FF_TIME_RE = /\btime=(\d{1,3}):(\d{2}):(\d{2})(?:\.\d+)?/;
const FF_SIZE_RE = /\b(?:L)?size=\s*(\d+)\s*(KiB|kB|MiB)/i;
const FF_SPEED_RE = /\bspeed=\s*([\d.]+)x/;
const FF_PROGRESS_RE = /\btime=\d{1,3}:\d{2}:\d{2}.*?\bbitrate=/;

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
  const isProgress = PROGRESS_RE.test(line) || FF_PROGRESS_RE.test(line);
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

// Vérifie la présence des outils au démarrage et l'affiche dans la console de l'interface
export function checkTools() {
  execFile(config.ytdlpPath, ['--version'], { windowsHide: true }, (err, stdout) => {
    pushLog(
      err ? 'err' : 'app',
      err
        ? `yt-dlp introuvable (${config.ytdlpPath}) — les téléchargements échoueront`
        : `yt-dlp ${stdout.trim()} détecté`
    );
  });
  execFile('ffmpeg', ['-version'], { windowsHide: true }, (err, stdout) => {
    const version = stdout?.match(/ffmpeg version (\S+)/)?.[1]?.split('-')[0] ?? '?';
    pushLog(
      err ? 'err' : 'app',
      err
        ? 'ffmpeg introuvable — les fragments ne seront pas assemblés en MP4 (installez-le : winget install Gyan.FFmpeg)'
        : `ffmpeg ${version} détecté — assemblage des fragments en MP4 actif`
    );
  });
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

// Certains CDN renvoient 403 quand la requête ne ressemble pas à celle d'un navigateur
export const BROWSER_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36';

// Un lien .m3u8 direct passe par le proxy local (nettoyage des segments empoisonnés) ;
// un lien de page (embed) est laissé à l'extraction de yt-dlp
function isDirectM3u8(url) {
  return /\.m3u8(\?|$)/i.test(url);
}

function buildArgs(item) {
  // Referer : celui d'uqload si la case est cochée, sinon l'origine du lien lui-même
  let referer = config.uqloadReferer;
  if (!item.is_uqload) {
    try {
      referer = new URL(item.url).origin + '/';
    } catch {
      referer = '';
    }
  }

  // Lien .m3u8 direct : on le remplace par l'URL du proxy, qui reçoit l'original + le referer.
  // ffmpeg téléchargera alors playlist et segments nettoyés depuis localhost.
  const useProxy = isDirectM3u8(item.url);
  const targetUrl = useProxy ? playlistProxyUrl(item.url, referer) : item.url;

  const args = [
    '-o', join(config.outputDir, `${item.filename}.%(ext)s`),
    // Meilleure qualité possible : meilleure vidéo + meilleur audio, sinon meilleur flux combiné
    '-f', 'bv*+ba/b',
    // ffmpeg assemble les fragments : conteneur MP4 propre quand vidéo et audio sont fusionnés
    '--merge-output-format', 'mp4',
    // ffmpeg télécharge lui-même les flux HLS : flux continu aux timestamps corrigés,
    // ce qui évite les désynchros image/son et coupures audio aux jonctions de fragments
    '--downloader', 'm3u8:ffmpeg',
    '--downloader-args', 'ffmpeg_i:-fflags +genpts',
    // Les hébergeurs de streaming ont souvent des certificats TLS cassés (CERTIFICATE_VERIFY_FAILED)
    '--no-check-certificates',
    '--user-agent', BROWSER_UA,
    '--newline',
  ];
  // Sans proxy (lien de page/embed), on transmet referer + origin à yt-dlp lui-même.
  // Avec proxy, ces en-têtes sont gérés par le proxy vers l'hôte réel ; inutile ici.
  if (referer && !useProxy) {
    args.push('--referer', referer);
    args.push('--add-header', `Origin: ${referer.replace(/\/$/, '')}`);
  }
  args.push(targetUrl);
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
  let ffDurationSec = null; // durée totale annoncée par ffmpeg, pour calculer le %

  // On limite les écritures DB / événements SSE à 2 par seconde
  const reportProgress = (progress) => {
    const now = Date.now();
    if (now - lastUpdate >= 500 || (progress.progress_percent ?? 0) >= 100) {
      lastUpdate = now;
      queries.setProgress(item.id, progress);
      broadcast('progress', { id: item.id, retry_count: item.retry_count, ...progress });
    }
  };

  // ffmpeg n'affiche pas de % : on le déduit de "time=" rapporté à la durée totale
  const parseFfmpegLine = (line) => {
    const duration = FF_DURATION_RE.exec(line);
    if (duration) {
      ffDurationSec = +duration[1] * 3600 + +duration[2] * 60 + +duration[3];
      return;
    }
    if (!ffDurationSec || !FF_PROGRESS_RE.test(line)) return;
    const time = FF_TIME_RE.exec(line);
    if (!time) return;
    const seconds = +time[1] * 3600 + +time[2] * 60 + +time[3];
    const ratio = Math.min(seconds / ffDurationSec, 0.999);
    const size = FF_SIZE_RE.exec(line);
    const speed = FF_SPEED_RE.exec(line);
    const downloadedMb = size ? Number(size[1]) * (size[2].toLowerCase() === 'mib' ? 1 : 1 / 1024) : null;
    reportProgress({
      progress_percent: Math.round(ratio * 1000) / 10,
      // taille totale estimée par extrapolation de ce qui est déjà écrit
      size_estimate_mb:
        downloadedMb && ratio > 0.01 ? Math.round((downloadedMb / ratio) * 100) / 100 : null,
      eta_seconds:
        speed && Number(speed[1]) > 0 ? Math.round((ffDurationSec - seconds) / Number(speed[1])) : null,
    });
  };

  child.stdout.setEncoding('utf8');
  child.stderr.setEncoding('utf8');

  child.stdout.on('data', (chunk) => {
    stdoutBuffer += chunk;
    const lines = stdoutBuffer.split(/\r\n|\r|\n/);
    stdoutBuffer = lines.pop();
    for (const line of lines) {
      pushLog('out', line);
      const match = PROGRESS_RE.exec(line);
      if (!match) continue;
      reportProgress({
        progress_percent: Number(match[1]),
        size_estimate_mb:
          Math.round(Number(match[2]) * SIZE_TO_MB[match[3]] * 100) / 100,
        eta_seconds: etaToSeconds(match[4]),
      });
    }
  });

  child.stderr.on('data', (chunk) => {
    // yt-dlp écrit ses avertissements sur stderr, et ffmpeg y écrit tout (dont sa progression)
    stderrTail = (stderrTail + chunk).slice(-2000);
    stderrBuffer += chunk;
    const lines = stderrBuffer.split(/\r\n|\r|\n/);
    stderrBuffer = lines.pop();
    for (const line of lines) {
      pushLog(/^\s*(ERROR|WARNING)/i.test(line) ? 'err' : 'out', line);
      parseFfmpegLine(line);
    }
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

    // Un 403 signale un jeton de lien mort (expiré ou invalide) : réessayer ne le ressuscite
    // pas, et chaque minute perdue fait expirer les jetons des liens suivants dans la file
    const is403 = /403|Forbidden/i.test(message);
    const maxAttempts = is403 ? Math.min(3, config.maxRetries) : config.maxRetries;

    if (fresh.retry_count + 1 < maxAttempts) {
      // On relance la même entrée (elle ne repart pas en fin de file),
      // avec un court délai pour éviter de boucler à chaud
      pushLog(
        'app',
        `yt-dlp s'est arrêté (code ${code}) — nouvelle tentative ${fresh.retry_count + 2}/${maxAttempts} dans 3 s`
      );
      broadcastQueue();
      retryTimer = setTimeout(() => {
        retryTimer = null;
        const again = queries.getDownload(item.id);
        if (again && again.status === 'downloading' && !currentChild) launch(again);
        else tick();
      }, 3000);
    } else {
      pushLog('app', `Échec définitif après ${maxAttempts} tentatives : ${item.filename}`);
      let finalMessage = message;
      if (is403) {
        const hint =
          'Erreur 403 : lien expiré — ces liens .m3u8 signés ne vivent que quelques heures. Récupérez un lien frais, ou collez plutôt le lien de la page du lecteur (embed) : il sera résolu au moment du téléchargement';
        pushLog('app', hint);
        finalMessage = `${message}\n${hint}`;
      }
      queries.markError(item.id, finalMessage);
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
