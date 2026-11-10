// Téléchargeur HLS natif : parseur de playlists, téléchargement parallèle avec reprise,
// déchiffrement AES-128, nettoyage des segments « empoisonnés », puis assemblage par ffmpeg
// (copie de flux, aucun ré-encodage). Un segment perdu fait échouer le téléchargement.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::model::HttpCtx;

pub mod assemble;
pub mod fetch;
pub mod file;
pub mod playlist;

pub use assemble::assemble;
pub use fetch::{download, refetch_segments};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
}

impl TrackKind {
    /// Nom du sous-dossier et de la playlist locale (`video` / `audio`).
    pub fn dir_name(self) -> &'static str {
        match self {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
        }
    }

    /// Libellé pour les messages d'erreur.
    pub fn label(self) -> &'static str {
        match self {
            TrackKind::Video => "vidéo",
            TrackKind::Audio => "audio",
        }
    }
}

pub struct HlsRequest {
    pub url: String,
    pub http: HttpCtx,
    pub work_dir: PathBuf,
    pub parallel: usize,
    pub segment_retries: u32,
}

#[derive(Clone, Debug)]
pub struct SegmentInfo {
    pub index: u32,                 // position dans la playlist (0..n)
    pub sequence: u64,              // media sequence
    pub url: String,
    pub path: PathBuf,              // fichier local, déjà déchiffré et nettoyé
    pub duration: f64,              // EXTINF
    pub discontinuity_before: bool, // #EXT-X-DISCONTINUITY juste avant
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct TrackInfo {
    pub kind: TrackKind,
    pub playlist_url: String,
    pub local_playlist: PathBuf,       // playlist locale réécrite (chemins locaux, sans #EXT-X-KEY)
    pub init_segment: Option<PathBuf>, // #EXT-X-MAP (fMP4), sinon None (MPEG-TS)
    pub segments: Vec<SegmentInfo>,
    pub expected_duration: f64,        // somme des EXTINF
}

#[derive(Clone, Debug)]
pub struct HlsDownload {
    pub work_dir: PathBuf,
    pub video: TrackInfo,
    pub audio: Option<TrackInfo>, // piste audio séparée (#EXT-X-MEDIA) si la variante en référence une
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bandwidth: Option<u64>,
}

/// Mini serveur HTTP local pour les tests d'intégration (routes fixes, Range, compteurs).
#[cfg(test)]
pub(crate) mod testsrv {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    pub struct Route {
        pub status: u16,
        pub content_type: &'static str,
        pub body: Vec<u8>,
    }

    impl Route {
        pub fn ok(body: impl Into<Vec<u8>>) -> Self {
            Route { status: 200, content_type: "application/octet-stream", body: body.into() }
        }
    }

    pub fn tempdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "ursa-hls-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    pub struct Server {
        pub base: String,
        pub hits: Arc<Mutex<HashMap<String, u32>>>,
        pub routes: Arc<Mutex<HashMap<String, Route>>>,
    }

    impl Server {
        pub fn hits(&self, path: &str) -> u32 {
            self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
        }
        pub fn set(&self, path: &str, route: Route) {
            self.routes.lock().unwrap().insert(path.to_string(), route);
        }
    }

    pub async fn start(initial: Vec<(&str, Route)>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::new(Mutex::new(
            initial.into_iter().map(|(p, r)| (p.to_string(), r)).collect(),
        ));
        let hits: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));
        let (r2, h2) = (routes.clone(), hits.clone());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let (routes, hits) = (r2.clone(), h2.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let mut lines = head.lines();
                    let first = lines.next().unwrap_or("");
                    let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let range = lines
                        .filter_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("range").then(|| v.trim().to_string())
                        })
                        .next();
                    *hits.lock().unwrap().entry(path.clone()).or_insert(0) += 1;
                    let route = routes.lock().unwrap().get(&path).cloned();
                    let resp: Vec<u8> = match route {
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                        Some(r) => {
                            let total = r.body.len();
                            let mut status = r.status;
                            let mut body: &[u8] = &r.body;
                            let mut extra = String::new();
                            if status == 200 {
                                if let Some(spec) = range.as_deref().and_then(|s| s.strip_prefix("bytes=")) {
                                    let mut it = spec.splitn(2, '-');
                                    let start: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                                    let end: usize = it
                                        .next()
                                        .and_then(|v| v.parse::<usize>().ok())
                                        .map(|e| e.min(total.saturating_sub(1)))
                                        .unwrap_or(total.saturating_sub(1));
                                    if start >= total {
                                        status = 416;
                                        body = &[];
                                        extra = format!("Content-Range: bytes */{total}\r\n");
                                    } else {
                                        status = 206;
                                        body = &r.body[start..=end];
                                        extra = format!("Content-Range: bytes {start}-{end}/{total}\r\n");
                                    }
                                }
                            }
                            let reason = match status {
                                200 => "OK",
                                206 => "Partial Content",
                                416 => "Range Not Satisfiable",
                                403 => "Forbidden",
                                _ => "Status",
                            };
                            let mut out = format!(
                                "HTTP/1.1 {status} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                                r.content_type,
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(body);
                            out
                        }
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        Server { base: format!("http://127.0.0.1:{port}"), hits, routes }
    }
}
