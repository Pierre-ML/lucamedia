//! Persistance SQLite : file de téléchargements et petits réglages clé/valeur.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::detect::{MediaKind, ResolvedVia};
use crate::model::{Phase, Progress};
use crate::verify::GapReport;

/// Millisecondes depuis l'epoch Unix.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Pending,
    Running,
    Done,
    Warning,
    Error,
    Cancelled,
}

impl DownloadStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "done" => Self::Done,
            "warning" => Self::Warning,
            "error" => Self::Error,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// Élément de la file, tel que vu par le frontend.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: i64,
    pub url: String,
    pub filename: String,
    /// Le nom a été donné par l'utilisateur (sinon le titre résolu peut le remplacer).
    #[serde(skip)]
    pub name_is_custom: bool,
    pub referer: Option<String>,
    pub status: DownloadStatus,
    pub phase: Option<Phase>,
    pub progress_percent: f32,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
    pub segments_done: Option<u32>,
    pub segments_total: Option<u32>,
    pub retry_count: u32,
    pub error: Option<String>,
    pub output_path: Option<String>,
    pub resolved_via: Option<ResolvedVia>,
    pub media_kind: Option<MediaKind>,
    pub quality: Option<String>,
    pub report: Option<GapReport>,
    pub created_at: i64,
    pub finished_at: Option<i64>,
}

/// Énumération serde (minuscules) → texte.
fn enum_str<T: Serialize>(v: &T) -> Option<String> {
    match serde_json::to_value(v).ok()? {
        serde_json::Value::String(s) => Some(s),
        _ => None,
    }
}

/// Texte → énumération serde.
fn parse_enum<T: DeserializeOwned>(s: Option<String>) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s?)).ok()
}

const COLS: &str = "id, url, filename, name_is_custom, referer, status, phase, progress_percent, \
downloaded_bytes, total_bytes, speed_bps, eta_seconds, segments_done, segments_total, retry_count, \
error, output_path, resolved_via, media_kind, quality, report, created_at, finished_at";

