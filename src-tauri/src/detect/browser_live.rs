//! Tests « navigateur réel » de la détection : de vraies pages locales (plusieurs ports, donc
//! plusieurs origines) servies par un petit serveur tokio, ouvertes dans le vrai Chrome / Edge.
//!
//! Tous les tests sont `#[ignore]` (ils lancent un navigateur) :
//! `cargo test --manifest-path src-tauri/Cargo.toml -- --ignored detect --test-threads=2`

use super::browser::{read_devtools_port, resolve_auto, resolve_with, Cdp, Limits};
use super::{MediaKind, ResolveHint, ResolvedMedia};
use crate::model::{JobCtx, BROWSER_UA};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// Petit serveur HTTP de test
// ---------------------------------------------------------------------------

pub struct Req {
    /// Chemin et requête (`/play?id=3`).
    pub path: String,
    /// En-têtes, noms en minuscules.
    pub headers: HashMap<String, String>,
}

pub struct Resp {
    status: u16,
    ctype: String,
    body: Vec<u8>,
    extra: Vec<(String, String)>,
    delay: Duration,
}

impl Resp {
    fn ok(ctype: &str, body: impl Into<Vec<u8>>) -> Self {
        Resp {
            status: 200,
            ctype: ctype.to_string(),
            body: body.into(),
            extra: Vec::new(),
            delay: Duration::ZERO,
        }
    }
    fn html(body: impl Into<String>) -> Self {
        Self::ok("text/html; charset=utf-8", body.into().into_bytes())
    }
    fn status(code: u16) -> Self {
        let mut r = Self::ok("text/plain", "refusé");
        r.status = code;
        r
    }
    fn after(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }
}

type Handler = Arc<dyn Fn(&Req) -> Resp + Send + Sync>;

pub struct Site {
    pub ports: Vec<u16>,
    /// Requêtes reçues : `port chemin`.
    pub log: Arc<Mutex<Vec<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Site {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Site {
    /// Ouvre `n` ports ; `make` reçoit les ports et construit le gestionnaire commun.
    pub async fn start(n: usize, make: impl FnOnce(&[u16]) -> Handler) -> Site {
        let mut listeners = Vec::new();
        for _ in 0..n {
            listeners.push(tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind"));
        }
        let ports: Vec<u16> = listeners
            .iter()
            .map(|l| l.local_addr().expect("addr").port())
            .collect();
        let handler = make(&ports);
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (listener, port) in listeners.into_iter().zip(ports.iter().copied()) {
            let handler = handler.clone();
            let log = log.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let Ok((sock, _)) = listener.accept().await else { break };
                    let handler = handler.clone();
                    let log = log.clone();
                    tokio::spawn(serve_one(sock, port, handler, log));
                }
            }));
        }
        Site { ports, log, tasks }
    }

    pub fn requested(&self, fragment: &str) -> bool {
        self.log.lock().map(|l| l.iter().any(|r| r.contains(fragment))).unwrap_or(false)
    }
}

async fn serve_one(mut sock: tokio::net::TcpStream, port: u16, handler: Handler, log: Arc<Mutex<Vec<String>>>) {
    let mut buf = vec![0u8; 8192];
    let mut got = Vec::new();
    loop {
        let Ok(n) = sock.read(&mut buf).await else { return };
        if n == 0 {
            return;
        }
        got.extend_from_slice(&buf[..n]);
        if got.windows(4).any(|w| w == b"\r\n\r\n") || got.len() > 65_536 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&got).to_string();
    let mut lines = head.split("\r\n");
    let path = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    if let Ok(mut l) = log.lock() {
        l.push(format!("{port} {path}"));
    }
    let resp = handler(&Req { path, headers });
    if !resp.delay.is_zero() {
        tokio::time::sleep(resp.delay).await;
    }
    let reason = match resp.status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Status",
    };
    let mut out = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        resp.status,
        resp.ctype,
        resp.body.len()
    );
    for (k, v) in &resp.extra {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&resp.body);
    let _ = sock.write_all(&bytes).await;
    let _ = sock.shutdown().await;
}

