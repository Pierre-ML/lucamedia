//! Étape 3 : navigateur (Chrome, sinon Edge) piloté par le protocole DevTools (CDP).
//!
//! On ouvre la page dans un navigateur au profil temporaire, on essaie de lancer la lecture
//! et on écoute le réseau pour repérer le flux (m3u8, mpd, fichier vidéo).
//! Aucun contournement de DRM : si le flux est protégé, on le dit et on s'arrête.

use super::{BrowserChoice, MediaKind, ResolveHint, ResolvedMedia, ResolvedVia, MSG_DRM};
use crate::model::{HttpCtx, JobCtx, BROWSER_UA};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

/// Durée totale maximale d'observation de la page.
const TOTAL_WAIT: Duration = Duration::from_secs(30);
/// Délai d'arrêt après le premier candidat m3u8 (laisse arriver la playlist maître).
const AFTER_HLS: Duration = Duration::from_secs(3);
/// Délai d'arrêt après le premier candidat mpd / fichier vidéo.
const AFTER_OTHER: Duration = Duration::from_secs(6);
/// Intervalle entre deux tentatives de lecture.
const PLAY_EVERY: Duration = Duration::from_secs(2);
/// Taille minimale d'un fichier vidéo pour être retenu (les petits sont des extraits ou des segments).
const MIN_FILE_BYTES: u64 = 1_000_000;

// ---------------------------------------------------------------------------
// Localisation du navigateur
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserInfo {
    /// « Chrome » ou « Edge ».
    pub name: &'static str,
    pub kind: BrowserChoice,
    pub path: PathBuf,
}

fn env_path(var: &str, rest: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var).filter(|v| !v.is_empty())?;
    Some(PathBuf::from(base).join(rest))
}

/// Emplacements habituels de Chrome, par ordre de préférence.
pub fn chrome_candidates() -> Vec<PathBuf> {
    let rest = r"Google\Chrome\Application\chrome.exe";
    ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
        .iter()
        .filter_map(|v| env_path(v, rest))
        .collect()
}

/// Emplacements habituels d'Edge, par ordre de préférence.
pub fn edge_candidates() -> Vec<PathBuf> {
    let rest = r"Microsoft\Edge\Application\msedge.exe";
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(|v| env_path(v, rest))
        .collect()
}

/// Choix du navigateur parmi des chemins candidats donnés (testable).
pub fn locate_in(choice: BrowserChoice, chrome: &[PathBuf], edge: &[PathBuf]) -> Option<BrowserInfo> {
    let find = |list: &[PathBuf]| list.iter().find(|p| p.is_file()).cloned();
    let chrome_info = || {
        find(chrome).map(|path| BrowserInfo {
            name: "Chrome",
            kind: BrowserChoice::Chrome,
            path,
        })
    };
    let edge_info = || {
        find(edge).map(|path| BrowserInfo {
            name: "Edge",
            kind: BrowserChoice::Edge,
            path,
        })
    };
    match choice {
        BrowserChoice::Chrome => chrome_info(),
        BrowserChoice::Edge => edge_info(),
        BrowserChoice::Auto => chrome_info().or_else(edge_info),
    }
}

/// Navigateur installé correspondant au choix (Auto : Chrome, sinon Edge).
pub fn locate(choice: BrowserChoice) -> Option<BrowserInfo> {
    locate_in(choice, &chrome_candidates(), &edge_candidates())
}

// ---------------------------------------------------------------------------
// Analyse des URL, des réponses et des candidats (fonctions pures)
// ---------------------------------------------------------------------------

/// En-tête HTTP, insensible à la casse, dans un objet JSON `{nom: valeur}`.
fn header_ci<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_str())
        .filter(|s| !s.is_empty())
}

fn path_of(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase(),
    }
}

/// Extensions de segments (jamais retenues comme fichier vidéo).
fn is_segment_path(path: &str) -> bool {
    [".ts", ".m4s", ".m4f", ".cmfv", ".cmfa", ".aac", ".m4a", ".mp3"]
        .iter()
        .any(|e| path.ends_with(e))
}

/// Domaine ou chemin publicitaire évident (`ads`, `doubleclick`, `vast`, `preroll`…).
///
/// L'URL est découpée en mots : « uploads » ou « downloads » ne sont pas des publicités.
pub fn is_ad_url(url: &str) -> bool {
    let target = match reqwest::Url::parse(url) {
        Ok(u) => format!("{} {}", u.host_str().unwrap_or(""), u.path()),
        Err(_) => url.split(['?', '#']).next().unwrap_or("").to_string(),
    }
    .to_ascii_lowercase();
    if target.contains("doubleclick") || target.contains("googlesyndication") {
        return true;
    }
    const AD_WORDS: [&str; 14] = [
        "ads", "ad", "adserver", "adservice", "adservices", "adsystem", "vast", "vmap", "preroll",
        "midroll", "postroll", "imasdk", "advert", "adverts",
    ];
    target
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .any(|w| {
            AD_WORDS.contains(&w)
                || w.strip_prefix("ads")
                    .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        })
}

/// Requête de licence DRM (Widevine, PlayReady…) : indice de contenu protégé.
fn is_license_url(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    ["widevine", "playready", "/license", "licenseserver", "drmtoday", "/drm/", "fairplay"]
        .iter()
        .any(|k| u.contains(k))
}

/// Manifeste annonçant une protection DRM (Widevine, PlayReady, FairPlay).
pub fn manifest_is_drm(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed", // Widevine
        "9a04f079-9840-4286-ab92-e65be0885f95", // PlayReady
        "94ce86fb-07ff-4f43-adb8-93d2fa968ca2", // FairPlay (DASH)
        "com.apple.streamingkeydelivery",       // FairPlay (HLS)
        "com.microsoft.playready",
        "com.widevine",
    ]
    .iter()
    .any(|k| t.contains(k))
}

