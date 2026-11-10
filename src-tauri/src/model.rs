//! Types partagés entre les modules : contexte HTTP, progression, contexte d'un téléchargement.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Certains CDN renvoient 403 quand la requête ne ressemble pas à celle d'un navigateur.
pub const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// En-têtes à envoyer pour télécharger un média.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HttpCtx {
    pub referer: Option<String>,
    pub origin: Option<String>,
    /// Vide => `BROWSER_UA`.
    pub user_agent: String,
    /// Valeur de l'en-tête `Cookie`.
    pub cookies: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

/// Origine (`schéma://hôte[:port]`) d'une URL, si elle en a une.
fn origin_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let origin = parsed.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

impl HttpCtx {
    /// Referer donné, sinon l'origine de l'URL suivie de `/` ; l'origin en est déduit.
    pub fn for_url(url: &str, referer: Option<&str>) -> Self {
        let referer = referer
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string)
            .or_else(|| origin_of(url).map(|o| format!("{o}/")));
        let origin = referer.as_deref().and_then(origin_of);
        Self {
            referer,
            origin,
            user_agent: BROWSER_UA.to_string(),
            cookies: None,
            extra_headers: Vec::new(),
        }
    }

    /// Ajoute les en-têtes à une requête.
    pub fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        use reqwest::header::{COOKIE, ORIGIN, REFERER, USER_AGENT};
        let ua = if self.user_agent.is_empty() {
            BROWSER_UA
        } else {
            self.user_agent.as_str()
        };
        let mut rb = rb.header(USER_AGENT, ua);
        if let Some(r) = self.referer.as_deref().filter(|r| !r.is_empty()) {
            rb = rb.header(REFERER, r);
        }
        if let Some(o) = self.origin.as_deref().filter(|o| !o.is_empty()) {
            rb = rb.header(ORIGIN, o);
        }
        if let Some(c) = self.cookies.as_deref().filter(|c| !c.is_empty()) {
            rb = rb.header(COOKIE, c);
        }
        for (k, v) in &self.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        rb
    }

    /// Client HTTP partagé : certificats invalides acceptés (hébergeurs de streaming),
    /// 10 redirections, connexion 15 s, lecture 30 s.
    pub fn client() -> reqwest::Client {
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        CLIENT
            .get_or_init(|| {
                reqwest::Client::builder()
                    .danger_accept_invalid_certs(true)
                    .redirect(reqwest::redirect::Policy::limited(10))
                    .connect_timeout(Duration::from_secs(15))
                    .read_timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_default()
            })
            .clone()
    }
}

/// Étape courante d'un téléchargement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Resolving,
    Downloading,
    Verifying,
    Repairing,
    Assembling,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Progression d'un téléchargement.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub phase: Option<Phase>,
    /// 0..100, dans la phase courante.
    pub percent: f32,
    pub downloaded_bytes: u64,
    /// Estimation.
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
    pub segments_done: Option<u32>,
    pub segments_total: Option<u32>,
}

type LogFn = Arc<dyn Fn(LogLevel, String) + Send + Sync>;
type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// Contexte passé à chaque étape d'un téléchargement. Clonable, `Send + Sync`.
#[derive(Clone)]
pub struct JobCtx {
    pub cancel: CancellationToken,
    log: LogFn,
    progress: ProgressFn,
}

impl JobCtx {
    pub fn new(
        cancel: CancellationToken,
        log: impl Fn(LogLevel, String) + Send + Sync + 'static,
        progress: impl Fn(Progress) + Send + Sync + 'static,
    ) -> Self {
        Self {
            cancel,
            log: Arc::new(log),
            progress: Arc::new(progress),
        }
    }

    /// Contexte muet, pour les tests.
    pub fn noop() -> Self {
        Self::new(CancellationToken::new(), |_, _| {}, |_| {})
    }

    pub fn info(&self, m: impl Into<String>) {
        (self.log)(LogLevel::Info, m.into());
    }

    pub fn warn(&self, m: impl Into<String>) {
        (self.log)(LogLevel::Warn, m.into());
    }

    pub fn error(&self, m: impl Into<String>) {
        (self.log)(LogLevel::Error, m.into());
    }

    pub fn progress(&self, p: Progress) {
        (self.progress)(p);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// `Err("Annulé")` si le téléchargement a été annulé.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            anyhow::bail!("Annulé");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn for_url_derives_referer_and_origin() {
        let h = HttpCtx::for_url("https://cdn.example.com:8443/a/b.m3u8?x=1", None);
        assert_eq!(h.referer.as_deref(), Some("https://cdn.example.com:8443/"));
        assert_eq!(h.origin.as_deref(), Some("https://cdn.example.com:8443"));
        assert_eq!(h.user_agent, BROWSER_UA);
    }

    #[test]
    fn for_url_uses_given_referer() {
        let h = HttpCtx::for_url("https://cdn.example.com/a.m3u8", Some("https://site.tv/page/1"));
        assert_eq!(h.referer.as_deref(), Some("https://site.tv/page/1"));
        assert_eq!(h.origin.as_deref(), Some("https://site.tv"));
        let h = HttpCtx::for_url("https://cdn.example.com/a.m3u8", Some("  "));
        assert_eq!(h.referer.as_deref(), Some("https://cdn.example.com/"));
    }

    #[test]
    fn apply_sets_headers() {
        let mut h = HttpCtx::for_url("https://a.example/x", None);
        h.cookies = Some("k=v".into());
        h.extra_headers.push(("X-Test".into(), "1".into()));
        let req = h
            .apply(reqwest::Client::new().get("https://a.example/x"))
            .build()
            .expect("requête valide");
        let headers = req.headers();
        assert_eq!(headers["user-agent"], BROWSER_UA);
        assert_eq!(headers["referer"], "https://a.example/");
        assert_eq!(headers["origin"], "https://a.example");
        assert_eq!(headers["cookie"], "k=v");
        assert_eq!(headers["x-test"], "1");
    }

    #[test]
    fn empty_user_agent_falls_back() {
        let h = HttpCtx::default();
        let req = h
            .apply(reqwest::Client::new().get("https://a.example/"))
            .build()
            .expect("requête valide");
        assert_eq!(req.headers()["user-agent"], BROWSER_UA);
    }

    #[test]
    fn ctx_check_and_callbacks() {
        let ctx = JobCtx::noop();
        assert!(ctx.check().is_ok());
        ctx.cancel.cancel();
        assert!(ctx.is_cancelled());
        assert_eq!(ctx.check().unwrap_err().to_string(), "Annulé");

        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        let ctx = JobCtx::new(
            CancellationToken::new(),
            move |_, _| {
                n2.fetch_add(1, Ordering::SeqCst);
            },
            |_| {},
        );
        ctx.info("a");
        ctx.warn("b");
        ctx.error("c");
        assert_eq!(n.load(Ordering::SeqCst), 3);
    }
}