// ---------------------------------------------------------------------------
// Contenu commun des pages
// ---------------------------------------------------------------------------

const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nv0.m3u8\n";
const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg0.ts\n#EXT-X-ENDLIST\n";
const MPEGURL: &str = "application/vnd.apple.mpegurl";

/// Routes HLS classiques sous `/hls/`.
fn hls_routes(path: &str) -> Option<Resp> {
    match path {
        "/hls/master.m3u8" => Some(Resp::ok(MPEGURL, MASTER)),
        "/hls/v0.m3u8" => Some(Resp::ok(MPEGURL, MEDIA)),
        "/hls/seg0.ts" => Some(Resp::ok("video/mp2t", vec![0x47u8; 4096])),
        _ => None,
    }
}

/// Façon hls.js : la playlist est demandée par script, puis la variante, puis un segment.
const JS_LIB: &str = r#"
async function loadHls(master) {
  const t = await (await fetch(master)).text();
  const l = t.split('\n').map(x => x.trim()).find(x => x && x[0] !== '#');
  if (!l) return;
  const v = new URL(l, master).href;
  const t2 = await (await fetch(v)).text();
  const sg = t2.split('\n').map(x => x.trim()).find(x => x && x[0] !== '#');
  if (sg) await fetch(new URL(sg, v).href);
}
"#;

fn page(title: &str, body: &str) -> Resp {
    Resp::html(format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{title}</title></head>\
         <body style=\"margin:0;font-family:sans-serif\">{body}</body></html>"
    ))
}

/// Contexte de test : les journaux de la détection vont sur la sortie d'erreur.
fn test_ctx() -> JobCtx {
    JobCtx::new(
        tokio_util::sync::CancellationToken::new(),
        |l, m| eprintln!("   [{l:?}] {m}"),
        |_| {},
    )
}

/// Passe automatique seule (navigateur en arrière-plan) sur une page locale.
async fn detect(url: &str) -> anyhow::Result<Option<ResolvedMedia>> {
    resolve_auto(url, &ResolveHint::default(), &test_ctx(), Limits::default()).await
}

/// Les tests partagent le navigateur : celui qui compte les profils temporaires restants prend
/// le verrou en écriture, les autres le partagent.
static LIVE: tokio::sync::RwLock<()> = tokio::sync::RwLock::const_new(());

/// `None` (test ignoré) si ni Chrome ni Edge n'est installé.
async fn live() -> Option<tokio::sync::RwLockReadGuard<'static, ()>> {
    if super::browser::locate(super::BrowserChoice::Auto).is_none() {
        eprintln!("Ni Chrome ni Edge installé : test ignoré");
        return None;
    }
    Some(LIVE.read().await)
}

fn expect_hls(r: anyhow::Result<Option<ResolvedMedia>>, url: &str) {
    let media = r.expect("résolution").expect("un flux doit être détecté");
    assert_eq!(media.kind, MediaKind::Hls);
    assert_eq!(media.url, url);
}

// ---------------------------------------------------------------------------
// Scénarios
// ---------------------------------------------------------------------------

