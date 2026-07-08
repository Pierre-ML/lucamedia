import express from 'express';
import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, basename } from 'node:path';
import { config } from './config.js';
import { queries } from './db.js';
import { addClient, broadcast } from './sse.js';
import { tick, queueSnapshot, broadcastQueue, getLogs } from './worker.js';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const app = express();
app.use(express.json());

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

app.listen(config.port, () => {
  console.log(`[api] Serveur démarré sur http://localhost:${config.port}`);
  console.log(`[api] Dossier de sortie : ${config.outputDir}`);
  tick(); // reprend la file au démarrage si des éléments sont en attente
});