/// Type de flux deviné d'après l'URL seule : playlists m3u8 / mpd.
fn manifest_kind_of_url(url: &str) -> Option<MediaKind> {
    let p = path_of(url);
    if p.contains(".m3u8") {
        Some(MediaKind::Hls)
    } else if p.contains(".mpd") {
        Some(MediaKind::Dash)
    } else {
        None
    }
}

/// Taille totale annoncée par une réponse (`Content-Range` en priorité, sinon `Content-Length`).
fn response_size(headers: &Value) -> Option<u64> {
    if let Some(total) = header_ci(headers, "Content-Range")
        .and_then(|r| r.rsplit('/').next())
        .and_then(|t| t.trim().parse::<u64>().ok())
    {
        return Some(total);
    }
    header_ci(headers, "Content-Length").and_then(|l| l.trim().parse::<u64>().ok())
}

/// Un flux est-il annoncé par cette réponse ? (Content-Type, URL, taille)
///
/// Les segments (`.ts`, `.m4s`…) et les fichiers de moins de 1 Mo sont ignorés.
pub fn classify_response(url: &str, mime: &str, size: Option<u64>) -> Option<MediaKind> {
    let mime = mime.to_ascii_lowercase();
    if mime.contains("mpegurl") {
        return Some(MediaKind::Hls);
    }
    if mime.contains("dash+xml") {
        return Some(MediaKind::Dash);
    }
    if let Some(k) = manifest_kind_of_url(url) {
        // Une playlist dont la réponse est un fichier vidéo n'en est pas une.
        if !mime.starts_with("video/") {
            return Some(k);
        }
    }
    let path = path_of(url);
    let is_video_ext = [".mp4", ".webm", ".mkv", ".mov", ".m4v"].iter().any(|e| path.ends_with(e));
    let is_video_mime = mime.starts_with("video/")
        && !mime.contains("mp2t")
        && !mime.contains("iso.segment")
        && !mime.contains("mpegurl");
    // Certains serveurs servent un .mp4 en application/octet-stream.
    let generic_mime = mime.is_empty() || mime == "application/octet-stream";
    if is_segment_path(&path) || !(is_video_mime || (is_video_ext && generic_mime)) {
        return None;
    }
    if size.is_some_and(|s| s < MIN_FILE_BYTES) {
        return None;
    }
    Some(MediaKind::File)
}

/// Flux repéré dans le réseau de la page.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub url: String,
    pub kind: MediaKind,
    pub size: Option<u64>,
    pub referer: Option<String>,
    pub origin: Option<String>,
    /// Texte de la playlist / du manifeste, quand on a pu le récupérer.
    pub body: Option<String>,
    /// Ordre d'apparition.
    pub seq: usize,
    /// (session, requestId) CDP de la requête.
    req: Option<(String, String)>,
}

impl Candidate {
    pub fn new(url: &str, kind: MediaKind, size: Option<u64>) -> Self {
        Candidate {
            url: url.to_string(),
            kind,
            size,
            referer: None,
            origin: None,
            body: None,
            seq: 0,
            req: None,
        }
    }

    /// `Some(true)` : playlist maître HLS ; `Some(false)` : playlist média ; `None` : inconnu.
    pub fn is_master(&self) -> Option<bool> {
        if self.kind != MediaKind::Hls {
            return None;
        }
        self.body.as_deref().map(|b| b.contains("#EXT-X-STREAM-INF"))
    }
}

/// Choix du meilleur candidat : playlist maître m3u8 > playlist média m3u8 > mpd > plus gros fichier.
/// Les flux publicitaires évidents sont ignorés.
pub fn pick_best(cands: &[Candidate]) -> Option<usize> {
    let rank = |c: &Candidate| -> u8 {
        match c.kind {
            MediaKind::Hls if c.is_master() == Some(true) => 4,
            MediaKind::Hls => 3,
            MediaKind::Dash => 2,
            _ => 1,
        }
    };
    cands
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_ad_url(&c.url) && matches!(c.kind, MediaKind::Hls | MediaKind::Dash | MediaKind::File))
        .max_by(|(_, a), (_, b)| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.size.unwrap_or(0).cmp(&b.size.unwrap_or(0)))
                // À égalité, le plus ancien l'emporte.
                .then_with(|| b.seq.cmp(&a.seq))
        })
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// Client CDP minimal (WebSocket + JSON)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Event {
    session_id: Option<String>,
    method: String,
    params: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: AtomicU64,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Cdp {
    async fn connect(ws_url: &str) -> anyhow::Result<(Cdp, mpsc::UnboundedReceiver<Event>)> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| anyhow::anyhow!("Connexion au navigateur impossible : {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<Event>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let writer = tokio::spawn(async move {
            while let Some(s) = out_rx.recv().await {
                if sink.send(Message::text(s)).await.is_err() {
                    break;
                }
            }
        });

        let pending_r = pending.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                if let Some(id) = v.get("id").and_then(Value::as_u64) {
                    let tx = pending_r.lock().ok().and_then(|mut p| p.remove(&id));
                    if let Some(tx) = tx {
                        let r = match v.get("error") {
                            Some(e) => Err(e
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("erreur CDP")
                                .to_string()),
                            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = tx.send(r);
                    }
                } else if let Some(method) = v.get("method").and_then(Value::as_str) {
                    let _ = ev_tx.send(Event {
                        session_id: v.get("sessionId").and_then(Value::as_str).map(str::to_string),
                        method: method.to_string(),
                        params: v.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
            }
            // Connexion fermée : les appels en attente échouent.
            if let Ok(mut p) = pending_r.lock() {
                p.clear();
            }
        });

        Ok((
            Cdp {
                out: out_tx,
                pending,
                next: AtomicU64::new(1),
                tasks: vec![writer, reader],
            },
            ev_rx,
        ))
    }

    async fn call_t(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        if self.out.send(msg.to_string()).is_err() {
            anyhow::bail!("Le navigateur ne répond plus");
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => anyhow::bail!("{method} : {e}"),
            Ok(Err(_)) => anyhow::bail!("{method} : connexion au navigateur fermée"),
            Err(_) => {
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&id);
                }
                anyhow::bail!("{method} : délai dépassé")
            }
        }
    }

    async fn call(&self, method: &str, params: Value, session: Option<&str>) -> anyhow::Result<Value> {
        self.call_t(method, params, session, Duration::from_secs(10)).await
    }
}