/// Lecteur dans une iframe d'une autre origine, elle-même contenant une iframe imbriquée.
/// `click` : le lecteur ne charge le flux qu'après un clic de confiance sur sa superposition.
fn iframe_site(click: bool) -> impl FnOnce(&[u16]) -> Handler {
    move |ports: &[u16]| {
        let (p1, p2) = (ports[1], ports[2]);
        Arc::new(move |r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Page hôte",
                    &format!(
                        "<div style=\"height:90px;background:#222;color:#fff\">En-tête du site</div>\
                         <div style=\"margin:20px 60px\"><iframe src=\"http://localhost:{p1}/embed\" \
                         width=560 height=315 frameborder=0 allowfullscreen></iframe></div>"
                    ),
                ),
                "/embed" => page(
                    "Embed",
                    &format!("<iframe src=\"http://127.0.0.1:{p2}/player\" width=560 height=315 style=\"border:0\"></iframe>"),
                ),
                "/player" => {
                    let start = if click {
                        "<div id=ov style=\"position:absolute;left:0;top:0;right:0;bottom:0;background:#000;color:#fff;\
                         display:flex;align-items:center;justify-content:center;font-size:48px;cursor:pointer\">&#9654;</div>\
                         <script>ov.addEventListener('click', e => { if (!e.isTrusted) return; ov.remove(); \
                         loadHls('/hls/master.m3u8'); });</script>"
                    } else {
                        "<script>setTimeout(() => loadHls('/hls/master.m3u8'), 800);</script>"
                    };
                    page(
                        "Lecteur",
                        &format!("<video id=v muted playsinline width=560 height=315 style=\"background:#000\"></video><script>{JS_LIB}</script>{start}"),
                    )
                }
                _ => Resp::status(404),
            }
        })
    }
}

/// Lecteur sans `<video>` : une superposition (hors du centre de la page) charge le flux au clic.
/// `popup` : le premier clic ouvre une fenêtre publicitaire, la lecture ne démarre qu'au second.
fn click_site(popup: bool) -> impl FnOnce(&[u16]) -> Handler {
    move |_ports: &[u16]| {
        Arc::new(move |r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => {
                    let first = if popup {
                        "if (n === 1) { window.open('/ad', '_blank'); return; }"
                    } else {
                        ""
                    };
                    page(
                        "Lecteur à clic",
                        &format!(
                            "<div style=\"height:100px;background:#222;color:#fff\">En-tête</div>\
                             <div id=box style=\"position:absolute;left:60px;top:120px;width:480px;height:270px;background:#111\">\
                             <div id=ov style=\"position:absolute;left:0;top:0;right:0;bottom:0;color:#fff;display:flex;\
                             align-items:center;justify-content:center;font-size:48px;cursor:pointer\">&#9654;</div></div>\
                             <script>{JS_LIB}\nlet n = 0;\
                             ov.addEventListener('click', e => {{ if (!e.isTrusted) return; n++; {first} \
                             ov.remove(); box.innerHTML = '<video autoplay muted></video>'; loadHls('/hls/master.m3u8'); }});</script>"
                        ),
                    )
                }
                "/ad" => page("Publicité", "<h1>Publicité</h1><script>fetch('/ads/pub.m3u8').catch(()=>{})</script>"),
                _ => Resp::status(404),
            }
        })
    }
}

/// Page qui ne charge rien si elle détecte l'automatisation ; le serveur vérifie aussi le user-agent.
fn antibot_site() -> impl FnOnce(&[u16]) -> Handler {
    |_ports: &[u16]| {
        Arc::new(|r: &Req| {
            if r.path == "/hls/master.m3u8" {
                let headless = ["user-agent", "sec-ch-ua"]
                    .iter()
                    .any(|h| r.headers.get(*h).is_some_and(|v| v.contains("Headless")));
                if headless {
                    return Resp::status(403);
                }
            }
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            if r.path.starts_with("/diag") {
                return Resp::ok("text/plain", "ok");
            }
            if r.path == "/" {
                return page(
                    "Page anti-bot",
                    &format!(
                        "<video id=v muted width=480 height=270></video><script>{JS_LIB}\n\
                         const brands = ((navigator.userAgentData && navigator.userAgentData.brands) || []).map(b => b.brand).join(',');\
                         const signs = [];\
                         if (navigator.webdriver === true) signs.push('webdriver');\
                         if (!window.chrome) signs.push('nochrome');\
                         if (/Headless/i.test(navigator.userAgent)) signs.push('ua');\
                         if (/Headless/i.test(brands)) signs.push('brands');\
                         if (!navigator.languages || navigator.languages.length === 0) signs.push('languages');\
                         if (navigator.plugins && navigator.plugins.length === 0) signs.push('plugins');\
                         fetch('/diag?signs=' + encodeURIComponent(signs.join('+') || 'aucun'));\
                         if (signs.length === 0) setTimeout(() => loadHls('/hls/master.m3u8'), 1000);\
                         else document.body.append('Accès refusé : navigateur automatisé');</script>"
                    ),
                );
            }
            Resp::status(404)
        })
    }
}

