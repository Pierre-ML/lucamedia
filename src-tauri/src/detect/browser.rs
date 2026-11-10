//! Étape 3 : navigateur (Chrome, sinon Edge) piloté par le protocole DevTools (CDP).
//!
//! On ouvre la page dans un navigateur au profil temporaire, on essaie de lancer la lecture
//! (vrais clics souris CDP) et on écoute le réseau de la page, de ses iframes (même hors
//! processus) et de ses workers pour repérer le flux (m3u8, mpd, fichier vidéo).
//!
//! Deux passes : d'abord en arrière-plan (headless) ; si rien n'est trouvé, une passe « assistée »
//! avec le navigateur visible, où l'utilisateur lance lui-même la lecture (publicités, captcha…).
//! Aucun contournement de DRM : si le flux est protégé, on le dit et on s'arrête.

use super::{
    BrowserChoice, MediaKind, ResolveHint, ResolvedMedia, ResolvedVia, MSG_ASSIST_DELAI, MSG_ASSIST_FERME,
    MSG_DRM,
};
use crate::model::{HttpCtx, JobCtx, BROWSER_UA};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

/// Durée totale maximale d'observation de la page en arrière-plan.
const AUTO_WAIT: Duration = Duration::from_secs(40);
/// Durée maximale d'attente de l'utilisateur dans la fenêtre visible.
const ASSIST_WAIT: Duration = Duration::from_secs(180);
/// Délai d'arrêt après le premier candidat m3u8 (laisse arriver la playlist maître).
const AFTER_HLS: Duration = Duration::from_secs(4);
/// Délai d'arrêt après le premier candidat mpd / fichier vidéo.
const AFTER_OTHER: Duration = Duration::from_secs(6);
/// Intervalle entre deux tentatives de lecture.
const PLAY_EVERY: Duration = Duration::from_secs(2);
/// Taille minimale d'un fichier vidéo pour être retenu (les petits sont des extraits ou des segments).
const MIN_FILE_BYTES: u64 = 1_000_000;
/// Taille maximale d'une réponse lue pour y reconnaître une playlist.
const MAX_SNIFF_BYTES: u64 = 2_000_000;
/// Nombre maximal de réponses sans type reconnaissable dont on lit le corps.
const MAX_SNIFFS: usize = 60;

/// Durées d'observation (réglables pour les tests).
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub auto: Duration,
    pub assist: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            auto: AUTO_WAIT,
            assist: ASSIST_WAIT,
        }
    }
}

// ---------------------------------------------------------------------------
// Localisation du navigateur
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowserInfo {
    /// « Chrome » ou « Edge ».
    pub name: &'static str,
    pub path: PathBuf,
}

fn env_path(var: &str, rest: &str) -> Option<PathBuf> {
    let base = std::env::var_os(var).filter(|v| !v.is_empty())?;
    Some(PathBuf::from(base).join(rest))
}

/// Emplacements habituels de Chrome, par ordre de préférence.
fn chrome_candidates() -> Vec<PathBuf> {
    let rest = r"Google\Chrome\Application\chrome.exe";
    ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
        .iter()
        .filter_map(|v| env_path(v, rest))
        .collect()
}

/// Emplacements habituels d'Edge, par ordre de préférence.
fn edge_candidates() -> Vec<PathBuf> {
    let rest = r"Microsoft\Edge\Application\msedge.exe";
    ["ProgramFiles(x86)", "ProgramFiles"]
        .iter()
        .filter_map(|v| env_path(v, rest))
        .collect()
}