fn row_to_download(r: &Row<'_>) -> rusqlite::Result<Download> {
    let opt_u64 = |i: usize| -> rusqlite::Result<Option<u64>> {
        Ok(r.get::<_, Option<i64>>(i)?.map(|v| v.max(0) as u64))
    };
    let opt_u32 = |i: usize| -> rusqlite::Result<Option<u32>> {
        Ok(r.get::<_, Option<i64>>(i)?.map(|v| v.max(0) as u32))
    };
    let status: String = r.get(5)?;
    let report: Option<String> = r.get(20)?;
    Ok(Download {
        id: r.get(0)?,
        url: r.get(1)?,
        filename: r.get(2)?,
        name_is_custom: r.get::<_, i64>(3)? != 0,
        referer: r.get(4)?,
        status: DownloadStatus::parse(&status).unwrap_or(DownloadStatus::Error),
        phase: parse_enum(r.get(6)?),
        progress_percent: r.get::<_, f64>(7)? as f32,
        downloaded_bytes: r.get::<_, i64>(8)?.max(0) as u64,
        total_bytes: opt_u64(9)?,
        speed_bps: opt_u64(10)?,
        eta_seconds: opt_u64(11)?,
        segments_done: opt_u32(12)?,
        segments_total: opt_u32(13)?,
        retry_count: r.get::<_, i64>(14)?.max(0) as u32,
        error: r.get(15)?,
        output_path: r.get(16)?,
        resolved_via: parse_enum(r.get(17)?),
        media_kind: parse_enum(r.get(18)?),
        quality: r.get(19)?,
        report: report.and_then(|s| serde_json::from_str(&s).ok()),
        created_at: r.get(21)?,
        finished_at: r.get(22)?,
    })
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    /// Ouvre (ou crée) la base `<app_data_dir>/ursa.db`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("création du dossier {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("ouverture de la base {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS downloads (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 url TEXT NOT NULL,
                 filename TEXT NOT NULL,
                 name_is_custom INTEGER NOT NULL DEFAULT 0,
                 referer TEXT,
                 status TEXT NOT NULL DEFAULT 'pending',
                 phase TEXT,
                 progress_percent REAL NOT NULL DEFAULT 0,
                 downloaded_bytes INTEGER NOT NULL DEFAULT 0,
                 total_bytes INTEGER,
                 speed_bps INTEGER,
                 eta_seconds INTEGER,
                 segments_done INTEGER,
                 segments_total INTEGER,
                 retry_count INTEGER NOT NULL DEFAULT 0,
                 error TEXT,
                 output_path TEXT,
                 resolved_via TEXT,
                 media_kind TEXT,
                 quality TEXT,
                 report TEXT,
                 created_at INTEGER NOT NULL,
                 finished_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )
        .context("création du schéma SQLite")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── Téléchargements ────────────────────────────────────────────────────

    pub fn add(
        &self,
        url: &str,
        filename: &str,
        name_is_custom: bool,
        referer: Option<&str>,
    ) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO downloads (url, filename, name_is_custom, referer, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![url, filename, name_is_custom as i64, referer, now_ms()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> anyhow::Result<Option<Download>> {
        let sql = format!("SELECT {COLS} FROM downloads WHERE id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, params![id], row_to_download)
            .optional()?)
    }

    /// Tous les éléments, par ordre de file (id croissant).
    pub fn all(&self) -> anyhow::Result<Vec<Download>> {
        let sql = format!("SELECT {COLS} FROM downloads ORDER BY id ASC");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_download)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn next_pending(&self) -> anyhow::Result<Option<Download>> {
        let sql =
            format!("SELECT {COLS} FROM downloads WHERE status = 'pending' ORDER BY id ASC LIMIT 1");
        Ok(self.conn().query_row(&sql, [], row_to_download).optional()?)
    }

    /// Passe l'élément en cours d'exécution (progression remise à zéro, retryCount conservé).
    pub fn mark_running(&self, id: i64) -> anyhow::Result<()> {
        let n = self.conn().execute(
            "UPDATE downloads SET status = 'running', phase = 'resolving', progress_percent = 0,
                 downloaded_bytes = 0, total_bytes = NULL, speed_bps = NULL, eta_seconds = NULL,
                 segments_done = NULL, segments_total = NULL, error = NULL, finished_at = NULL
             WHERE id = ?1",
            params![id],
        )?;
        anyhow::ensure!(n > 0, "téléchargement {id} introuvable");
        Ok(())
    }

    /// Au démarrage : les `running` (interrompus) repassent en `pending`.
    pub fn reset_running(&self) -> anyhow::Result<usize> {
        Ok(self.conn().execute(
            "UPDATE downloads SET status = 'pending', phase = NULL, speed_bps = NULL, eta_seconds = NULL
             WHERE status = 'running'",
            [],
        )?)
    }

    /// Remet un élément en attente sans toucher à ses compteurs (interruption propre).
    pub fn requeue(&self, id: i64) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET status = 'pending', phase = NULL, speed_bps = NULL, eta_seconds = NULL
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn update_progress(&self, id: i64, p: &Progress) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET phase = ?2, progress_percent = ?3, downloaded_bytes = ?4,
                 total_bytes = ?5, speed_bps = ?6, eta_seconds = ?7, segments_done = ?8,
                 segments_total = ?9
             WHERE id = ?1 AND status = 'running'",
            params![
                id,
                p.phase.and_then(|ph| enum_str(&ph)),
                p.percent as f64,
                p.downloaded_bytes as i64,
                p.total_bytes.map(|v| v as i64),
                p.speed_bps.map(|v| v as i64),
                p.eta_seconds.map(|v| v as i64),
                p.segments_done.map(|v| v as i64),
                p.segments_total.map(|v| v as i64),
            ],
        )?;
        Ok(())
    }

    pub fn set_filename(&self, id: i64, filename: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET filename = ?2 WHERE id = ?1",
            params![id, filename],
        )?;
        Ok(())
    }

    pub fn set_media_info(
        &self,
        id: i64,
        via: Option<ResolvedVia>,
        kind: Option<MediaKind>,
        quality: Option<&str>,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET resolved_via = ?2, media_kind = ?3, quality = COALESCE(?4, quality)
             WHERE id = ?1",
            params![
                id,
                via.and_then(|v| enum_str(&v)),
                kind.and_then(|k| enum_str(&k)),
                quality
            ],
        )?;
        Ok(())
    }

    pub fn set_quality(&self, id: i64, quality: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET quality = ?2 WHERE id = ?1",
            params![id, quality],
        )?;
        Ok(())
    }

    /// Échec d'une tentative qui sera relancée.
    pub fn set_retry(&self, id: i64, retry_count: u32, error: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET retry_count = ?2, error = ?3 WHERE id = ?1",
            params![id, retry_count as i64, error],
        )?;
        Ok(())
    }

    /// Statut final (done / warning / error / cancelled).
    pub fn finish(
        &self,
        id: i64,
        status: DownloadStatus,
        error: Option<&str>,
        output_path: Option<&str>,
        report: Option<&GapReport>,
    ) -> anyhow::Result<()> {
        let report = report.map(serde_json::to_string).transpose()?;
        self.conn().execute(
            "UPDATE downloads SET status = ?2, error = ?3,
                 output_path = COALESCE(?4, output_path), report = COALESCE(?5, report),
                 phase = NULL, speed_bps = NULL, eta_seconds = NULL,
                 progress_percent = CASE WHEN ?2 = 'done' THEN 100 ELSE progress_percent END,
                 retry_count = retry_count, finished_at = ?6
             WHERE id = ?1",
            params![id, status.as_str(), error, output_path, report, now_ms()],
        )?;
        Ok(())
    }

    /// Nouveau rapport (re-vérification d'un fichier existant) et statut associé.
    pub fn set_report(
        &self,
        id: i64,
        status: DownloadStatus,
        report: &GapReport,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET status = ?2, report = ?3, error = NULL WHERE id = ?1",
            params![id, status.as_str(), serde_json::to_string(report)?],
        )?;
        Ok(())
    }

    /// Relance manuelle : retour en `pending`, compteur de tentatives remis à zéro.
    pub fn reset_for_retry(&self, id: i64) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE downloads SET status = 'pending', retry_count = 0, error = NULL, phase = NULL,
                 progress_percent = 0, downloaded_bytes = 0, total_bytes = NULL, speed_bps = NULL,
                 eta_seconds = NULL, segments_done = NULL, segments_total = NULL, report = NULL,
                 finished_at = NULL
             WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> anyhow::Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM downloads WHERE id = ?1", params![id])?
            > 0)
    }

    /// Retire done / warning / cancelled.
    pub fn clear_finished(&self) -> anyhow::Result<usize> {
        Ok(self.conn().execute(
            "DELETE FROM downloads WHERE status IN ('done', 'warning', 'cancelled')",
            [],
        )?)
    }

    // ── Réglages clé/valeur ────────────────────────────────────────────────

    pub fn get_setting(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.get_setting("paused"), Ok(Some(v)) if v == "1")
    }

    pub fn set_paused(&self, paused: bool) -> anyhow::Result<()> {
        self.set_setting("paused", if paused { "1" } else { "0" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::{BadSegment, Gap};
    use crate::hls::TrackKind;

    fn report(ok: bool) -> GapReport {
        GapReport {
            ok,
            expected_duration: Some(60.0),
            actual_duration: 57.5,
            gaps: vec![Gap {
                stream: TrackKind::Video,
                at: 12.0,
                missing: 2.5,
                segment_index: Some(3),
            }],
            bad_segments: vec![BadSegment {
                track: TrackKind::Audio,
                index: 7,
                reason: "illisible".into(),
            }],
            av_drift: Some(0.1),
            summary: "2,5 s manquantes".into(),
        }
    }

    #[test]
    fn add_and_list_in_order() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add("https://a.example/a.m3u8", "a", false, None).unwrap();
        let b = db
            .add("https://b.example/b.m3u8", "b", true, Some("https://ref.example/"))
            .unwrap();
        assert!(b > a);
        let all = db.all().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, a);
        assert_eq!(all[0].status, DownloadStatus::Pending);
        assert!(!all[0].name_is_custom);
        assert!(all[1].name_is_custom);
        assert_eq!(all[1].referer.as_deref(), Some("https://ref.example/"));
        assert_eq!(db.next_pending().unwrap().unwrap().id, a);
        assert_eq!(db.get(b).unwrap().unwrap().filename, "b");
        assert!(db.get(999).unwrap().is_none());
    }

    #[test]
    fn running_items_return_to_pending() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add("https://a.example/a", "a", false, None).unwrap();
        let b = db.add("https://b.example/b", "b", false, None).unwrap();
        db.mark_running(a).unwrap();
        db.finish(b, DownloadStatus::Done, None, Some("C:/b.mp4"), None)
            .unwrap();
        assert_eq!(db.get(a).unwrap().unwrap().status, DownloadStatus::Running);
        assert_eq!(db.reset_running().unwrap(), 1);
        assert_eq!(db.get(a).unwrap().unwrap().status, DownloadStatus::Pending);
        assert_eq!(db.get(b).unwrap().unwrap().status, DownloadStatus::Done);
        assert_eq!(db.next_pending().unwrap().unwrap().id, a);
    }

    #[test]
    fn paused_state_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("ursa.db");
        {
            let db = Db::open(&path).unwrap();
            assert!(!db.is_paused());
            db.set_paused(true).unwrap();
            db.add("https://a.example/a", "a", false, None).unwrap();
        }
        let db = Db::open(&path).unwrap();
        assert!(db.is_paused());
        assert_eq!(db.all().unwrap().len(), 1);
        db.set_paused(false).unwrap();
        assert!(!db.is_paused());
    }

    #[test]
    fn progress_and_finish_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let id = db.add("https://a.example/a", "a", false, None).unwrap();
        db.mark_running(id).unwrap();
        db.set_media_info(id, Some(ResolvedVia::Direct), Some(MediaKind::Hls), Some("1920x1080"))
            .unwrap();
        db.update_progress(
            id,
            &Progress {
                phase: Some(Phase::Downloading),
                percent: 42.5,
                downloaded_bytes: 1000,
                total_bytes: Some(5000),
                speed_bps: Some(250),
                eta_seconds: Some(16),
                segments_done: Some(4),
                segments_total: Some(10),
            },
        )
        .unwrap();
        let d = db.get(id).unwrap().unwrap();
        assert_eq!(d.phase, Some(Phase::Downloading));
        assert_eq!(d.progress_percent, 42.5);
        assert_eq!(d.total_bytes, Some(5000));
        assert_eq!(d.segments_total, Some(10));
        assert_eq!(d.resolved_via, Some(ResolvedVia::Direct));
        assert_eq!(d.media_kind, Some(MediaKind::Hls));
        assert_eq!(d.quality.as_deref(), Some("1920x1080"));

        db.finish(id, DownloadStatus::Warning, None, Some("C:/a.mp4"), Some(&report(false)))
            .unwrap();
        let d = db.get(id).unwrap().unwrap();
        assert_eq!(d.status, DownloadStatus::Warning);
        assert_eq!(d.phase, None);
        assert_eq!(d.output_path.as_deref(), Some("C:/a.mp4"));
        assert!(d.finished_at.is_some());
        let r = d.report.unwrap();
        assert!(!r.ok);
        assert_eq!(r.gaps[0].segment_index, Some(3));
        assert_eq!(r.bad_segments[0].index, 7);

        // Le JSON exposé au front est en camelCase, enums en minuscules.
        let json = serde_json::to_value(db.get(id).unwrap().unwrap()).unwrap();
        assert_eq!(json["status"], "warning");
        assert_eq!(json["mediaKind"], "hls");
        assert_eq!(json["report"]["badSegments"][0]["track"], "audio");
        assert!(json.get("nameIsCustom").is_none());
    }

    #[test]
    fn retry_delete_and_clear() {
        let db = Db::open_in_memory().unwrap();
        let a = db.add("https://a.example/a", "a", false, None).unwrap();
        let b = db.add("https://b.example/b", "b", false, None).unwrap();
        let c = db.add("https://c.example/c", "c", false, None).unwrap();
        db.set_retry(a, 3, "boom").unwrap();
        db.finish(a, DownloadStatus::Error, Some("boom"), None, None).unwrap();
        db.finish(b, DownloadStatus::Done, None, None, None).unwrap();
        db.finish(c, DownloadStatus::Cancelled, None, None, None).unwrap();
        assert_eq!(db.get(a).unwrap().unwrap().retry_count, 3);

        db.reset_for_retry(a).unwrap();
        let d = db.get(a).unwrap().unwrap();
        assert_eq!((d.status, d.retry_count, d.error), (DownloadStatus::Pending, 0, None));

        db.finish(a, DownloadStatus::Error, Some("x"), None, None).unwrap();
        assert_eq!(db.clear_finished().unwrap(), 2); // done + cancelled, l'erreur reste
        assert_eq!(db.all().unwrap().len(), 1);
        assert!(db.delete(a).unwrap());
        assert!(!db.delete(a).unwrap());
    }

    #[test]
    fn settings_kv_upsert() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_setting("k").unwrap(), None);
        db.set_setting("k", "1").unwrap();
        db.set_setting("k", "2").unwrap();
        assert_eq!(db.get_setting("k").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn status_strings_roundtrip() {
        for s in [
            DownloadStatus::Pending,
            DownloadStatus::Running,
            DownloadStatus::Done,
            DownloadStatus::Warning,
            DownloadStatus::Error,
            DownloadStatus::Cancelled,
        ] {
            assert_eq!(DownloadStatus::parse(s.as_str()), Some(s));
            assert_eq!(enum_str(&s).as_deref(), Some(s.as_str()));
        }
        assert_eq!(DownloadStatus::parse("nope"), None);
    }
}