#[tokio::test]
#[ignore]
async fn navigateur_iframe_autre_origine_imbriquee() {
    let Some(_live) = live().await else { return };
    let site = Site::start(3, iframe_site(false)).await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let want = format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[2]);
    expect_hls(detect(&page_url).await, &want);
}

#[tokio::test]
#[ignore]
async fn navigateur_iframe_autre_origine_clic_dans_le_lecteur() {
    let Some(_live) = live().await else { return };
    let site = Site::start(3, iframe_site(true)).await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let want = format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[2]);
    expect_hls(detect(&page_url).await, &want);
}

#[tokio::test]
#[ignore]
async fn navigateur_flux_apres_un_clic() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, click_site(false)).await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let want = format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]);
    expect_hls(detect(&page_url).await, &want);
}

#[tokio::test]
#[ignore]
async fn navigateur_popup_publicitaire_puis_lecture() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, click_site(true)).await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let want = format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]);
    let r = detect(&page_url).await;
    eprintln!("   popup demandée : {} (requêtes : {:?})", site.requested("/ad"), site.log.lock().map(|l| l.clone()).unwrap_or_default());
    expect_hls(r, &want);
}

#[tokio::test]
#[ignore]
async fn navigateur_page_anti_automatisation() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, antibot_site()).await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let want = format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]);
    let r = detect(&page_url).await;
    let diag: Vec<String> = site
        .log
        .lock()
        .map(|l| l.iter().filter(|x| x.contains("/diag")).cloned().collect())
        .unwrap_or_default();
    eprintln!("   signes d'automatisation vus par la page : {diag:?}");
    expect_hls(r, &want);
}

#[tokio::test]
#[ignore]
async fn navigateur_url_sans_extension_content_type() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| match r.path.as_str() {
            "/" => page(
                "Flux sans extension",
                &format!("<script>{JS_LIB}\nsetTimeout(() => loadHls('/api/stream/8731'), 500);</script>"),
            ),
            "/api/stream/8731" => Resp::ok(MPEGURL, "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\n/api/stream/8731/variant\n"),
            "/api/stream/8731/variant" => Resp::ok(MPEGURL, "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\n/api/seg/1\n#EXT-X-ENDLIST\n"),
            "/api/seg/1" => Resp::ok("video/mp2t", vec![0x47u8; 4096]),
            _ => Resp::status(404),
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    expect_hls(detect(&page_url).await, &format!("http://127.0.0.1:{}/api/stream/8731", site.ports[0]));
}

#[tokio::test]
#[ignore]
async fn navigateur_url_sans_extension_ni_content_type_corps_extm3u() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| match r.path.as_str() {
            "/" => page(
                "Flux sans type",
                &format!("<script>{JS_LIB}\nsetTimeout(() => loadHls('/play?id=3'), 500);</script>"),
            ),
            "/play?id=3" => Resp::ok("text/plain", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\n/play?id=3&v=0\n"),
            "/play?id=3&v=0" => Resp::ok("text/plain", "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\n/api/seg/1\n#EXT-X-ENDLIST\n"),
            "/api/seg/1" => Resp::ok("application/octet-stream", vec![0x47u8; 4096]),
            _ => Resp::status(404),
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    expect_hls(detect(&page_url).await, &format!("http://127.0.0.1:{}/play?id=3", site.ports[0]));
}

#[tokio::test]
#[ignore]
async fn navigateur_flux_demande_par_un_web_worker() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Worker",
                    "<script>setTimeout(() => new Worker('/worker.js'), 500);</script>",
                ),
                "/worker.js" => Resp::ok(
                    "application/javascript",
                    format!("{JS_LIB}\nloadHls(new URL('/hls/master.m3u8', self.location.href).href);"),
                ),
                _ => Resp::status(404),
            }
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    expect_hls(detect(&page_url).await, &format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]));
}