// ---------------------------------------------------------------------------
// Processus du navigateur
// ---------------------------------------------------------------------------

struct Browser {
    child: Child,
    profile: PathBuf,
    ws_url: String,
}

impl Drop for Browser {
    fn drop(&mut self) {
        // Filet de sécurité (le chemin normal passe par `shutdown`).
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn new_profile_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("ursa-browser-{}-{nanos}", std::process::id()))
}

/// Lit `DevToolsActivePort` : port et chemin WebSocket du navigateur.
fn read_devtools_port(profile: &Path) -> Option<String> {
    let text = std::fs::read_to_string(profile.join("DevToolsActivePort")).ok()?;
    let mut lines = text.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    if port == 0 || !path.starts_with('/') {
        return None;
    }
    Some(format!("ws://127.0.0.1:{port}{path}"))
}

impl Browser {
    async fn launch(info: &BrowserInfo, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<Browser> {
        let profile = new_profile_dir();
        std::fs::create_dir_all(&profile)
            .map_err(|e| anyhow::anyhow!("Impossible de créer le profil temporaire du navigateur : {e}"))?;

        let mut cmd = Command::new(&info.path);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .arg("--remote-debugging-port=0")
            .arg("--remote-allow-origins=*")
            .arg("--mute-audio")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--autoplay-policy=no-user-gesture-required")
            .arg(format!("--user-agent={BROWSER_UA}"))
            .arg("--window-size=1280,720")
            .arg("--disable-background-networking")
            .arg("--disable-component-update")
            .arg("--disable-sync")
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--password-store=basic");
        if !hint.show_browser {
            cmd.arg("--headless=new");
        }
        cmd.arg("about:blank")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

        ctx.info(format!("Lancement de {} ({})", info.name, info.path.display()));
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&profile);
                anyhow::bail!("Impossible de lancer {} : {e}", info.name);
            }
        };
        let mut br = Browser {
            child,
            profile,
            ws_url: String::new(),
        };

        let started = Instant::now();
        loop {
            ctx.check()?;
            if let Some(url) = read_devtools_port(&br.profile) {
                br.ws_url = url;
                return Ok(br);
            }
            if let Ok(Some(status)) = br.child.try_wait() {
                anyhow::bail!("{} s'est fermé au démarrage ({status})", info.name);
            }
            if started.elapsed() > Duration::from_secs(20) {
                anyhow::bail!("{} n'a pas démarré à temps", info.name);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Ferme le navigateur (et ses sous-processus) puis supprime le profil temporaire.
    async fn shutdown(mut self) {
        let exited = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
        if exited.is_err() {
            if let Some(pid) = self.child.id() {
                kill_tree(pid).await;
            }
            let _ = self.child.kill().await;
        }
        // Les fichiers du profil peuvent rester verrouillés un instant après l'arrêt.
        for _ in 0..15 {
            if std::fs::remove_dir_all(&self.profile).is_ok() || !self.profile.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
}

/// Tue un processus et ses enfants (Windows : `taskkill /T /F`).
async fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let mut c = Command::new("taskkill");
        c.args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        c.creation_flags(0x0800_0000);
        let _ = tokio::time::timeout(Duration::from_secs(5), c.status()).await;
    }
    #[cfg(not(windows))]
    {
        let _ = pid;
    }
}

// ---------------------------------------------------------------------------
// Scripts injectés dans les pages
// ---------------------------------------------------------------------------

/// Détecte les usages réels d'EME (événement `encrypted`, `setMediaKeys` avec des clés).
const DRM_HOOK_JS: &str = r#"(() => { try {
  const mark = () => { try { window.__ursaDrm = true; } catch (e) {} };
  document.addEventListener('encrypted', mark, true);
  const p = window.HTMLMediaElement && HTMLMediaElement.prototype;
  if (p && p.setMediaKeys) {
    const o = p.setMediaKeys;
    p.setMediaKeys = function (k) { if (k) mark(); return o.apply(this, arguments); };
  }
} catch (e) {} })();"#;

