import Database from 'better-sqlite3';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const db = new Database(join(root, 'queue.db'));

db.pragma('journal_mode = WAL');
// Avec WAL, NORMAL suffit (pas de corruption possible, seule la toute dernière
// écriture peut être perdue en cas de coupure) et réduit nettement les fsync
db.pragma('synchronous = NORMAL');

db.exec(`
  CREATE TABLE IF NOT EXISTS downloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL,
    filename TEXT NOT NULL,
    is_uqload INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending',
    progress_percent REAL NOT NULL DEFAULT 0,
    eta_seconds INTEGER,
    size_estimate_mb REAL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now', 'localtime'))
  );

  CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
  );
`);

// Un téléchargement interrompu par un arrêt brutal du serveur repart en attente
db.prepare(
  `UPDATE downloads SET status = 'pending', updated_at = datetime('now', 'localtime') WHERE status = 'downloading'`
).run();

const stmts = {
  insert: db.prepare(
    `INSERT INTO downloads (url, filename, is_uqload) VALUES (@url, @filename, @is_uqload)`
  ),
  byId: db.prepare(`SELECT * FROM downloads WHERE id = ?`),
  all: db.prepare(`SELECT * FROM downloads ORDER BY created_at ASC, id ASC`),
  nextPending: db.prepare(
    `SELECT * FROM downloads WHERE status = 'pending' ORDER BY created_at ASC, id ASC LIMIT 1`
  ),
  setStatus: db.prepare(
    `UPDATE downloads SET status = @status, updated_at = datetime('now', 'localtime') WHERE id = @id`
  ),
  setProgress: db.prepare(
    `UPDATE downloads
     SET progress_percent = @progress_percent,
         eta_seconds = @eta_seconds,
         size_estimate_mb = @size_estimate_mb,
         updated_at = datetime('now', 'localtime')
     WHERE id = @id`
  ),
  markError: db.prepare(
    `UPDATE downloads
     SET status = 'error', error_message = @error_message, updated_at = datetime('now', 'localtime')
     WHERE id = @id`
  ),
  incrementRetry: db.prepare(
    `UPDATE downloads
     SET retry_count = retry_count + 1, error_message = @error_message, updated_at = datetime('now', 'localtime')
     WHERE id = @id`
  ),
  resetForRetry: db.prepare(
    `UPDATE downloads
     SET status = 'pending', retry_count = 0, error_message = NULL, progress_percent = 0,
         eta_seconds = NULL, updated_at = datetime('now', 'localtime')
     WHERE id = @id`
  ),
  delete: db.prepare(`DELETE FROM downloads WHERE id = ?`),
  getSetting: db.prepare(`SELECT value FROM settings WHERE key = ?`),
  setSetting: db.prepare(
    `INSERT INTO settings (key, value) VALUES (@key, @value)
     ON CONFLICT(key) DO UPDATE SET value = @value`
  ),
};

export const queries = {
  addDownload: ({ url, filename, is_uqload }) =>
    stmts.insert.run({ url, filename, is_uqload: is_uqload ? 1 : 0 }),
  getDownload: (id) => stmts.byId.get(id),
  getAllDownloads: () => stmts.all.all(),
  getNextPending: () => stmts.nextPending.get(),
  setStatus: (id, status) => stmts.setStatus.run({ id, status }),
  setProgress: (id, { progress_percent, eta_seconds, size_estimate_mb }) =>
    stmts.setProgress.run({ id, progress_percent, eta_seconds, size_estimate_mb }),
  markError: (id, message) => stmts.markError.run({ id, error_message: message }),
  incrementRetry: (id, message) => stmts.incrementRetry.run({ id, error_message: message }),
  resetForRetry: (id) => stmts.resetForRetry.run({ id }),
  deleteDownload: (id) => stmts.delete.run(id),

  isPaused: () => stmts.getSetting.get('paused')?.value === '1',
  setPaused: (paused) => stmts.setSetting.run({ key: 'paused', value: paused ? '1' : '0' }),
};

export default db;
