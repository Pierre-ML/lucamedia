//! Étape 2 : extraction via yt-dlp (`-J`), sans rien télécharger.

use super::{MediaKind, ResolveHint, ResolvedMedia, ResolvedVia};
use crate::model::{HttpCtx, JobCtx, BROWSER_UA};
use crate::tools::{command, Tool};
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;

/// Délai maximal accordé à yt-dlp.
const TIMEOUT: Duration = Duration::from_secs(45);

/// Lance `yt-dlp -J` ; `Err` si yt-dlp est absent, échoue ou ne trouve pas de vidéo.
pub async fn resolve(url: &str, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<ResolvedMedia> {
    let mut cmd = command(Tool::YtDlp)?;
    cmd.args([
        "-J",
        "--no-playlist",
        "--no-warnings",
        "--no-check-certificates",
        "--encoding",
        "utf-8",
        "--user-agent",
        BROWSER_UA,
    ]);
    if let Some(r) = hint.referer.as_deref().filter(|r| !r.is_empty()) {
        cmd.args(["--referer", r]);
    }
    cmd.arg(url)
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Impossible de lancer yt-dlp : {e}"))?;
    // Annulation ou délai : `child` est abandonné, kill_on_drop le tue.
    let output = tokio::select! {
        _ = ctx.cancel.cancelled() => anyhow::bail!("Annulé"),
        r = tokio::time::timeout(TIMEOUT, child.wait_with_output()) => match r {
            Ok(r) => r?,
            Err(_) => anyhow::bail!("yt-dlp a dépassé le délai de {} s", TIMEOUT.as_secs()),
        },
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("échec sans message");
        anyhow::bail!("{line}");
    }
    let json: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| anyhow::anyhow!("Réponse yt-dlp illisible : {e}"))?;
    parse_ytdlp_json(&json, url, hint.referer.as_deref())
        .ok_or_else(|| anyhow::anyhow!("yt-dlp n'a renvoyé aucun format vidéo"))
}

fn header_ci<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| v.as_str())
        .filter(|s| !s.is_empty())
}

fn is_hls_protocol(f: &Value) -> bool {
    matches!(
        f.get("protocol").and_then(Value::as_str),
        Some("m3u8") | Some("m3u8_native")
    )
}

fn has_video(f: &Value) -> bool {
    // Absent ou null : on ne sait pas, on garde ; « none » : piste audio seule.
    f.get("vcodec").and_then(Value::as_str) != Some("none")
}

/// Meilleur format : plus grande hauteur, puis meilleur débit.
fn best_format(formats: &[Value]) -> Option<&Value> {
    let key = |f: &Value| {
        (
            f.get("height").and_then(Value::as_u64).unwrap_or(0),
            // Le débit est un flottant : comparé en millièmes de kbit/s.
            (f.get("tbr").and_then(Value::as_f64).unwrap_or(0.0) * 1000.0) as u64,
        )
    };
    formats
        .iter()
        .filter(|f| has_video(f))
        .max_by_key(|f| key(f))
        .or_else(|| formats.iter().max_by_key(|f| key(f)))
}