/// Lance la lecture dans le document (et ses iframes de même origine) et renvoie l'état.
const PLAY_JS: &str = r#"(() => {
  const out = { v: 0, p: 0, drm: !!window.__ursaDrm };
  const buttons = ['.vjs-big-play-button', '.jw-icon-display', '.plyr__control--overlaid',
    '.ytp-large-play-button', 'button[aria-label="Play"]', 'button[title="Play"]',
    '.vjs-play-control.vjs-paused', '.play-button'];
  const visit = (doc, depth) => {
    try {
      doc.querySelectorAll('video').forEach(v => {
        out.v++;
        if (!v.paused) out.p++;
        if (v.mediaKeys) out.drm = true;
        try { v.muted = true; const r = v.play(); if (r && r.catch) r.catch(() => {}); } catch (e) {}
      });
      if (out.p === 0) buttons.forEach(s => { try { doc.querySelectorAll(s).forEach(b => b.click()); } catch (e) {} });
      if (depth < 4) doc.querySelectorAll('iframe').forEach(f => {
        try { if (f.contentDocument) visit(f.contentDocument, depth + 1); } catch (e) {}
      });
      if (doc.defaultView && doc.defaultView.__ursaDrm) out.drm = true;
    } catch (e) {}
  };
  visit(document, 0);
  return out;
})()"#;

// ---------------------------------------------------------------------------
// Collecte du réseau
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Collector {
    cands: Vec<Candidate>,
    /// (session, requestId) -> (url, referer, origin)
    requests: HashMap<(String, String), (String, Option<String>, Option<String>)>,
    drm_evidence: bool,
    first_hls: Option<Instant>,
    first_other: Option<Instant>,
    seq: usize,
}

fn short(url: &str) -> String {
    if url.chars().count() > 160 {
        format!("{}…", url.chars().take(160).collect::<String>())
    } else {
        url.to_string()
    }
}

impl Collector {
    fn add(&mut self, mut c: Candidate, ctx: &JobCtx) {
        if is_ad_url(&c.url) {
            ctx.info(format!("Flux publicitaire ignoré : {}", short(&c.url)));
            return;
        }
        if let Some(existing) = self.cands.iter_mut().find(|e| e.url == c.url) {
            if existing.size.is_none() {
                existing.size = c.size;
            }
            if existing.referer.is_none() {
                existing.referer = c.referer;
            }
            if existing.origin.is_none() {
                existing.origin = c.origin;
            }
            if existing.req.is_none() {
                existing.req = c.req;
            }
            return;
        }
        c.seq = self.seq;
        self.seq += 1;
        ctx.info(format!("Flux candidat ({:?}) : {}", c.kind, short(&c.url)));
        let now = Instant::now();
        match c.kind {
            MediaKind::Hls => {
                self.first_hls.get_or_insert(now);
            }
            _ => {
                self.first_other.get_or_insert(now);
            }
        }
        self.cands.push(c);
    }

    /// Instant auquel l'observation doit s'arrêter (au plus tard `deadline`).
    fn stop_at(&self, deadline: Instant) -> Instant {
        let mut stop = deadline;
        if let Some(h) = self.first_hls {
            stop = stop.min(h + AFTER_HLS);
        }
        if let Some(o) = self.first_other {
            stop = stop.min(o + AFTER_OTHER);
        }
        stop
    }
}

struct Sessions {
    page: String,
    /// Sessions de la page, de ses iframes et workers rattachés (la page en premier).
    all: Vec<(String, String)>,
}

async fn setup_session(cdp: &Cdp, sid: &str, ttype: &str, waiting: bool) {
    let s = Some(sid);
    let _ = cdp.call_t("Network.enable", json!({}), s, Duration::from_secs(5)).await;
    if ttype == "page" || ttype == "iframe" {
        let _ = cdp.call_t("Page.enable", json!({}), s, Duration::from_secs(5)).await;
        let _ = cdp
            .call_t(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": DRM_HOOK_JS, "runImmediately": true}),
                s,
                Duration::from_secs(5),
            )
            .await;
        let _ = cdp
            .call_t(
                "Target.setAutoAttach",
                json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
                s,
                Duration::from_secs(5),
            )
            .await;
    }
    if waiting {
        let _ = cdp
            .call_t("Runtime.runIfWaitingForDebugger", json!({}), s, Duration::from_secs(5))
            .await;
    }
}

