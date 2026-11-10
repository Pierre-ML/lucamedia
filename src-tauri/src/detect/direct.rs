//! Étape 1 : lien direct vers un flux (extension du chemin, sinon sondage des premiers octets).

use super::{MediaKind, ResolveHint, ResolvedMedia, ResolvedVia};
use crate::model::{HttpCtx, JobCtx};
use std::time::Duration;

/// Nombre d'octets lus pour reconnaître le contenu.
const SNIFF_BYTES: usize = 2048;

/// Classe un lien d'après l'extension de son chemin (requête et fragment ignorés).
pub fn classify_by_extension(url: &str) -> Option<MediaKind> {
    let path = match reqwest::Url::parse(url) {
        Ok(u) => u.path().to_ascii_lowercase(),
        Err(_) => url
            .split(['?', '#'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase(),
    };
    let ext = path.rsplit('/').next()?.rsplit_once('.')?.1;
    match ext {
        "m3u8" => Some(MediaKind::Hls),
        "mpd" => Some(MediaKind::Dash),
        "mp4" | "mkv" | "webm" | "mov" | "ts" | "m4v" => Some(MediaKind::File),
        _ => None,
    }
}

/// Classe une réponse HTTP d'après son Content-Type puis le début du corps.
pub fn classify_response(content_type: Option<&str>, body: &[u8]) -> Option<MediaKind> {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("mpegurl") {
        return Some(MediaKind::Hls);
    }
    if ct.contains("dash+xml") {
        return Some(MediaKind::Dash);
    }
    // Corps : un éventuel BOM UTF-8 et les espaces de tête sont ignorés.
    let text = String::from_utf8_lossy(&body[..body.len().min(SNIFF_BYTES)]);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("#EXTM3U") {
        return Some(MediaKind::Hls);
    }
    if ct.starts_with("video/") {
        return Some(MediaKind::File);
    }
    None
}

fn media(kind: MediaKind, url: &str, hint: &ResolveHint) -> ResolvedMedia {
    ResolvedMedia {
        kind,
        url: url.to_string(),
        http: HttpCtx::for_url(url, hint.referer.as_deref()),
        via: ResolvedVia::Direct,
        title: None,
        duration: None,
        height: None,
    }
}

/// Lit au plus `SNIFF_BYTES` octets de la réponse.
async fn sniff(url: &str, http: &HttpCtx) -> anyhow::Result<(Option<String>, Vec<u8>, u16)> {
    let mut resp = http.apply(HttpCtx::client().get(url)).send().await?;
    let status = resp.status().as_u16();
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let mut body: Vec<u8> = Vec::new();
    while body.len() < SNIFF_BYTES {
        match resp.chunk().await? {
            Some(c) => body.extend_from_slice(&c),
            None => break,
        }
    }
    // `resp` est abandonnée ici : le reste du corps n'est jamais téléchargé.
    Ok((ct, body, status))
}

/// `Ok(None)` : ce n'est pas (ou on ne sait pas reconnaître) un lien direct vers un flux.
pub async fn resolve(url: &str, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<Option<ResolvedMedia>> {
    if let Some(kind) = classify_by_extension(url) {
        ctx.info(format!("Extension reconnue : {kind:?}"));
        return Ok(Some(media(kind, url, hint)));
    }

    let http = HttpCtx::for_url(url, hint.referer.as_deref());
    ctx.info("Sondage des premiers octets du lien…");
    let fetch = sniff(url, &http);
    let result = tokio::select! {
        _ = ctx.cancel.cancelled() => anyhow::bail!("Annulé"),
        r = tokio::time::timeout(Duration::from_secs(20), fetch) => r,
    };
    let (ct, body, status) = match result {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("délai dépassé"),
    };
    if status >= 400 {
        ctx.info(format!("Le lien répond HTTP {status}."));
        return Ok(None);
    }
    match classify_response(ct.as_deref(), &body) {
        Some(kind) => {
            ctx.info(format!(
                "Contenu reconnu ({}) : {kind:?}",
                ct.as_deref().unwrap_or("type inconnu")
            ));
            Ok(Some(media(kind, url, hint)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_hls_dash_fichier() {
        assert_eq!(classify_by_extension("https://a.com/x/master.m3u8"), Some(MediaKind::Hls));
        assert_eq!(classify_by_extension("https://a.com/x/master.M3U8?token=abc&x=1"), Some(MediaKind::Hls));
        assert_eq!(classify_by_extension("https://a.com/live.mpd#frag"), Some(MediaKind::Dash));
        for e in ["mp4", "mkv", "webm", "mov", "ts", "m4v"] {
            assert_eq!(
                classify_by_extension(&format!("https://a.com/v/film.{e}?dl=1")),
                Some(MediaKind::File),
                "{e}"
            );
        }
    }

    #[test]
    fn extension_inconnue() {
        assert_eq!(classify_by_extension("https://a.com/watch?v=abc.mp4"), None);
        assert_eq!(classify_by_extension("https://a.com/page.html"), None);
        assert_eq!(classify_by_extension("https://a.com/"), None);
        assert_eq!(classify_by_extension("https://a.com/dossier.mp4/page"), None);
    }

    #[test]
    fn reponse_par_content_type() {
        assert_eq!(classify_response(Some("application/vnd.apple.mpegurl"), b""), Some(MediaKind::Hls));
        assert_eq!(classify_response(Some("audio/x-mpegURL; charset=utf-8"), b"<html>"), Some(MediaKind::Hls));
        assert_eq!(classify_response(Some("application/dash+xml"), b"<MPD>"), Some(MediaKind::Dash));
        assert_eq!(classify_response(Some("video/mp4"), b"\x00\x00"), Some(MediaKind::File));
        assert_eq!(classify_response(Some("text/html"), b"<html></html>"), None);
        assert_eq!(classify_response(None, b""), None);
    }

    #[test]
    fn reponse_par_corps() {
        assert_eq!(classify_response(Some("text/plain"), b"#EXTM3U\n#EXT-X-VERSION:3"), Some(MediaKind::Hls));
        assert_eq!(classify_response(Some("application/octet-stream"), b"\xef\xbb\xbf  \n#EXTM3U\n"), Some(MediaKind::Hls));
        assert_eq!(classify_response(Some("text/html"), b"<html>#EXTM3U"), None);
    }
}
