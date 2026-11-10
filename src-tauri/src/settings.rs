//! Réglages utilisateur, stockés (JSON) dans la table `settings` de SQLite.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::detect::BrowserChoice;

const SETTINGS_KEY: &str = "settings";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Dossier de destination (défaut : `<Vidéos>/Ursa`).
    pub dest_dir: String,
    /// Segments téléchargés en parallèle, 1..32.
    pub parallel_segments: u32,
    /// Tentatives maximum par téléchargement, 1..10.
    pub max_retries: u32,
    pub browser: BrowserChoice,
    pub show_browser: bool,
    pub verify_after_download: bool,
    pub auto_repair: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dest_dir: default_dest_dir().to_string_lossy().into_owned(),
            parallel_segments: 8,
            max_retries: 5,
            browser: BrowserChoice::Auto,
            show_browser: false,
            verify_after_download: true,
            auto_repair: true,
        }
    }
}

/// `<Vidéos>/Ursa`.
pub fn default_dest_dir() -> PathBuf {
    dirs::video_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join("Videos")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Ursa")
}

impl Settings {
    /// Borne les valeurs dans les plages autorisées.
    pub fn normalized(mut self) -> Self {
        self.dest_dir = self.dest_dir.trim().to_string();
        if self.dest_dir.is_empty() {
            self.dest_dir = default_dest_dir().to_string_lossy().into_owned();
        }
        self.parallel_segments = self.parallel_segments.clamp(1, 32);
        self.max_retries = self.max_retries.clamp(1, 10);
        self
    }

    /// Lit les réglages de la base (défauts si absents ou illisibles).
    pub fn load(db: &Db) -> Self {
        db.get_setting(SETTINGS_KEY)
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Settings>(&raw).ok())
            .unwrap_or_default()
            .normalized()
    }

    pub fn save(&self, db: &Db) -> anyhow::Result<()> {
        db.set_setting(SETTINGS_KEY, &serde_json::to_string(self)?)
    }

    /// Crée le dossier de destination s'il est absent.
    pub fn ensure_dest_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dest_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_contract() {
        let s = Settings::default();
        assert_eq!(s.parallel_segments, 8);
        assert_eq!(s.max_retries, 5);
        assert_eq!(s.browser, BrowserChoice::Auto);
        assert!(!s.show_browser && s.verify_after_download && s.auto_repair);
        assert!(PathBuf::from(&s.dest_dir).ends_with("Ursa"));
    }

    #[test]
    fn values_are_clamped() {
        let s = Settings {
            parallel_segments: 500,
            max_retries: 0,
            dest_dir: "  ".into(),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(s.parallel_segments, 32);
        assert_eq!(s.max_retries, 1);
        assert!(s.dest_dir.ends_with("Ursa"));
        let s = Settings {
            parallel_segments: 0,
            max_retries: 99,
            ..Settings::default()
        }
        .normalized();
        assert_eq!((s.parallel_segments, s.max_retries), (1, 10));
    }

    #[test]
    fn json_is_camel_case_with_defaults() {
        let s: Settings =
            serde_json::from_str(r#"{"destDir":"C:\\v","browser":"edge","parallelSegments":3}"#).unwrap();
        assert_eq!(s.dest_dir, "C:\\v");
        assert_eq!(s.browser, BrowserChoice::Edge);
        assert_eq!(s.parallel_segments, 3);
        assert_eq!(s.max_retries, 5);
        let out = serde_json::to_string(&Settings::default()).unwrap();
        assert!(out.contains(r#""parallelSegments":8"#));
        assert!(out.contains(r#""verifyAfterDownload":true"#));
        assert!(out.contains(r#""browser":"auto""#));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(Settings::load(&db), Settings::default().normalized());
        let s = Settings {
            dest_dir: "D:\\Films".into(),
            max_retries: 7,
            show_browser: true,
            ..Settings::default()
        };
        s.save(&db).unwrap();
        assert_eq!(Settings::load(&db), s);
    }

    #[test]
    fn ensure_dest_dir_creates_it() {
        let dir = tempfile::tempdir().unwrap();
        let s = Settings {
            dest_dir: dir.path().join("a").join("Ursa").to_string_lossy().into_owned(),
            ..Settings::default()
        };
        s.ensure_dest_dir().unwrap();
        assert!(dir.path().join("a").join("Ursa").is_dir());
    }
}
