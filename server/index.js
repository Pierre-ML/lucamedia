import express from 'express';
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, basename } from 'node:path';
import { config } from './config.js';
import { queries } from './db.js';
import { addClient, broadcast } from './sse.js';
import { tick, queueSnapshot, broadcastQueue, getLogs, checkTools, BROWSER_UA } from './worker.js';
import { fetchBuffer, stripFakeHeader, rewritePlaylist, makeProxyUrls } from './hls-proxy.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const app = express();
app.use(express.json());

// --- Proxy HLS : nettoie les playlists/segments empoisonnés avant ffmpeg ---
// (segments vidéo déguisés en images ; voir hls-proxy.js)

function proxyHeaders(referer) {
  const headers = { 'User-Agent': BROWSER_UA };
  if (referer) {
    headers.Referer = referer;
    try {
      headers.Origin = new URL(referer).origin;
    } catch {
      /* referer non-URL : on n'ajoute pas d'Origin */
    }
  }
  return headers;
}

const { playlistProxyUrl, segmentProxyUrl } = makeProxyUrls(`http://localhost:${config.port}`);

app.get('/hls/list.m3u8', async (req, res) => {
  const { url, ref } = req.query;
  if (!/^https?:\/\//i.test(url ?? '')) return res.status(400).end('url invalide');
  try {
    const buf = await fetchBuffer(url, proxyHeaders(ref));
    const rewritten = rewritePlaylist(buf.toString('utf8'), url, {
      variantUrl: (u) => playlistProxyUrl(u, ref),
      segmentUrl: (u) => segmentProxyUrl(u, ref),
    });
    res.set('Content-Type', 'application/vnd.apple.mpegurl');
    res.end(rewritten);
  } catch (err) {
    res.status(502).end(`proxy playlist: ${err.message}`);
  }
});

app.get('/hls/seg.ts', async (req, res) => {
  const { url, ref } = req.query;
  if (!/^https?:\/\//i.test(url ?? '')) return res.status(400).end('url invalide');
  try {
    const buf = await fetchBuffer(url, proxyHeaders(ref));
    res.set('Content-Type', 'video/mp2t');
    res.end(stripFakeHeader(buf));
  } catch (err) {
    res.status(502).end(`proxy segment: ${err.message}`);
  }
});

// --- SSE ---
app.get('/api/events', (req, res) => {
  addClient(res);
});

// --- File d'attente ---
app.get('/api/queue', (req, res) => {
  res.json(queueSnapshot());
});

// --- Console : dernières lignes de sortie yt-dlp ---
app.get('/api/logs', (req, res) => {
  res.json(getLogs());
});

function sanitizeFilename(name) {
  return name
    .replace(/[<>:"/\\|?*\x00-\x1F]/g, '_')
    .replace(/[. ]+$/, '')
    .slice(0, 150)
    .trim();
}

function defaultFilename(url) {
  try {
    const segment = basename(new URL(url).pathname).replace(/\.[a-z0-9]+$/i, '');
    if (segment && segment !== '/') return sanitizeFilename(segment);
  } catch {
    // URL invalide : on retombe sur le timestamp
  }
  return `video-${Date.now()}`;
}

app.post('/api/queue', (req, res) => {
  const { items } = req.body ?? {};
  if (!Array.isArray(items) || items.length === 0) {
    return res.status(400).json({ error: 'items[] requis' });
  }

  const added = [];
  const rejected = [];
  for (const item of items) {
    const url = String(item?.url ?? '').trim();
    if (!/^https?:\/\//i.test(url)) {
      rejected.push(url || '(vide)');
      continue;
    }
    const filename = sanitizeFilename(String(item.filename ?? '').trim()) || defaultFilename(url);
    const result = queries.addDownload({ url, filename, is_uqload: Boolean(item.uqload) });
    added.push(Number(result.lastInsertRowid));
  }

  broadcastQueue();
  tick();
  res.status(added.length > 0 ? 201 : 400).json({ added, rejected });
});

app.delete('/api/queue/:id', (req, res) => {
  const row = queries.getDownload(req.params.id);
  if (!row) return res.status(404).json({ error: 'introuvable' });
  if (row.status === 'downloading') {
    return res.status(409).json({ error: 'téléchargement en cours, suppression impossible' });
  }
  queries.deleteDownload(req.params.id);
  broadcastQueue();
  res.json({ ok: true });
});

app.post('/api/queue/:id/retry', (req, res) => {
  const row = queries.getDownload(req.params.id);
  if (!row) return res.status(404).json({ error: 'introuvable' });
  if (row.status !== 'error') {
    return res.status(409).json({ error: 'seuls les éléments en erreur peuvent être relancés' });
  }
  queries.resetForRetry(row.id);
  broadcastQueue();
  tick();
  res.json({ ok: true });
});

// --- Pause / Play (global) ---
app.post('/api/control/pause', (req, res) => {
  queries.setPaused(true);
  broadcastQueue();
  res.json({ state: 'pause' });
});

app.post('/api/control/play', (req, res) => {
  queries.setPaused(false);
  broadcastQueue();
  tick();
  res.json({ state: 'play' });
});

// --- Utilitaires ---
app.post('/api/open-folder', (req, res) => {
  const opener =
    process.platform === 'win32' ? 'explorer' : process.platform === 'darwin' ? 'open' : 'xdg-open';
  spawn(opener, [config.outputDir], { detached: true, stdio: 'ignore' }).unref();
  res.json({ ok: true });
});

// --- En production : sert le build Astro ---
const dist = join(root, 'dist');
if (existsSync(dist)) {
  // Les assets de dist/_astro portent un hash dans leur nom : cache long et immuable
  app.use('/_astro', express.static(join(dist, '_astro'), { maxAge: '1y', immutable: true }));
  app.use(express.static(dist));
}

// Sous Windows, deux processus peuvent se lier au même port sans erreur : le second
// s'arrête alors silencieusement (code 0). On vérifie donc explicitement avant de démarrer.
try {
  await fetch(`http://localhost:${config.port}/`, { signal: AbortSignal.timeout(1500) });
  console.error(`[api] Le port ${config.port} est déjà utilisé — une autre instance de l'application tourne probablement.`);
  console.error(`[api] Fermez-la (ou utilisez-la directement : http://localhost:${config.port}) puis relancez.`);
  process.exit(1);
} catch {
  // rien ne répond : le port est libre, on peut démarrer
}

const server = app.listen(config.port, () => {
  console.log(`[api] Serveur démarré sur http://localhost:${config.port}`);
  console.log(`[api] Dossier de sortie : ${config.outputDir}`);
  checkTools(); // affiche les versions yt-dlp / ffmpeg dans la console de l'interface
  tick(); // reprend la file au démarrage si des éléments sont en attente
});

server.on('error', (err) => {
  console.error(`[api] Impossible de démarrer sur le port ${config.port} : ${err.message}`);
  process.exit(1);
});