async fn handle_event(
    cdp: &Cdp,
    ev: Event,
    col: &mut Collector,
    sessions: &mut Sessions,
    ctx: &JobCtx,
) {
    let sid = ev.session_id.clone().unwrap_or_default();
    let p = &ev.params;
    match ev.method.as_str() {
        "Target.attachedToTarget" => {
            let (Some(new_sid), Some(ttype)) = (
                p.get("sessionId").and_then(Value::as_str),
                p.pointer("/targetInfo/type").and_then(Value::as_str),
            ) else {
                return;
            };
            let waiting = p.get("waitingForDebugger").and_then(Value::as_bool).unwrap_or(false);
            if matches!(ttype, "iframe" | "page") {
                sessions.all.push((new_sid.to_string(), ttype.to_string()));
            }
            setup_session(cdp, new_sid, ttype, waiting).await;
        }
        "Target.detachedFromTarget" => {
            if let Some(s) = p.get("sessionId").and_then(Value::as_str) {
                sessions.all.retain(|(id, _)| id != s || *id == sessions.page);
            }
        }
        "Target.targetCreated" => {
            // Fenêtres publicitaires ouvertes par la page : on les ferme.
            let info = &p["targetInfo"];
            if info.get("type").and_then(Value::as_str) == Some("page")
                && info.get("openerId").and_then(Value::as_str).is_some()
            {
                if let Some(id) = info.get("targetId").and_then(Value::as_str) {
                    let _ = cdp
                        .call_t("Target.closeTarget", json!({"targetId": id}), None, Duration::from_secs(3))
                        .await;
                }
            }
        }
        "Network.requestWillBeSent" => {
            let rid = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
            let req = &p["request"];
            let url = req.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let method = req.get("method").and_then(Value::as_str).unwrap_or("GET");
            let rtype = p.get("type").and_then(Value::as_str).unwrap_or("");
            if url.is_empty() || method.eq_ignore_ascii_case("OPTIONS") || rtype == "Preflight" {
                return;
            }
            let headers = &req["headers"];
            let referer = header_ci(headers, "Referer").map(str::to_string);
            let origin = header_ci(headers, "Origin").map(str::to_string);
            if method.eq_ignore_ascii_case("POST") && is_license_url(&url) {
                if !col.drm_evidence {
                    ctx.warn("Requête de licence DRM détectée.");
                }
                col.drm_evidence = true;
            }
            col.requests
                .insert((sid.clone(), rid.clone()), (url.clone(), referer.clone(), origin.clone()));
            if let Some(kind) = manifest_kind_of_url(&url) {
                let mut c = Candidate::new(&url, kind, None);
                c.referer = referer;
                c.origin = origin;
                c.req = Some((sid, rid));
                col.add(c, ctx);
            }
        }
        "Network.responseReceived" => {
            let rid = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
            let resp = &p["response"];
            let url = resp.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let status = resp.get("status").and_then(Value::as_u64).unwrap_or(200);
            let mime = resp.get("mimeType").and_then(Value::as_str).unwrap_or("");
            if status >= 400 {
                // Playlist refusée ou introuvable : ce n'est pas un flux utilisable.
                col.cands.retain(|c| c.url != url);
                return;
            }
            let size = response_size(&resp["headers"]);
            let Some(kind) = classify_response(&url, mime, size) else {
                return;
            };
            let (referer, origin) = col
                .requests
                .get(&(sid.clone(), rid.clone()))
                .map(|(_, r, o)| (r.clone(), o.clone()))
                .unwrap_or((None, None));
            let mut c = Candidate::new(&url, kind, size);
            c.referer = referer;
            c.origin = origin;
            c.req = Some((sid, rid));
            col.add(c, ctx);
            // Mise à jour de la taille d'un candidat déjà vu sur la requête.
            if let Some(e) = col.cands.iter_mut().find(|e| e.url == url) {
                if e.size.is_none() {
                    e.size = size;
                }
            }
        }
        "Network.loadingFinished" => {
            let rid = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
            let key = Some((sid.clone(), rid.clone()));
            let wants_body = col
                .cands
                .iter()
                .any(|c| c.req == key && c.body.is_none() && matches!(c.kind, MediaKind::Hls | MediaKind::Dash));
            if !wants_body {
                return;
            }
            let r = cdp
                .call_t(
                    "Network.getResponseBody",
                    json!({"requestId": rid}),
                    Some(&sid),
                    Duration::from_secs(4),
                )
                .await;
            if let Ok(v) = r {
                let body = v.get("body").and_then(Value::as_str).unwrap_or("");
                let text = if v.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
                    String::new() // une playlist n'est jamais encodée en base64
                } else {
                    body.to_string()
                };
                if !text.is_empty() {
                    if let Some(c) = col.cands.iter_mut().find(|c| c.req == key) {
                        c.body = Some(text);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Tente de lancer la lecture partout ; renvoie le nombre de vidéos en lecture et le drapeau DRM.
async fn play_tick(cdp: &Cdp, sessions: &Sessions, col: &mut Collector, ctx: &JobCtx) {
    let futs = sessions.all.iter().map(|(sid, _)| {
        cdp.call_t(
            "Runtime.evaluate",
            json!({"expression": PLAY_JS, "returnByValue": true}),
            Some(sid.as_str()),
            Duration::from_secs(3),
        )
    });
    let results = futures::future::join_all(futs).await;
    let mut playing = 0u64;
    for r in results.into_iter().flatten() {
        let v = &r["result"]["value"];
        playing += v.get("p").and_then(Value::as_u64).unwrap_or(0);
        if v.get("drm").and_then(Value::as_bool) == Some(true) && !col.drm_evidence {
            ctx.warn("Lecture chiffrée (EME) détectée dans la page.");
            col.drm_evidence = true;
        }
    }
    if playing == 0 {
        // Clic au centre de la page (bouton de lecture, affiche du lecteur).
        let page = Some(sessions.page.as_str());
        for t in ["mouseMoved", "mousePressed", "mouseReleased"] {
            let _ = cdp
                .call_t(
                    "Input.dispatchMouseEvent",
                    json!({"type": t, "x": 640, "y": 360, "button": "left", "clickCount": 1}),
                    page,
                    Duration::from_secs(3),
                )
                .await;
        }
    }
}

/// Cookies du navigateur applicables à ces URL, au format de l'en-tête `Cookie`.
async fn cookie_header(cdp: &Cdp, sid: &str, urls: &[&str]) -> Option<String> {
    let v = cdp
        .call_t("Network.getCookies", json!({"urls": urls}), Some(sid), Duration::from_secs(5))
        .await
        .ok()?;
    let parts: Vec<String> = v
        .get("cookies")?
        .as_array()?
        .iter()
        .filter_map(|c| Some(format!("{}={}", c.get("name")?.as_str()?, c.get("value")?.as_str()?)))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

fn origin_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url).ok().map(|u| u.origin().ascii_serialization())
}

/// Observation de la page ; renvoie le flux retenu (ou une erreur définitive).
async fn session(
    cdp: &Cdp,
    mut events: mpsc::UnboundedReceiver<Event>,
    url: &str,
    hint: &ResolveHint,
    ctx: &JobCtx,
) -> anyhow::Result<Option<ResolvedMedia>> {
    // Cible : l'onglet initial s'il existe, sinon un nouvel onglet.
    let targets = cdp.call("Target.getTargets", json!({}), None).await?;
    let existing = targets
        .get("targetInfos")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        })
        .and_then(|t| t.get("targetId").and_then(Value::as_str))
        .map(str::to_string);
    let target_id = match existing {
        Some(id) => id,
        None => cdp
            .call("Target.createTarget", json!({"url": "about:blank"}), None)
            .await?
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Le navigateur n'a pas créé d'onglet"))?
            .to_string(),
    };
    let _ = cdp
        .call("Target.setDiscoverTargets", json!({"discover": true}), None)
        .await;
    let attached = cdp
        .call("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None)
        .await?;
    let page_sid = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Impossible de piloter l'onglet du navigateur"))?
        .to_string();

    let mut sessions = Sessions {
        page: page_sid.clone(),
        all: vec![(page_sid.clone(), "page".to_string())],
    };
    let mut col = Collector::default();
    setup_session(cdp, &page_sid, "page", false).await;

    ctx.info("Ouverture de la page dans le navigateur…");
    let mut nav = json!({"url": url});
    if let Some(r) = hint.referer.as_deref().filter(|r| !r.is_empty()) {
        nav["referrer"] = json!(r);
    }
    let nav_res = cdp.call_t("Page.navigate", nav, Some(&page_sid), Duration::from_secs(30)).await?;
    if let Some(err) = nav_res.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
        if err != "net::ERR_ABORTED" {
            anyhow::bail!("Impossible d'ouvrir la page dans le navigateur ({err}).");
        }
    }

    let start = Instant::now();
    let deadline = start + TOTAL_WAIT;
    let mut ticker = tokio::time::interval_at(start + Duration::from_millis(1500), PLAY_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let wake = col.stop_at(deadline);
        tokio::select! {
            _ = ctx.cancel.cancelled() => anyhow::bail!("Annulé"),
            _ = tokio::time::sleep_until(wake) => break,
            ev = events.recv() => match ev {
                Some(ev) => handle_event(cdp, ev, &mut col, &mut sessions, ctx).await,
                None => {
                    ctx.warn("Le navigateur s'est fermé pendant l'analyse de la page.");
                    break;
                }
            },
            _ = ticker.tick() => play_tick(cdp, &sessions, &mut col, ctx).await,
        }
    }
    ctx.check()?;

    // Cookies (toutes cibles confondues) : page + candidats
    let mut urls: Vec<&str> = vec![url];
    urls.extend(col.cands.iter().map(|c| c.url.as_str()));
    let cookies_all = cookie_header(cdp, &page_sid, &urls).await;

    // Playlists dont on n'a pas pu lire le corps : récupération directe, avec les en-têtes capturés.
    for i in 0..col.cands.len() {
        let c = &col.cands[i];
        if c.body.is_some() || !matches!(c.kind, MediaKind::Hls | MediaKind::Dash) || is_ad_url(&c.url) || i >= 8 {
            continue;
        }
        let http = http_for(c, url, cookies_all.clone());
        let fetched = tokio::time::timeout(Duration::from_secs(8), async {
            http.apply(HttpCtx::client().get(&c.url)).send().await?.text().await
        })
        .await;
        if let Ok(Ok(text)) = fetched {
            col.cands[i].body = Some(text);
        }
    }
    // Une « playlist » qui n'en est pas une (page d'erreur HTML…) n'est pas un flux.
    col.cands.retain(|c| {
        !(c.kind == MediaKind::Hls && c.body.as_deref().is_some_and(|b| !b.contains("#EXTM3U")))
    });

    let chosen = pick_best(&col.cands);
    let Some(idx) = chosen else {
        if col.drm_evidence {
            ctx.error(MSG_DRM);
            anyhow::bail!(MSG_DRM);
        }
        return Ok(None);
    };
    let c = col.cands[idx].clone();
    if c.body.as_deref().is_some_and(manifest_is_drm) {
        ctx.error(MSG_DRM);
        anyhow::bail!(MSG_DRM);
    }
    ctx.info(format!("Flux retenu ({:?}) : {}", c.kind, short(&c.url)));

    let title = cdp
        .call_t(
            "Runtime.evaluate",
            json!({"expression": "document.title", "returnByValue": true}),
            Some(&page_sid),
            Duration::from_secs(3),
        )
        .await
        .ok()
        .and_then(|v| v["result"]["value"].as_str().map(|s| s.trim().to_string()))
        .filter(|t| !t.is_empty());

    let cookies = cookie_header(cdp, &page_sid, &[c.url.as_str()]).await.or(cookies_all);
    Ok(Some(ResolvedMedia {
        kind: c.kind,
        url: c.url.clone(),
        http: http_for(&c, url, cookies),
        via: ResolvedVia::Browser,
        title,
        duration: None,
        height: None,
    }))
}

/// En-têtes de téléchargement : Referer/Origin de la requête capturée, cookies, user-agent.
fn http_for(c: &Candidate, page_url: &str, cookies: Option<String>) -> HttpCtx {
    let referer = c.referer.clone().or_else(|| Some(page_url.to_string()));
    let origin = c
        .origin
        .clone()
        .or_else(|| referer.as_deref().and_then(origin_of));
    HttpCtx {
        referer,
        origin,
        user_agent: BROWSER_UA.to_string(),
        cookies,
        extra_headers: Vec::new(),
    }
}

/// Ouvre la page dans le navigateur et renvoie le flux détecté (`None` : rien trouvé).
pub async fn resolve(url: &str, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<Option<ResolvedMedia>> {
    let Some(info) = locate(hint.browser) else {
        anyhow::bail!(match hint.browser {
            BrowserChoice::Chrome => "Google Chrome n'est pas installé : impossible d'analyser la page. Choisissez un autre navigateur dans les réglages.",
            BrowserChoice::Edge => "Microsoft Edge n'est pas installé : impossible d'analyser la page. Choisissez un autre navigateur dans les réglages.",
            BrowserChoice::Auto => "Ni Google Chrome ni Microsoft Edge ne sont installés : impossible d'analyser la page. Installez l'un des deux pour détecter les vidéos intégrées aux pages.",
        });
    };

    let browser = Browser::launch(&info, hint, ctx).await?;
    // `browser` est nettoyé même si `launch` a réussi et que la suite échoue (Drop + shutdown).
    let result = async {
        let (cdp, events) = Cdp::connect(&browser.ws_url).await?;
        let r = session(&cdp, events, url, hint, ctx).await;
        // Fermeture propre ; le kill est fait par `shutdown` si le navigateur ne part pas.
        let _ = cdp
            .call_t("Browser.close", json!({}), None, Duration::from_secs(2))
            .await;
        r
    }
    .await;
    browser.shutdown().await;
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(url: &str, kind: MediaKind, size: Option<u64>, seq: usize, master: Option<bool>) -> Candidate {
        let mut c = Candidate::new(url, kind, size);
        c.seq = seq;
        c.body = master.map(|m| {
            if m {
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8".to_string()
            } else {
                "#EXTM3U\n#EXTINF:4,\nseg.ts".to_string()
            }
        });
        c
    }

    #[test]
    fn priorite_master_media_mpd_fichier() {
        let all = vec![
            cand("https://a.com/video.mp4", MediaKind::File, Some(90_000_000), 0, None),
            cand("https://a.com/manifest.mpd", MediaKind::Dash, None, 1, None),
            cand("https://a.com/media_720.m3u8", MediaKind::Hls, None, 2, Some(false)),
            cand("https://a.com/master.m3u8", MediaKind::Hls, None, 3, Some(true)),
        ];
        assert_eq!(pick_best(&all), Some(3));
        assert_eq!(pick_best(&all[..3]), Some(2));
        assert_eq!(pick_best(&all[..2]), Some(1));
        assert_eq!(pick_best(&all[..1]), Some(0));
        assert_eq!(pick_best(&[]), None);
    }

    #[test]
    fn plus_gros_fichier_puis_plus_ancien() {
        let all = vec![
            cand("https://a.com/petit.mp4", MediaKind::File, Some(2_000_000), 0, None),
            cand("https://a.com/gros.mp4", MediaKind::File, Some(80_000_000), 1, None),
        ];
        assert_eq!(pick_best(&all), Some(1));
        let two = vec![
            cand("https://a.com/1.m3u8", MediaKind::Hls, None, 0, Some(true)),
            cand("https://a.com/2.m3u8", MediaKind::Hls, None, 1, Some(true)),
        ];
        assert_eq!(pick_best(&two), Some(0));
    }

    #[test]
    fn filtre_publicitaire() {
        for u in [
            "https://ads.example.com/preroll.m3u8",
            "https://pubads.g.doubleclick.net/ondemand/x.m3u8",
            "https://cdn.example.com/vast/clip.mp4",
            "https://cdn.example.com/content/preroll/index.m3u8",
            "https://cdn.example.com/cdn-ads/v.m3u8",
            "https://cdn.example.com/ads2/v.m3u8",
        ] {
            assert!(is_ad_url(u), "{u}");
        }
        for u in [
            "https://cdn.example.com/uploads/video.mp4",
            "https://downloads.example.com/headsup/master.m3u8",
            "https://cdn.example.com/hls/master.m3u8?ads=0",
            "https://cdn.example.com/hls/leads.m3u8",
        ] {
            assert!(!is_ad_url(u), "{u}");
        }
        let all = vec![
            cand("https://ads.example.com/ad.m3u8", MediaKind::Hls, None, 0, Some(true)),
            cand("https://cdn.example.com/film.m3u8", MediaKind::Hls, None, 1, Some(false)),
        ];
        assert_eq!(pick_best(&all), Some(1));
        assert_eq!(pick_best(&all[..1]), None);
    }

    #[test]
    fn classification_des_reponses() {
        let k = |u: &str, m: &str, s| classify_response(u, m, s);
        assert_eq!(k("https://a.com/x", "application/vnd.apple.mpegurl", None), Some(MediaKind::Hls));
        assert_eq!(k("https://a.com/x", "audio/x-mpegURL", None), Some(MediaKind::Hls));
        assert_eq!(k("https://a.com/x", "application/dash+xml", None), Some(MediaKind::Dash));
        assert_eq!(k("https://a.com/p.m3u8?t=1", "text/plain", None), Some(MediaKind::Hls));
        assert_eq!(k("https://a.com/m.mpd", "application/octet-stream", None), Some(MediaKind::Dash));
        assert_eq!(k("https://a.com/v.mp4", "video/mp4", Some(50_000_000)), Some(MediaKind::File));
        assert_eq!(k("https://a.com/stream", "video/webm", None), Some(MediaKind::File));
        // Trop petit, segments, autres types
        assert_eq!(k("https://a.com/v.mp4", "video/mp4", Some(300_000)), None);
        assert_eq!(k("https://a.com/seg1.ts", "video/mp2t", Some(5_000_000)), None);
        assert_eq!(k("https://a.com/seg1.m4s", "video/mp4", Some(5_000_000)), None);
        assert_eq!(k("https://a.com/seg", "video/mp2t", None), None);
        assert_eq!(k("https://a.com/page", "text/html", None), None);
        assert_eq!(k("https://a.com/img.jpg", "image/jpeg", Some(9_000_000)), None);
    }

    #[test]
    fn taille_des_reponses() {
        let h = json!({"content-length": "123", "Content-Range": "bytes 0-99/5000000"});
        assert_eq!(response_size(&h), Some(5_000_000));
        assert_eq!(response_size(&json!({"Content-Length": "42"})), Some(42));
        assert_eq!(response_size(&json!({})), None);
    }

    #[test]
    fn detection_drm_des_manifestes() {
        assert!(manifest_is_drm(
            "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,KEYFORMAT=\"urn:uuid:EDEF8BA9-79D6-4ACE-A3C8-27DCD51D21ED\""
        ));
        assert!(manifest_is_drm("#EXT-X-KEY:METHOD=SAMPLE-AES,KEYFORMAT=\"com.apple.streamingkeydelivery\""));
        assert!(manifest_is_drm("<ContentProtection schemeIdUri=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\"/>"));
        assert!(!manifest_is_drm("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\"\n#EXTINF:4,\ns.ts"));
        assert!(is_license_url("https://lic.example.com/widevine/license"));
        assert!(!is_license_url("https://cdn.example.com/master.m3u8"));
    }

    #[test]
    fn localisation_chrome_edge() {
        let dir = tempfile::tempdir().expect("tmp");
        let mk = |name: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, b"").expect("fichier");
            p
        };
        let chrome = mk("chrome.exe");
        let edge = mk("msedge.exe");
        let absent = dir.path().join("absent.exe");

        let c = [absent.clone(), chrome.clone()];
        let e = [edge.clone()];
        let auto = locate_in(BrowserChoice::Auto, &c, &e).expect("auto");
        assert_eq!((auto.name, auto.path.clone()), ("Chrome", chrome.clone()));
        let forced_edge = locate_in(BrowserChoice::Edge, &c, &e).expect("edge");
        assert_eq!((forced_edge.name, forced_edge.path), ("Edge", edge.clone()));
        // Pas de Chrome : Auto retombe sur Edge ; Chrome forcé échoue.
        let no_chrome = [absent.clone()];
        let fallback = locate_in(BrowserChoice::Auto, &no_chrome, &e).expect("secours");
        assert_eq!(fallback.name, "Edge");
        assert!(locate_in(BrowserChoice::Chrome, &no_chrome, &e).is_none());
        assert!(locate_in(BrowserChoice::Auto, &no_chrome, &[absent]).is_none());
    }

    #[test]
    fn port_devtools_lu_dans_le_profil() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(read_devtools_port(dir.path()), None);
        std::fs::write(dir.path().join("DevToolsActivePort"), "9222\n/devtools/browser/abc-def").expect("w");
        assert_eq!(
            read_devtools_port(dir.path()).as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-def")
        );
    }

    /// Lance réellement Chrome/Edge sur une page locale contenant un `<video>` vers un m3u8.
    /// `cargo test -- --ignored navigateur_reel`
    #[tokio::test]
    #[ignore]
    async fn navigateur_reel_detecte_le_m3u8_local() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Some(info) = locate(BrowserChoice::Auto) else {
            eprintln!("Ni Chrome ni Edge installé : test ignoré");
            return;
        };
        eprintln!("navigateur : {} ({})", info.name, info.path.display());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut got = Vec::new();
                    loop {
                        let Ok(n) = sock.read(&mut buf).await else { return };
                        if n == 0 {
                            return;
                        }
                        got.extend_from_slice(&buf[..n]);
                        if got.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = String::from_utf8_lossy(&got).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, ctype, body) = match path.as_str() {
                        "/" => (
                            "200 OK",
                            "text/html; charset=utf-8",
                            "<html><head><title>Page de test Ursa</title></head><body>\
                             <video id=v src=\"/master.m3u8\" muted autoplay controls width=640 height=360></video>\
                             <script>setTimeout(function(){fetch('/master.m3u8').then(function(r){return r.text()})\
                             .then(function(t){var m=t.split('\\n').filter(function(l){return l.indexOf('.m3u8')>0})[0];\
                             if(m)fetch('/'+m.trim())})},500)</script></body></html>"
                                .to_string(),
                        ),
                        "/master.m3u8" => (
                            "200 OK",
                            "application/vnd.apple.mpegurl",
                            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nmedia_360.m3u8\n".to_string(),
                        ),
                        "/media_360.m3u8" => (
                            "200 OK",
                            "application/vnd.apple.mpegurl",
                            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST\n".to_string(),
                        ),
                        _ => ("404 Not Found", "text/plain", "introuvable".to_string()),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        let page = format!("http://127.0.0.1:{port}/");
        let hint = ResolveHint::default();
        let before: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        let media = resolve(&page, &hint, &JobCtx::noop())
            .await
            .expect("résolution")
            .expect("un flux doit être détecté");
        assert_eq!(media.kind, MediaKind::Hls);
        assert_eq!(media.url, format!("http://127.0.0.1:{port}/master.m3u8"));
        assert_eq!(media.via, ResolvedVia::Browser);
        assert_eq!(media.title.as_deref(), Some("Page de test Ursa"));
        assert_eq!(media.http.referer.as_deref(), Some(page.as_str()));
        assert_eq!(media.http.user_agent, BROWSER_UA);

        // Le profil temporaire a bien été supprimé.
        let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name())
                    .filter(|n| n.to_string_lossy().starts_with("ursa-browser-") && !before.contains(n))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "profil temporaire non supprimé : {leftovers:?}");
    }
}