#[tokio::test]
#[ignore]
async fn navigateur_flux_demande_par_un_service_worker() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Service worker",
                    "<script>navigator.serviceWorker.register('/sw.js').then(() => navigator.serviceWorker.ready)\
                     .then(reg => setTimeout(() => reg.active.postMessage('go'), 1500));</script>",
                ),
                "/sw.js" => Resp::ok(
                    "application/javascript",
                    format!(
                        "{JS_LIB}\nself.addEventListener('install', () => self.skipWaiting());\
                         self.addEventListener('activate', e => e.waitUntil(clients.claim()));\
                         self.addEventListener('message', e => e.waitUntil(loadHls('/hls/master.m3u8')));"
                    ),
                ),
                _ => Resp::status(404),
            }
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    expect_hls(detect(&page_url).await, &format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]));
}

#[tokio::test]
#[ignore]
async fn navigateur_page_lente_flux_apres_15_secondes() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Page lente",
                    &format!("<video muted width=480 height=270></video><script>{JS_LIB}\nsetTimeout(() => loadHls('/hls/master.m3u8'), 15000);</script>"),
                )
                .after(Duration::from_secs(2)),
                _ => Resp::status(404),
            }
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    expect_hls(detect(&page_url).await, &format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]));
}

#[tokio::test]
#[ignore]
async fn navigateur_flux_drm_reste_une_erreur() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| match r.path.as_str() {
            "/" => page(
                "Contenu protégé",
                &format!("<video muted width=480 height=270></video><script>{JS_LIB}\nsetTimeout(() => loadHls('/hls/master.m3u8'), 500);</script>"),
            ),
            "/hls/master.m3u8" => Resp::ok(
                MPEGURL,
                "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=800000\nv0.m3u8\n",
            ),
            "/hls/v0.m3u8" => Resp::ok(MPEGURL, MEDIA),
            "/hls/seg0.ts" => Resp::ok("video/mp2t", vec![0x47u8; 4096]),
            _ => Resp::status(404),
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let err = detect(&page_url).await.expect_err("le DRM doit rester une erreur");
    assert!(err.to_string().contains("DRM"), "{err}");
}

#[tokio::test]
#[ignore]
async fn navigateur_reel_detecte_le_m3u8_local() {
    // Verrou exclusif : aucun autre navigateur de test ne tourne pendant le comptage des profils.
    if super::browser::locate(super::BrowserChoice::Auto).is_none() {
        eprintln!("Ni Chrome ni Edge installé : test ignoré");
        return;
    }
    let _exclusive = LIVE.write().await;
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| {
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Page de test Lucamedia",
                    &format!(
                        "<video id=v src=\"/hls/master.m3u8\" muted autoplay controls width=640 height=360></video>\
                         <script>{JS_LIB}\nsetTimeout(() => loadHls('/hls/master.m3u8'), 500);</script>"
                    ),
                ),
                _ => Resp::status(404),
            }
        }) as Handler
    })
    .await;
    let page_url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let before: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    let media = detect(&page_url).await.expect("résolution").expect("un flux doit être détecté");
    assert_eq!(media.kind, MediaKind::Hls);
    assert_eq!(media.url, format!("http://127.0.0.1:{}/hls/master.m3u8", site.ports[0]));
    assert_eq!(media.via, super::ResolvedVia::Browser);
    assert_eq!(media.title.as_deref(), Some("Page de test Lucamedia"));
    assert_eq!(media.http.referer.as_deref(), Some(page_url.as_str()));
    assert_eq!(media.http.user_agent, BROWSER_UA);

    // Le profil temporaire a bien été supprimé.
    let leftovers: Vec<_> = std::fs::read_dir(std::env::temp_dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name())
                .filter(|n| n.to_string_lossy().starts_with("lucamedia-browser-") && !before.contains(n))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "profil temporaire non supprimé : {leftovers:?}");
}