/// Transforme le JSON de `yt-dlp -J` en média à télécharger.
pub fn parse_ytdlp_json(
    json: &Value,
    page_url: &str,
    hint_referer: Option<&str>,
) -> Option<ResolvedMedia> {
    // Playlist (ne devrait pas arriver avec --no-playlist) : première entrée utilisable.
    let info = if json.get("_type").and_then(Value::as_str) == Some("playlist") {
        json.get("entries")?.as_array()?.iter().find(|e| e.is_object())?
    } else {
        json
    };

    let title = info
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let duration = info
        .get("duration")
        .and_then(Value::as_f64)
        .filter(|d| *d > 0.0);

    // Pas de liste de formats : l'objet lui-même décrit l'unique format.
    let formats: Vec<Value> = match info.get("formats").and_then(Value::as_array) {
        Some(f) if !f.is_empty() => f.clone(),
        _ => {
            if info.get("url").and_then(Value::as_str).is_some() {
                vec![info.clone()]
            } else {
                return None;
            }
        }
    };
    let best = best_format(&formats)?;
    let height = best
        .get("height")
        .and_then(Value::as_u64)
        .or_else(|| info.get("height").and_then(Value::as_u64))
        .map(|h| h as u32)
        .filter(|h| *h > 0);

    let manifest = best
        .get("manifest_url")
        .and_then(Value::as_str)
        .or_else(|| info.get("manifest_url").and_then(Value::as_str))
        .filter(|s| !s.is_empty());

    if is_hls_protocol(best) {
        if let Some(manifest) = manifest {
            let mut http = HttpCtx::for_url(manifest, hint_referer);
            let headers = best
                .get("http_headers")
                .filter(|h| h.is_object())
                .or_else(|| info.get("http_headers"))
                .unwrap_or(&Value::Null);
            if let Some(v) = header_ci(headers, "Referer") {
                http.referer = Some(v.to_string());
            }
            if let Some(v) = header_ci(headers, "Origin") {
                http.origin = Some(v.to_string());
            }
            if let Some(v) = header_ci(headers, "Cookie") {
                http.cookies = Some(v.to_string());
            }
            if let Some(v) = header_ci(headers, "User-Agent") {
                http.user_agent = v.to_string();
            }
            return Some(ResolvedMedia {
                kind: MediaKind::Hls,
                url: manifest.to_string(),
                http,
                via: ResolvedVia::Ytdlp,
                title,
                duration,
                height,
            });
        }
    }

    Some(ResolvedMedia {
        kind: MediaKind::Ytdlp,
        url: page_url.to_string(),
        http: HttpCtx::for_url(page_url, hint_referer),
        via: ResolvedVia::Ytdlp,
        title,
        duration,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hls_avec_manifest() {
        let j = json!({
            "title": "Mon film",
            "duration": 3600.5,
            "formats": [
                {"format_id":"hls-360","protocol":"m3u8_native","height":360,"tbr":600.0,"vcodec":"avc1","manifest_url":"https://cdn.ex/master.m3u8","url":"https://cdn.ex/360.m3u8"},
                {"format_id":"hls-1080","protocol":"m3u8_native","height":1080,"tbr":4500.0,"vcodec":"avc1","manifest_url":"https://cdn.ex/master.m3u8","url":"https://cdn.ex/1080.m3u8",
                 "http_headers":{"User-Agent":"UA-test","Referer":"https://site.ex/film","Origin":"https://site.ex","Cookie":"a=b"}},
                {"format_id":"audio","protocol":"m3u8_native","vcodec":"none","tbr":128.0,"manifest_url":"https://cdn.ex/master.m3u8","url":"https://cdn.ex/a.m3u8"}
            ]
        });
        let m = parse_ytdlp_json(&j, "https://site.ex/film", None).expect("média");
        assert_eq!(m.kind, MediaKind::Hls);
        assert_eq!(m.url, "https://cdn.ex/master.m3u8");
        assert_eq!(m.via, ResolvedVia::Ytdlp);
        assert_eq!(m.title.as_deref(), Some("Mon film"));
        assert_eq!(m.duration, Some(3600.5));
        assert_eq!(m.height, Some(1080));
        assert_eq!(m.http.referer.as_deref(), Some("https://site.ex/film"));
        assert_eq!(m.http.origin.as_deref(), Some("https://site.ex"));
        assert_eq!(m.http.cookies.as_deref(), Some("a=b"));
        assert_eq!(m.http.user_agent, "UA-test");
    }

    #[test]
    fn youtube_like_dash_donne_ytdlp() {
        let j = json!({
            "title": "Clip",
            "duration": 212,
            "formats": [
                {"format_id":"18","protocol":"https","height":360,"tbr":500.0,"vcodec":"avc1","url":"https://rr.ex/18"},
                {"format_id":"299","protocol":"https","height":1080,"tbr":3000.0,"vcodec":"avc1","url":"https://rr.ex/299"},
                {"format_id":"137","protocol":"http_dash_segments","height":1080,"tbr":4400.0,"vcodec":"avc1","url":"https://rr.ex/137","manifest_url":"https://rr.ex/dash.mpd"},
                {"format_id":"95","protocol":"m3u8_native","height":720,"tbr":1500.0,"vcodec":"avc1","manifest_url":"https://rr.ex/master.m3u8","url":"https://rr.ex/95.m3u8"},
                {"format_id":"140","protocol":"https","vcodec":"none","tbr":130.0,"url":"https://rr.ex/140"}
            ]
        });
        let m = parse_ytdlp_json(&j, "https://www.youtube.com/watch?v=abc", Some("https://ref.ex/"))
            .expect("média");
        assert_eq!(m.kind, MediaKind::Ytdlp);
        assert_eq!(m.url, "https://www.youtube.com/watch?v=abc");
        assert_eq!(m.height, Some(1080));
        assert_eq!(m.duration, Some(212.0));
        assert_eq!(m.http.referer.as_deref(), Some("https://ref.ex/"));
    }

    #[test]
    fn hls_sans_manifest_donne_ytdlp() {
        let j = json!({"formats":[{"protocol":"m3u8","height":720,"url":"https://cdn.ex/720.m3u8"}]});
        let m = parse_ytdlp_json(&j, "https://site.ex/p", None).expect("média");
        assert_eq!(m.kind, MediaKind::Ytdlp);
    }

    #[test]
    fn format_unique_sans_liste() {
        let j = json!({"title":"Fichier","url":"https://cdn.ex/v.mp4","protocol":"https","height":480});
        let m = parse_ytdlp_json(&j, "https://site.ex/p", None).expect("média");
        assert_eq!(m.kind, MediaKind::Ytdlp);
        assert_eq!(m.height, Some(480));
    }

    #[test]
    fn playlist_premiere_entree_et_vide() {
        let j = json!({"_type":"playlist","entries":[{"title":"E1","formats":[
            {"protocol":"m3u8_native","height":720,"manifest_url":"https://cdn.ex/m.m3u8","url":"https://cdn.ex/720.m3u8"}]}]});
        let m = parse_ytdlp_json(&j, "https://site.ex/p", None).expect("média");
        assert_eq!(m.kind, MediaKind::Hls);
        assert_eq!(m.title.as_deref(), Some("E1"));
        assert!(parse_ytdlp_json(&json!({"title":"rien"}), "https://site.ex/p", None).is_none());
    }
}