/// Choix du navigateur parmi des chemins candidats donnés (testable).
fn locate_in(choice: BrowserChoice, chrome: &[PathBuf], edge: &[PathBuf]) -> Option<BrowserInfo> {
    let find = |list: &[PathBuf]| list.iter().find(|p| p.is_file()).cloned();
    let chrome_info = || {
        find(chrome).map(|path| BrowserInfo {
            name: "Chrome",
            path,
        })
    };
    let edge_info = || {
        find(edge).map(|path| BrowserInfo {
            name: "Edge",
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
fn is_ad_url(url: &str) -> bool {
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
fn manifest_is_drm(text: &str) -> bool {
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

/// Type de flux deviné d'après le chemin de l'URL : playlists m3u8 / mpd.
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

/// Type de flux deviné d'après la requête de l'URL (`/proxy?file=master.m3u8`).
fn manifest_kind_in_query(url: &str) -> Option<MediaKind> {
    let q = reqwest::Url::parse(url).ok()?.query()?.to_ascii_lowercase();
    if q.contains(".m3u8") {
        Some(MediaKind::Hls)
    } else if q.contains(".mpd") {
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

/// Type MIME qui ne peut pas être une playlist (page, image, script, style, JSON, police, vidéo).
fn mime_is_never_a_manifest(mime: &str) -> bool {
    mime.starts_with("text/html")
        || mime.starts_with("image/")
        || mime.starts_with("font/")
        || mime.starts_with("video/")
        || mime.starts_with("text/css")
        || mime.contains("javascript")
        || mime.contains("ecmascript")
        || mime.contains("json")
}

/// Un flux est-il annoncé par cette réponse ? (Content-Type, URL, taille)
///
/// Les segments (`.ts`, `.m4s`…) et les fichiers de moins de 1 Mo sont ignorés.
fn classify_response(url: &str, mime: &str, size: Option<u64>) -> Option<MediaKind> {
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
    if let Some(k) = manifest_kind_in_query(url) {
        if !mime_is_never_a_manifest(&mime) {
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

/// La réponse (dont le type n'est pas reconnaissable) vaut-elle la peine d'être lue pour y trouver
/// une playlist ? Requêtes de script (XHR, fetch, médias) à type vague ou texte, hors segments.
fn is_sniffable(rtype: &str, url: &str, mime: &str, size: Option<u64>) -> bool {
    if !matches!(rtype, "XHR" | "Fetch" | "Other" | "Media") {
        return false;
    }
    if size.is_some_and(|s| s > MAX_SNIFF_BYTES) || is_segment_path(&path_of(url)) {
        return false;
    }
    let mime = mime.to_ascii_lowercase();
    if mime_is_never_a_manifest(&mime) {
        return false;
    }
    mime.is_empty()
        || mime.starts_with("text/")
        || mime.starts_with("application/")
        || mime.starts_with("binary/")
}

/// Reconnaît une playlist HLS (`#EXTM3U`) ou un manifeste DASH (`<MPD`) d'après le début du corps.
fn manifest_kind_of_body(text: &str) -> Option<MediaKind> {
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("#EXTM3U") {
        return Some(MediaKind::Hls);
    }
    let head: String = t.chars().take(2000).collect();
    if head.contains("<MPD") {
        return Some(MediaKind::Dash);
    }
    None
}

/// Décode du base64 standard (les réponses non textuelles de `Network.getResponseBody`).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
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
fn pick_best(cands: &[Candidate]) -> Option<usize> {
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

/// Points de clic pour ce tour : `targets` = `[x, y, largeur, hauteur, priorité]` (zones visibles
/// de la page, repère de la fenêtre). Les zones sont triées par priorité puis par taille ; trois
/// clics par tour. Les trois premiers tours visent toujours les mêmes zones (certains lecteurs
/// ouvrent une publicité au premier clic et ne démarrent qu'au second) ; ensuite la fenêtre
/// glisse sur toute la liste et le point visé varie dans la zone.
fn plan_clicks(targets: &[Value], tick: usize) -> Vec<(f64, f64)> {
    let mut zones: Vec<(u8, f64, f64, f64, f64)> = targets
        .iter()
        .filter_map(|t| {
            let a = t.as_array()?;
            let n = |i: usize| a.get(i).and_then(Value::as_f64);
            Some((n(4)? as u8, n(0)?, n(1)?, n(2)?, n(3)?))
        })
        .collect();
    zones.sort_by(|a, b| a.0.cmp(&b.0).then((b.3 * b.4).total_cmp(&(a.3 * a.4))));
    let mut kept: Vec<(u8, f64, f64, f64, f64)> = Vec::new();
    for z in zones {
        let (cx, cy) = (z.1 + z.3 / 2.0, z.2 + z.4 / 2.0);
        let near = kept
            .iter()
            .any(|k| (k.1 + k.3 / 2.0 - cx).abs() < 10.0 && (k.2 + k.4 / 2.0 - cy).abs() < 10.0);
        if !near {
            kept.push(z);
        }
    }
    if kept.is_empty() {
        return Vec::new();
    }
    let n = kept.len();
    let start = if tick < 3 { 0 } else { (tick - 3) * 2 % n };
    const JITTER: [(f64, f64); 5] = [(0.0, 0.0), (0.2, 0.0), (-0.2, 0.0), (0.0, 0.2), (0.0, -0.2)];
    let (jx, jy) = if tick < 3 { (0.0, 0.0) } else { JITTER[(tick / 3) % JITTER.len()] };
    (0..n.min(3))
        .map(|i| {
            let z = kept[(start + i) % n];
            (z.1 + z.3 * (0.5 + jx), z.2 + z.4 * (0.5 + jy))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Client CDP minimal (WebSocket + JSON)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(super) struct Event {
    session_id: Option<String>,
    method: String,
    params: Value,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Arrête les tâches de lecture / écriture quand le dernier clone du client disparaît.
struct TaskGuard(Vec<tokio::task::JoinHandle<()>>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

/// Connexion CDP ; les clones partagent la même connexion.
#[derive(Clone)]
pub(super) struct Cdp {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicU64>,
    _tasks: Arc<TaskGuard>,
}

impl Cdp {
    pub(super) async fn connect(ws_url: &str) -> anyhow::Result<(Cdp, mpsc::UnboundedReceiver<Event>)> {
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
                next: Arc::new(AtomicU64::new(1)),
                _tasks: Arc::new(TaskGuard(vec![writer, reader])),
            },
            ev_rx,
        ))
    }

    pub(super) async fn call_t(
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

    pub(super) async fn call(&self, method: &str, params: Value, session: Option<&str>) -> anyhow::Result<Value> {
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
pub(super) fn read_devtools_port(profile: &Path) -> Option<String> {
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
    /// Lance le navigateur ; `visible` : fenêtre normale (mode assisté), sinon headless.
    async fn launch(info: &BrowserInfo, visible: bool, ctx: &JobCtx) -> anyhow::Result<Browser> {
        let profile = new_profile_dir();
        std::fs::create_dir_all(&profile)
            .map_err(|e| anyhow::anyhow!("Impossible de créer le profil temporaire du navigateur : {e}"))?;

        let mut cmd = Command::new(&info.path);
        cmd.arg(format!("--user-data-dir={}", profile.display()))
            .arg("--remote-debugging-port=0")
            .arg("--remote-allow-origins=*")
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--autoplay-policy=no-user-gesture-required")
            .arg(format!("--user-agent={BROWSER_UA}"))
            .arg("--window-size=1280,720")
            .arg("--disable-background-networking")
            .arg("--disable-component-update")
            .arg("--disable-sync")
            // Pas de signe d'automatisation : ni `navigator.webdriver`, ni bandeau « logiciel de test ».
            .arg("--disable-blink-features=AutomationControlled")
            // Fenêtres et onglets cachés ou recouverts : pas de mise en veille des pages.
            .arg("--disable-renderer-backgrounding")
            .arg("--disable-background-timer-throttling")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-features=CalculateNativeWinOcclusion")
            .arg("--password-store=basic");
        if visible {
            cmd.arg("--window-position=60,40");
        } else {
            cmd.arg("--headless=new").arg("--mute-audio");
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

/// Injecté dans chaque document (page et iframes) avant ses propres scripts : détecte les usages
/// réels d'EME (événement `encrypted`, `setMediaKeys` avec des clés) et gomme les signes
/// d'automatisation les plus courants (`navigator.webdriver`, `window.chrome`, langues).
const PAGE_HOOK_JS: &str = r#"(() => { try {
  const mark = () => { try { window.__ursaDrm = true; } catch (e) {} };
  document.addEventListener('encrypted', mark, true);
  const p = window.HTMLMediaElement && HTMLMediaElement.prototype;
  if (p && p.setMediaKeys) {
    const o = p.setMediaKeys;
    p.setMediaKeys = function (k) { if (k) mark(); return o.apply(this, arguments); };
  }
} catch (e) {}
try {
  const def = (o, k, g) => { try { Object.defineProperty(o, k, { get: g, configurable: true }); } catch (e) {} };
  if (navigator.webdriver) def(Navigator.prototype, 'webdriver', () => false);
  if (!window.chrome) window.chrome = { runtime: {}, app: { isInstalled: false } };
  if (!navigator.languages || !navigator.languages.length) def(Navigator.prototype, 'languages', () => ['fr-FR', 'fr', 'en-US', 'en']);
} catch (e) {} })();"#;

/// Exécuté dans chaque document accessible. Renvoie l'état de lecture (`v` vidéos, `p` en lecture,
/// `drm`) et, si `ACT`, les zones à cliquer `t` = `[x, y, largeur, hauteur, priorité]` dans le repère
/// de la fenêtre (les iframes de même origine sont parcourues avec leur décalage ; une iframe
/// d'une autre origine est cliquée en son centre, le clic réel est routé par le navigateur).
/// Priorités : 0 bouton de lecture, 1 vidéo, 2 iframe, 3 lecteur / grande zone cliquable, 4-5 repli.
const PLAY_JS: &str = r##"(() => {
  const ACT = __ACT__;
  const out = { v: 0, p: 0, drm: !!window.__ursaDrm, vw: window.innerWidth, vh: window.innerHeight, t: [] };
  const PLAY_BTN = ['.vjs-big-play-button', '.jw-icon-display', '.jw-display-icon-container', '.plyr__control--overlaid',
    '.ytp-large-play-button', '.vjs-play-control.vjs-paused', '[class*="big-play" i]', '[class*="play-button" i]',
    '[class*="playbutton" i]', '[class*="btn-play" i]', '[class*="play-btn" i]', '[aria-label*="play" i]',
    '[aria-label*="lire" i]', '[title*="play" i]', '[title*="lire" i]', '[data-testid*="play" i]'];
  const PLAYER = ['.video-js', '.jwplayer', '.plyr', '.flowplayer', '[class*="player" i]', '[id*="player" i]',
    '[class*="video-container" i]'];
  const TEXT_BTN = /^\s*(play|lire|lecture|regarder|watch|start|d[ée]marrer)\b/i;
  const add = (el, ox, oy, pr) => {
    try {
      let r = el.getBoundingClientRect();
      if (ox === 0 && oy === 0 && (r.bottom < 0 || r.top > out.vh)) {
        el.scrollIntoView({ block: 'center', inline: 'center' });
        r = el.getBoundingClientRect();
      }
      const x0 = Math.max(0, ox + r.left), y0 = Math.max(0, oy + r.top);
      const x1 = Math.min(out.vw, ox + r.right), y1 = Math.min(out.vh, oy + r.bottom);
      if (x1 - x0 < 4 || y1 - y0 < 4) return;
      const cs = el.ownerDocument.defaultView.getComputedStyle(el);
      if (cs.visibility === 'hidden' || cs.display === 'none' || cs.pointerEvents === 'none') return;
      out.t.push([x0, y0, x1 - x0, y1 - y0, pr]);
    } catch (e) {}
  };
  const visit = (doc, ox, oy, depth) => {
    try {
      doc.querySelectorAll('video').forEach(v => {
        out.v++;
        if (!v.paused) out.p++;
        if (v.mediaKeys) out.drm = true;
        if (ACT) {
          try { v.muted = true; const r = v.play(); if (r && r.catch) r.catch(() => {}); } catch (e) {}
          if (v.paused) add(v, ox, oy, 1);
        }
      });
      if (ACT) {
        PLAY_BTN.forEach(s => { try { doc.querySelectorAll(s).forEach(b => { b.click(); add(b, ox, oy, 0); }); } catch (e) {} });
        doc.querySelectorAll('button,[role="button"]').forEach(b => {
          const txt = (b.innerText || b.getAttribute('aria-label') || '').slice(0, 40);
          if (TEXT_BTN.test(txt)) add(b, ox, oy, 0);
        });
        PLAYER.forEach(s => { try { doc.querySelectorAll(s).forEach(b => add(b, ox, oy, 3)); } catch (e) {} });
        let n = 0;
        for (const el of doc.querySelectorAll('div,section,figure,canvas,span,img')) {
          if (++n > 1500) break;
          const r = el.getBoundingClientRect(), a = r.width * r.height;
          if (a < 30000 || a > 0.85 * out.vw * out.vh) continue;
          if (el.closest('a[href]') || doc.defaultView.getComputedStyle(el).cursor !== 'pointer') continue;
          add(el, ox, oy, 3);
        }
      }
      if (depth < 4) doc.querySelectorAll('iframe').forEach(f => {
        if (ACT) add(f, ox, oy, 2);
        try {
          if (f.contentDocument) {
            const r = f.getBoundingClientRect();
            visit(f.contentDocument, ox + r.left + f.clientLeft, oy + r.top + f.clientTop, depth + 1);
          }
        } catch (e) {}
      });
      if (doc.defaultView && doc.defaultView.__ursaDrm) out.drm = true;
    } catch (e) {}
  };
  visit(document, 0, 0, 0);
  if (ACT) {
    // Repli : centre de la fenêtre puis grille, en évitant les liens (ils feraient quitter la page).
    [[.5, .5], [.5, .3], [.5, .7], [.25, .5], [.75, .5], [.25, .3], [.75, .3], [.25, .7], [.75, .7]].forEach((q, i) => {
      try {
        const x = out.vw * q[0], y = out.vh * q[1], el = document.elementFromPoint(x, y);
        if (el && !el.closest('a[href]')) out.t.push([x - 2, y - 2, 4, 4, i ? 5 : 4]);
      } catch (e) {}
    });
  }
  return out;
})()"##;

// ---------------------------------------------------------------------------
// Collecte du réseau
// ---------------------------------------------------------------------------

/// Corps de réponse lu en tâche de fond.
struct BodyResult {
    key: ReqKey,
    url: String,
    text: Option<String>,
}

/// (session, requestId) CDP d'une requête.
type ReqKey = (String, String);
/// (url, referer, origin) d'une requête.
type ReqInfo = (String, Option<String>, Option<String>);

#[derive(Default)]
struct Collector {
    cands: Vec<Candidate>,
    /// (session, requestId) -> (url, referer, origin)
    requests: HashMap<ReqKey, ReqInfo>,
    /// Réponses dont on lira le corps pour y chercher une playlist.
    sniff: HashSet<ReqKey>,
    sniffed: usize,
    /// Lectures de corps en cours.
    inflight: usize,
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
            if existing.body.is_none() {
                existing.body = c.body;
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

    /// Corps d'une réponse lu : complète un candidat connu, ou en révèle un (URL sans extension
    /// ni Content-Type reconnaissable).
    fn on_body(&mut self, b: BodyResult, ctx: &JobCtx) {
        self.inflight = self.inflight.saturating_sub(1);
        let Some(text) = b.text else { return };
        if let Some(c) = self.cands.iter_mut().find(|c| c.url == b.url) {
            if c.body.is_none() {
                c.body = Some(text);
            }
            return;
        }
        let Some(kind) = manifest_kind_of_body(&text) else { return };
        let (referer, origin) = self
            .requests
            .get(&b.key)
            .map(|(_, r, o)| (r.clone(), o.clone()))
            .unwrap_or((None, None));
        let mut c = Candidate::new(&b.url, kind, None);
        c.referer = referer;
        c.origin = origin;
        c.body = Some(text);
        self.add(c, ctx);
    }
}

/// État des cibles du navigateur (onglets, iframes, workers).
struct State {
    /// Session de l'onglet principal.
    main: String,
    /// Sessions où l'on peut évaluer du JavaScript (onglets et iframes), l'onglet principal en premier.
    eval: Vec<String>,
    /// targetId -> session déjà ouverte (évite les doublons).
    sessions: HashMap<String, String>,
    /// Onglets encore ouverts.
    pages: HashSet<String>,
    /// Navigateur visible : on ne ferme pas les onglets ouverts par la page.
    visible: bool,
    bodies: mpsc::UnboundedSender<BodyResult>,
}

/// Prépare une session CDP (onglet, iframe, worker) : réseau, scripts, auto-attachement récursif.
async fn setup_session(cdp: &Cdp, sid: &str, ttype: &str, waiting: bool) {
    let t = Duration::from_secs(5);
    let s = Some(sid);
    let _ = cdp.call_t("Network.enable", json!({}), s, t).await;
    if matches!(ttype, "page" | "iframe") {
        let _ = cdp.call_t("Page.enable", json!({}), s, t).await;
        let _ = cdp
            .call_t(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source": PAGE_HOOK_JS, "runImmediately": true}),
                s,
                t,
            )
            .await;
        // La page se croit toujours au premier plan (lecteurs qui testent le focus).
        let _ = cdp
            .call_t("Emulation.setFocusEmulationEnabled", json!({"enabled": true}), s, t)
            .await;
    }
    // Iframes imbriquées et workers de cette cible. Sans `waitForDebuggerOnStart` : avec, Chrome
    // détache aussitôt les sessions des iframes hors processus, qui restent alors invisibles.
    let _ = cdp
        .call_t(
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
            s,
            t,
        )
        .await;
    if waiting {
        let _ = cdp.call_t("Runtime.runIfWaitingForDebugger", json!({}), s, t).await;
    }
}

async fn handle_event(cdp: &Cdp, ev: Event, col: &mut Collector, st: &mut State, ctx: &JobCtx) {
    let sid = ev.session_id.clone().unwrap_or_default();
    let p = &ev.params;
    match ev.method.as_str() {
        "Target.attachedToTarget" => {
            let Some(new_sid) = p.get("sessionId").and_then(Value::as_str) else {
                return;
            };
            let info = &p["targetInfo"];
            let ttype = info.get("type").and_then(Value::as_str).unwrap_or("");
            let tid = info.get("targetId").and_then(Value::as_str).unwrap_or("");
            let waiting = p.get("waitingForDebugger").and_then(Value::as_bool).unwrap_or(false);
            match st.sessions.get(tid) {
                Some(known) if known == new_sid => return,
                Some(_) => {
                    // Cible déjà suivie par une autre session : on relâche le doublon.
                    if waiting {
                        let _ = cdp
                            .call_t("Runtime.runIfWaitingForDebugger", json!({}), Some(new_sid), Duration::from_secs(3))
                            .await;
                    }
                    let _ = cdp
                        .call_t("Target.detachFromTarget", json!({"sessionId": new_sid}), None, Duration::from_secs(3))
                        .await;
                    return;
                }
                None => {}
            }
            if !matches!(ttype, "page" | "iframe" | "worker" | "service_worker" | "shared_worker") {
                if waiting {
                    let _ = cdp
                        .call_t("Runtime.runIfWaitingForDebugger", json!({}), Some(new_sid), Duration::from_secs(3))
                        .await;
                }
                return;
            }
            st.sessions.insert(tid.to_string(), new_sid.to_string());
            if ttype == "page" {
                st.pages.insert(tid.to_string());
            }
            if matches!(ttype, "page" | "iframe") {
                st.eval.push(new_sid.to_string());
            }
            setup_session(cdp, new_sid, ttype, waiting).await;
        }
        "Target.targetCreated" => {
            let info = &p["targetInfo"];
            let ttype = info.get("type").and_then(Value::as_str).unwrap_or("");
            let tid = info.get("targetId").and_then(Value::as_str).unwrap_or("");
            let url = info.get("url").and_then(Value::as_str).unwrap_or("");
            if tid.is_empty() || st.sessions.contains_key(tid) {
                return;
            }
            let is_popup = ttype == "page" && info.get("openerId").and_then(Value::as_str).is_some();
            let is_web_worker =
                matches!(ttype, "service_worker" | "shared_worker") && url.starts_with("http");
            if is_popup && !st.visible {
                // Fenêtre publicitaire ouverte par la page : fermée sans toucher à l'onglet principal.
                ctx.info("Fenêtre publicitaire fermée.");
                let (cdp, tid) = (cdp.clone(), tid.to_string());
                tokio::spawn(async move {
                    let _ = cdp
                        .call_t("Target.closeTarget", json!({"targetId": tid}), None, Duration::from_secs(3))
                        .await;
                });
            } else if is_popup || is_web_worker {
                // Nouvel onglet (mode visible) ou service worker : on observe aussi son réseau.
                // Le rattachement est traité à l'arrivée de `Target.attachedToTarget`.
                let (cdp, tid) = (cdp.clone(), tid.to_string());
                tokio::spawn(async move {
                    let _ = cdp
                        .call_t("Target.attachToTarget", json!({"targetId": tid, "flatten": true}), None, Duration::from_secs(5))
                        .await;
                });
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(s) = p.get("sessionId").and_then(Value::as_str) {
                st.eval.retain(|id| id != s || *id == st.main);
                st.sessions.retain(|_, v| v != s);
            }
        }
        "Target.targetDestroyed" => {
            if let Some(t) = p.get("targetId").and_then(Value::as_str) {
                st.pages.remove(t);
                st.sessions.remove(t);
            }
        }
        "Page.javascriptDialogOpening" => {
            // alert / confirm / beforeunload : sans réponse, la page reste bloquée.
            let _ = cdp
                .call_t("Page.handleJavaScriptDialog", json!({"accept": true}), Some(&sid), Duration::from_secs(3))
                .await;
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
                col.add(c, ctx);
            }
        }
        "Network.responseReceived" => {
            let rid = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
            let resp = &p["response"];
            let url = resp.get("url").and_then(Value::as_str).unwrap_or("").to_string();
            let status = resp.get("status").and_then(Value::as_u64).unwrap_or(200);
            let mime = resp.get("mimeType").and_then(Value::as_str).unwrap_or("");
            let rtype = p.get("type").and_then(Value::as_str).unwrap_or("");
            if status >= 400 {
                // Playlist refusée ou introuvable : ce n'est pas un flux utilisable.
                col.cands.retain(|c| c.url != url);
                return;
            }
            let size = response_size(&resp["headers"]);
            let Some(kind) = classify_response(&url, mime, size) else {
                // Type inconnu : on lira le corps, il peut s'agir d'une playlist sans extension.
                if is_sniffable(rtype, &url, mime, size) {
                    col.sniff.insert((sid, rid));
                }
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
            col.add(c, ctx);
        }
        "Network.loadingFinished" => {
            let rid = p.get("requestId").and_then(Value::as_str).unwrap_or("").to_string();
            let key = (sid.clone(), rid.clone());
            let Some(url) = col.requests.get(&key).map(|r| r.0.clone()) else {
                return;
            };
            let known = col
                .cands
                .iter()
                .any(|c| c.url == url && c.body.is_none() && matches!(c.kind, MediaKind::Hls | MediaKind::Dash));
            let sniff = col.sniff.remove(&key) && col.sniffed < MAX_SNIFFS;
            if !(known || sniff) {
                return;
            }
            if sniff {
                col.sniffed += 1;
            }
            col.inflight += 1;
            // Lecture en tâche de fond : elle ne bloque ni les événements ni les tentatives de lecture.
            let cdp = cdp.clone();
            let tx = st.bodies.clone();
            tokio::spawn(async move {
                let text = fetch_body(&cdp, &key.0, &key.1).await;
                let _ = tx.send(BodyResult { key, url, text });
            });
        }
        _ => {}
    }
}

/// Corps d'une réponse réseau (décodé si le navigateur l'a transmis en base64).
async fn fetch_body(cdp: &Cdp, sid: &str, rid: &str) -> Option<String> {
    let v = cdp
        .call_t("Network.getResponseBody", json!({"requestId": rid}), Some(sid), Duration::from_secs(4))
        .await
        .ok()?;
    let body = v.get("body").and_then(Value::as_str)?;
    let text = if v.get("base64Encoded").and_then(Value::as_bool) == Some(true) {
        let bytes = base64_decode(body)?;
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        body.to_string()
    };
    (!text.is_empty()).then_some(text)
}

/// Clic souris « de confiance » (vrais événements CDP, routés vers l'iframe sous le point).
async fn click_at(cdp: &Cdp, sid: &str, x: f64, y: f64) {
    let t = Duration::from_secs(3);
    let steps = [("mouseMoved", "none", 0, 0, 30u64), ("mousePressed", "left", 1, 1, 60), ("mouseReleased", "left", 0, 1, 0)];
    for (kind, button, buttons, count, pause) in steps {
        let _ = cdp
            .call_t(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": button, "buttons": buttons,
                       "clickCount": count, "pointerType": "mouse"}),
                Some(sid),
                t,
            )
            .await;
        if pause > 0 {
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    }
}

/// Tente de lancer la lecture partout (si `act`) et relève l'état : vidéos en lecture, DRM.
///
/// Tourne dans sa propre tâche : un clic qui ouvre une fenêtre suspend la page tant que cette
/// fenêtre n'a pas été traitée (fermée) par la boucle d'événements, qui doit donc rester libre.
async fn play_tick(
    cdp: Cdp,
    eval: Vec<String>,
    main: String,
    has_flux: bool,
    act: bool,
    tick: usize,
) -> TickReport {
    let expr = PLAY_JS.replace("__ACT__", if act { "true" } else { "false" });
    let futs = eval.iter().map(|sid| {
        cdp.call_t(
            "Runtime.evaluate",
            json!({"expression": expr, "returnByValue": true}),
            Some(sid.as_str()),
            Duration::from_secs(3),
        )
    });
    let results = futures::future::join_all(futs).await;
    let mut playing = 0u64;
    let mut drm = false;
    let mut targets: Vec<Value> = Vec::new();
    for (i, r) in results.into_iter().enumerate() {
        let Ok(r) = r else { continue };
        let v = &r["result"]["value"];
        playing += v.get("p").and_then(Value::as_u64).unwrap_or(0);
        drm |= v.get("drm").and_then(Value::as_bool) == Some(true);
        // Les zones n'ont de sens que dans le repère de l'onglet principal.
        if i == 0 {
            if let Some(t) = v.get("t").and_then(Value::as_array) {
                targets = t.clone();
            }
        }
    }
    // Plus de clic dès qu'un flux utilisable est vu : un clic de trop mettrait la lecture en pause.
    if act && playing == 0 && !has_flux {
        for (x, y) in plan_clicks(&targets, tick) {
            click_at(&cdp, &main, x, y).await;
        }
    }
    TickReport { drm }
}

/// Résultat d'un tour de lecture.
struct TickReport {
    drm: bool,
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

/// Résultat d'une passe d'observation.
struct Observed {
    media: Option<ResolvedMedia>,
    /// Le navigateur (ou tous ses onglets) a été fermé pendant l'observation.
    closed: bool,
}

/// Observation de la page ; renvoie le flux retenu (ou une erreur définitive).
async fn session(
    cdp: &Cdp,
    mut events: mpsc::UnboundedReceiver<Event>,
    url: &str,
    hint: &ResolveHint,
    ctx: &JobCtx,
    visible: bool,
    total: Duration,
) -> anyhow::Result<Observed> {
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
    let attached = cdp
        .call("Target.attachToTarget", json!({"targetId": target_id, "flatten": true}), None)
        .await?;
    let page_sid = attached
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Impossible de piloter l'onglet du navigateur"))?
        .to_string();

    let (bodies_tx, mut bodies_rx) = mpsc::unbounded_channel::<BodyResult>();
    let mut st = State {
        main: page_sid.clone(),
        eval: vec![page_sid.clone()],
        sessions: HashMap::from([(target_id.clone(), page_sid.clone())]),
        pages: HashSet::from([target_id.clone()]),
        visible,
        bodies: bodies_tx,
    };
    let mut col = Collector::default();
    setup_session(cdp, &page_sid, "page", false).await;

    // Onglets ouverts par la page (publicités) et service workers : découverte des cibles, puis
    // rattachement ou fermeture au cas par cas (voir `Target.targetCreated`). Un auto-attachement au
    // niveau du navigateur figerait la page qui ouvre une fenêtre.
    let _ = cdp
        .call("Target.setDiscoverTargets", json!({"discover": true}), None)
        .await;
    if visible {
        // Fenêtre au premier plan, ni réduite ni masquée.
        if let Ok(w) = cdp.call("Browser.getWindowForTarget", json!({"targetId": target_id}), None).await {
            if let Some(id) = w.get("windowId").and_then(Value::as_i64) {
                let _ = cdp
                    .call("Browser.setWindowBounds", json!({"windowId": id, "bounds": {"windowState": "normal"}}), None)
                    .await;
            }
        }
        let _ = cdp.call("Page.bringToFront", json!({}), Some(&page_sid)).await;
    }

    ctx.info("Ouverture de la page dans le navigateur…");
    let mut nav = json!({"url": url});
    if let Some(r) = hint.referer.as_deref().filter(|r| !r.is_empty()) {
        nav["referrer"] = json!(r);
    }
    // La navigation est suivie en parallèle des événements : une page lente à répondre ne bloque rien.
    let nav_cdp = cdp.clone();
    let nav_sid = page_sid.clone();
    let nav_fut = async move { nav_cdp.call_t("Page.navigate", nav, Some(&nav_sid), Duration::from_secs(90)).await };
    tokio::pin!(nav_fut);
    let mut nav_done = false;

    let start = Instant::now();
    let deadline = start + total;
    let mut ticker = tokio::time::interval_at(start + Duration::from_millis(1500), PLAY_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tick = 0usize;
    let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<TickReport>();
    let mut tick_running = false;
    let mut closed = false;
    loop {
        let wake = col.stop_at(deadline);
        tokio::select! {
            _ = ctx.cancel.cancelled() => anyhow::bail!("Annulé"),
            _ = tokio::time::sleep_until(wake) => break,
            r = &mut nav_fut, if !nav_done => {
                nav_done = true;
                let res = r?;
                if let Some(err) = res.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
                    if err != "net::ERR_ABORTED" {
                        anyhow::bail!("Impossible d'ouvrir la page dans le navigateur ({err}).");
                    }
                }
            }
            ev = events.recv() => match ev {
                Some(ev) => handle_event(cdp, ev, &mut col, &mut st, ctx).await,
                None => {
                    closed = true;
                    break;
                }
            },
            Some(b) = bodies_rx.recv() => col.on_body(b, ctx),
            Some(rep) = tick_rx.recv() => {
                tick_running = false;
                if rep.drm && !col.drm_evidence {
                    ctx.warn("Lecture chiffrée (EME) détectée dans la page.");
                    col.drm_evidence = true;
                }
            }
            _ = ticker.tick(), if !tick_running => {
                tick_running = true;
                // En mode assisté, l'utilisateur est aux commandes : on observe sans cliquer.
                let (cdp, eval, main) = (cdp.clone(), st.eval.clone(), st.main.clone());
                let (has_flux, act, n, tx) = (pick_best(&col.cands).is_some(), !visible, tick, tick_tx.clone());
                tokio::spawn(async move {
                    let _ = tx.send(play_tick(cdp, eval, main, has_flux, act, n).await);
                });
                tick += 1;
            }
        }
        if st.pages.is_empty() {
            closed = true;
            break;
        }
    }
    ctx.check()?;
    if closed && !visible {
        ctx.warn("Le navigateur s'est fermé pendant l'analyse de la page.");
    }

    // Corps encore en cours de lecture : courte attente.
    let patience = Instant::now() + Duration::from_secs(2);
    while col.inflight > 0 {
        match tokio::time::timeout_at(patience, bodies_rx.recv()).await {
            Ok(Some(b)) => col.on_body(b, ctx),
            _ => break,
        }
    }

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
        return Ok(Observed { media: None, closed });
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
    Ok(Observed {
        media: Some(ResolvedMedia {
            kind: c.kind,
            url: c.url.clone(),
            http: http_for(&c, url, cookies),
            via: ResolvedVia::Browser,
            title,
            duration: None,
            height: None,
        }),
        closed,
    })
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

fn not_installed_message(choice: BrowserChoice) -> &'static str {
    match choice {
        BrowserChoice::Chrome => "Google Chrome n'est pas installé : impossible d'analyser la page. Choisissez un autre navigateur dans les réglages.",
        BrowserChoice::Edge => "Microsoft Edge n'est pas installé : impossible d'analyser la page. Choisissez un autre navigateur dans les réglages.",
        BrowserChoice::Auto => "Ni Google Chrome ni Microsoft Edge ne sont installés : impossible d'analyser la page. Installez l'un des deux pour détecter les vidéos intégrées aux pages.",
    }
}

/// Une passe : lance le navigateur, observe la page, le ferme et supprime son profil.
async fn run_browser(
    info: &BrowserInfo,
    url: &str,
    hint: &ResolveHint,
    ctx: &JobCtx,
    visible: bool,
    total: Duration,
) -> anyhow::Result<Observed> {
    let browser = Browser::launch(info, visible, ctx).await?;
    // `browser` est nettoyé même si `launch` a réussi et que la suite échoue (Drop + shutdown).
    let result = async {
        let (cdp, events) = Cdp::connect(&browser.ws_url).await?;
        let r = session(&cdp, events, url, hint, ctx, visible, total).await;
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

/// Prévient l'interface pendant que la fenêtre visible attend l'utilisateur (toujours désactivé à la fin).
struct AssistGuard<'a>(&'a JobCtx);

impl<'a> AssistGuard<'a> {
    fn new(ctx: &'a JobCtx) -> Self {
        ctx.assist(true);
        AssistGuard(ctx)
    }
}

impl Drop for AssistGuard<'_> {
    fn drop(&mut self) {
        self.0.assist(false);
    }
}

/// Passe automatique (headless) seule : `None` si rien n'a été trouvé.
#[cfg(test)]
pub(super) async fn resolve_auto(
    url: &str,
    hint: &ResolveHint,
    ctx: &JobCtx,
    limits: Limits,
) -> anyhow::Result<Option<ResolvedMedia>> {
    let Some(info) = locate(hint.browser) else {
        anyhow::bail!(not_installed_message(hint.browser));
    };
    Ok(run_browser(&info, url, hint, ctx, false, limits.auto).await?.media)
}

pub(super) async fn resolve_with(
    url: &str,
    hint: &ResolveHint,
    ctx: &JobCtx,
    limits: Limits,
) -> anyhow::Result<ResolvedMedia> {
    let Some(info) = locate(hint.browser) else {
        anyhow::bail!(not_installed_message(hint.browser));
    };

    if hint.show_browser {
        ctx.info("Navigateur visible demandé : lancez la lecture de la vidéo dans la fenêtre qui s'ouvre.");
    } else {
        // 1. Passe automatique, en arrière-plan.
        if let Some(media) = run_browser(&info, url, hint, ctx, false, limits.auto).await?.media {
            return Ok(media);
        }
        ctx.info("Détection automatique sans résultat : ouverture du navigateur, lancez la lecture de la vidéo.");
    }

    // 2. Passe assistée : navigateur visible, l'utilisateur lance la lecture.
    let _assist = AssistGuard::new(ctx);
    let observed = run_browser(&info, url, hint, ctx, true, limits.assist).await?;
    match observed.media {
        Some(media) => Ok(media),
        None if observed.closed => anyhow::bail!(MSG_ASSIST_FERME),
        None => anyhow::bail!(MSG_ASSIST_DELAI),
    }
}

/// Ouvre la page dans le navigateur et renvoie le flux détecté : d'abord en arrière-plan, puis,
/// si rien n'est trouvé, dans une fenêtre visible où l'utilisateur lance lui-même la lecture.
pub async fn resolve(url: &str, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<ResolvedMedia> {
    resolve_with(url, hint, ctx, Limits::default()).await
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
        // Playlist citée dans la requête d'un proxy
        assert_eq!(k("https://a.com/proxy?file=master.m3u8", "text/plain", None), Some(MediaKind::Hls));
        assert_eq!(k("https://a.com/page?next=video.m3u8", "text/html", None), None);
    }

    #[test]
    fn reponses_dont_on_lit_le_corps() {
        assert!(is_sniffable("Fetch", "https://a.com/play?id=3", "text/plain", Some(200)));
        assert!(is_sniffable("XHR", "https://a.com/api/s", "application/octet-stream", None));
        assert!(is_sniffable("Other", "https://a.com/x", "", None));
        assert!(!is_sniffable("Image", "https://a.com/x", "text/plain", None));
        assert!(!is_sniffable("Fetch", "https://a.com/x", "application/json", None));
        assert!(!is_sniffable("Fetch", "https://a.com/x", "image/png", None));
        assert!(!is_sniffable("Fetch", "https://a.com/seg1.ts", "application/octet-stream", None));
        assert!(!is_sniffable("Fetch", "https://a.com/x", "text/plain", Some(50_000_000)));
    }

    #[test]
    fn reconnaissance_par_le_corps() {
        assert_eq!(manifest_kind_of_body("#EXTM3U\n#EXTINF:4,\ns.ts"), Some(MediaKind::Hls));
        assert_eq!(manifest_kind_of_body("\u{feff}\n  #EXTM3U\n"), Some(MediaKind::Hls));
        assert_eq!(
            manifest_kind_of_body("<?xml version=\"1.0\"?>\n<MPD type=\"static\">"),
            Some(MediaKind::Dash)
        );
        assert_eq!(manifest_kind_of_body("<html><body>#EXTM3U</body></html>"), None);
        assert_eq!(manifest_kind_of_body("{\"a\":1}"), None);
    }

    #[test]
    fn base64_decode_corps() {
        assert_eq!(base64_decode("I0VYVE0zVQ==").as_deref(), Some(&b"#EXTM3U"[..]));
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGVs\nbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a$b").is_none());
    }

    #[test]
    fn points_de_clic() {
        let t = |x: f64, y: f64, w: f64, h: f64, p: u8| json!([x, y, w, h, p]);
        assert!(plan_clicks(&[], 0).is_empty());
        // Priorité d'abord, puis la plus grande zone ; les doublons proches sont fusionnés.
        let zones = vec![
            t(0.0, 0.0, 100.0, 100.0, 3),
            t(300.0, 200.0, 400.0, 200.0, 2),
            t(498.0, 298.0, 4.0, 4.0, 4),
            t(60.0, 120.0, 480.0, 270.0, 0),
        ];
        assert_eq!(plan_clicks(&zones, 0), vec![(300.0, 255.0), (500.0, 300.0), (50.0, 50.0)]);
        // Après les trois premiers tours, la fenêtre glisse et le point varie dans la zone.
        let later = plan_clicks(&zones, 4);
        assert_eq!(later.len(), 3);
        assert_ne!(later, plan_clicks(&zones, 0));
        // Une seule zone : un seul clic.
        assert_eq!(plan_clicks(&zones[..1], 0), vec![(50.0, 50.0)]);
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
}