// ---------------------------------------------------------------------------
// Mode assisté : navigateur visible, l'utilisateur lance lui-même la lecture
// ---------------------------------------------------------------------------

/// Page à bouton de lecture qui exige un geste que l'automatisation ne sait pas fournir : le
/// serveur ne remet la playlist qu'après un clic de confiance accompagné d'un « captcha résolu »
/// (`human`), ce que seul l'utilisateur (le test, dans la fenêtre visible) fait.
fn gate_site(human: Arc<AtomicBool>) -> impl FnOnce(&[u16]) -> Handler {
    move |_ports: &[u16]| {
        Arc::new(move |r: &Req| {
            if r.path == "/gate" {
                return if human.load(Ordering::SeqCst) { Resp::ok("text/plain", "ok") } else { Resp::status(403) };
            }
            if let Some(h) = hls_routes(&r.path) {
                return h;
            }
            match r.path.as_str() {
                "/" => page(
                    "Lecteur protégé par captcha",
                    &format!(
                        "<div style=\"height:100px;background:#222;color:#fff\">Je ne suis pas un robot</div>\
                         <div id=box style=\"position:absolute;left:60px;top:120px;width:480px;height:270px;background:#111\">\
                         <div id=ov style=\"position:absolute;left:0;top:0;right:0;bottom:0;color:#fff;display:flex;\
                         align-items:center;justify-content:center;font-size:48px;cursor:pointer\">&#9654;</div></div>\
                         <script>{JS_LIB}\n\
                         ov.addEventListener('click', async e => {{ if (!e.isTrusted) return; \
                         const g = await fetch('/gate'); if (!g.ok) return; ov.remove(); loadHls('/hls/master.m3u8'); }});</script>"
                    ),
                ),
                _ => Resp::status(404),
            }
        }) as Handler
    }
}

/// Page qui ne charge jamais rien.
fn empty_site() -> impl FnOnce(&[u16]) -> Handler {
    |_ports: &[u16]| {
        Arc::new(|r: &Req| match r.path.as_str() {
            "/" => page("Page vide", "<h1>Aucune vidéo ici</h1>"),
            _ => Resp::status(404),
        }) as Handler
    }
}

/// Retrouve, parmi les navigateurs lancés par les tests, celui dont un onglet affiche cette page.
async fn find_page(port: u16) -> Option<(Cdp, String)> {
    let needle = format!("127.0.0.1:{port}");
    for entry in std::fs::read_dir(std::env::temp_dir()).ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("lucamedia-browser-") {
            continue;
        }
        let Some(ws) = read_devtools_port(&entry.path()) else { continue };
        let Ok((cdp, _events)) = Cdp::connect(&ws).await else { continue };
        let Ok(t) = cdp.call("Target.getTargets", json!({}), None).await else { continue };
        let id = t["targetInfos"].as_array().and_then(|a| {
            a.iter()
                .find(|i| i["type"] == "page" && i["url"].as_str().is_some_and(|u| u.contains(&needle)))
                .and_then(|i| i["targetId"].as_str().map(str::to_string))
        });
        if let Some(id) = id {
            return Some((cdp, id));
        }
    }
    None
}

/// Ce que fait l'utilisateur dans la fenêtre visible.
#[derive(Clone, Copy)]
enum Human {
    /// Résout le « captcha » puis clique sur le bouton de lecture.
    Plays,
    /// Ferme la fenêtre.
    ClosesWindow,
    /// Annule le téléchargement.
    Cancels,
    /// Ne fait rien.
    Idles,
}

/// Lance l'utilisateur simulé : il attend l'ouverture de la fenêtre visible, patiente `pause`, puis agit.
fn spawn_human(
    what: Human,
    port: u16,
    pause: Duration,
    opened: Arc<AtomicBool>,
    human: Arc<AtomicBool>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !opened.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(pause).await;
        match what {
            Human::Idles => {}
            Human::Cancels => cancel.cancel(),
            Human::Plays | Human::ClosesWindow => {
                let mut found = None;
                for _ in 0..40 {
                    if let Some(f) = find_page(port).await {
                        found = Some(f);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                let Some((cdp, target)) = found else {
                    eprintln!("   (utilisateur simulé : fenêtre introuvable)");
                    return;
                };
                if matches!(what, Human::ClosesWindow) {
                    let _ = cdp.call("Browser.close", json!({}), None).await;
                    return;
                }
                human.store(true, Ordering::SeqCst);
                let Ok(att) = cdp.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None).await
                else {
                    return;
                };
                let Some(sid) = att["sessionId"].as_str() else { return };
                let _ = cdp.call("Page.bringToFront", json!({}), Some(sid)).await;
                // Clic de confiance au centre du bouton de lecture (zone 60,120 – 540,390).
                for (kind, buttons, count) in [("mouseMoved", 0, 0), ("mousePressed", 1, 1), ("mouseReleased", 0, 1)] {
                    let _ = cdp
                        .call(
                            "Input.dispatchMouseEvent",
                            json!({"type": kind, "x": 300, "y": 255, "button": if count == 0 { "none" } else { "left" },
                                   "buttons": buttons, "clickCount": count}),
                            Some(sid),
                        )
                        .await;
                    tokio::time::sleep(Duration::from_millis(60)).await;
                }
            }
        }
    })
}

struct Assisted {
    result: anyhow::Result<ResolvedMedia>,
    /// Appels du rappel `assist` dans l'ordre.
    assist_calls: Vec<bool>,
    site: Site,
}

type SiteFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Site> + Send>>;

fn gate(human: Arc<AtomicBool>) -> SiteFuture {
    Box::pin(Site::start(1, gate_site(human)))
}

fn empty(_: Arc<AtomicBool>) -> SiteFuture {
    Box::pin(Site::start(1, empty_site()))
}

async fn assisted_run(
    what: Human,
    show_browser: bool,
    site_of: impl FnOnce(Arc<AtomicBool>) -> SiteFuture,
    limits: Limits,
    pause: Duration,
) -> Assisted {
    let human = Arc::new(AtomicBool::new(false));
    let site = site_of(human.clone()).await;
    let port = site.ports[0];
    let opened = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cancel = tokio_util::sync::CancellationToken::new();
    let ctx = JobCtx::new(cancel.clone(), |l, m| eprintln!("   [{l:?}] {m}"), |_| {}).with_assist({
        let (opened, calls) = (opened.clone(), calls.clone());
        move |a| {
            if let Ok(mut c) = calls.lock() {
                c.push(a);
            }
            if a {
                opened.store(true, Ordering::SeqCst);
            }
        }
    });
    let user = spawn_human(what, port, pause, opened, human, cancel);
    let hint = ResolveHint {
        show_browser,
        ..ResolveHint::default()
    };
    let result = resolve_with(&format!("http://127.0.0.1:{port}/"), &hint, &ctx, limits).await;
    user.abort();
    let assist_calls = calls.lock().map(|c| c.clone()).unwrap_or_default();
    Assisted {
        result,
        assist_calls,
        site,
    }
}

const SHORT_AUTO: Limits = Limits {
    auto: Duration::from_secs(8),
    assist: Duration::from_secs(60),
};

#[tokio::test]
#[ignore]
async fn navigateur_mode_assiste_apres_echec_automatique() {
    let Some(_live) = live().await else { return };
    let r = assisted_run(Human::Plays, false, gate, SHORT_AUTO, Duration::from_secs(4)).await;
    let port = r.site.ports[0];
    let media = r.result.expect("le clic de l'utilisateur doit révéler le flux");
    assert_eq!(media.kind, MediaKind::Hls);
    assert_eq!(media.url, format!("http://127.0.0.1:{port}/hls/master.m3u8"));
    // Le mode assisté n'a été ouvert qu'une fois la passe automatique échouée, et refermé.
    assert_eq!(r.assist_calls, vec![true, false]);
    assert!(r.site.requested("/gate"), "la passe automatique aurait dû essayer de cliquer");
}

#[tokio::test]
#[ignore]
async fn navigateur_mode_assiste_direct_si_navigateur_visible_demande() {
    let Some(_live) = live().await else { return };
    let r = assisted_run(Human::Plays, true, gate, SHORT_AUTO, Duration::from_secs(3)).await;
    let port = r.site.ports[0];
    let media = r.result.expect("flux");
    assert_eq!(media.url, format!("http://127.0.0.1:{port}/hls/master.m3u8"));
    assert_eq!(r.assist_calls, vec![true, false]);
}

#[tokio::test]
#[ignore]
async fn navigateur_mode_assiste_fenetre_fermee_par_l_utilisateur() {
    let Some(_live) = live().await else { return };
    let r = assisted_run(Human::ClosesWindow, false, empty, SHORT_AUTO, Duration::from_secs(3)).await;
    let err = r.result.expect_err("fenêtre fermée : erreur claire");
    assert!(err.to_string().contains("fenêtre du navigateur a été fermée"), "{err}");
    assert_eq!(r.assist_calls, vec![true, false]);
}

#[tokio::test]
#[ignore]
async fn navigateur_mode_assiste_delai_expire() {
    let Some(_live) = live().await else { return };
    let limits = Limits {
        auto: Duration::from_secs(6),
        assist: Duration::from_secs(10),
    };
    let r = assisted_run(Human::Idles, false, empty, limits, Duration::ZERO).await;
    let err = r.result.expect_err("délai expiré : erreur claire");
    assert!(err.to_string().contains("délai de 3 minutes"), "{err}");
    assert_eq!(r.assist_calls, vec![true, false]);
}

#[tokio::test]
#[ignore]
async fn navigateur_mode_assiste_annule_par_l_utilisateur() {
    let Some(_live) = live().await else { return };
    let r = assisted_run(Human::Cancels, false, empty, SHORT_AUTO, Duration::from_secs(2)).await;
    let err = r.result.expect_err("annulation");
    assert_eq!(err.to_string(), "Annulé");
    assert_eq!(r.assist_calls, vec![true, false]);
}

#[tokio::test]
#[ignore]
async fn navigateur_drm_ne_passe_pas_en_mode_assiste() {
    let Some(_live) = live().await else { return };
    let site = Site::start(1, |_p: &[u16]| {
        Arc::new(|r: &Req| match r.path.as_str() {
            "/" => page(
                "Contenu protégé",
                &format!("<video muted width=480 height=270></video><script>{JS_LIB}\nsetTimeout(() => loadHls('/hls/master.m3u8'), 500);</script>"),
            ),
            "/hls/master.m3u8" => Resp::ok(
                MPEGURL,
                "#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=800000\nv0.m3u8\n",
            ),
            _ => Resp::status(404),
        }) as Handler
    })
    .await;
    let calls = Arc::new(Mutex::new(Vec::new()));
    let c2 = calls.clone();
    let ctx = test_ctx().with_assist(move |a| {
        if let Ok(mut c) = c2.lock() {
            c.push(a);
        }
    });
    let url = format!("http://127.0.0.1:{}/", site.ports[0]);
    let err = resolve_with(&url, &ResolveHint::default(), &ctx, SHORT_AUTO)
        .await
        .expect_err("DRM : erreur");
    assert!(err.to_string().contains("DRM"), "{err}");
    assert!(calls.lock().map(|c| c.is_empty()).unwrap_or(false), "aucune fenêtre visible pour du DRM");
}
