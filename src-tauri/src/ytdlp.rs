//! Téléchargement via le sidecar yt-dlp (DASH, pages de lecteurs, sites supportés).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;

use anyhow::{bail, Context};
use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

use crate::detect::ResolvedMedia;
use crate::model::{JobCtx, Phase, Progress, BROWSER_UA};
use crate::tools::{self, Tool};

/// Progression lue sur une ligne `[download]` de yt-dlp.
#[derive(Debug, Clone, PartialEq)]
pub struct YtProgress {
    pub percent: f32,
    pub total_bytes: u64,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^\[download\]\s+(\d+(?:\.\d+)?)%\s+of\s+~?\s*([\d.]+)\s*(B|KiB|MiB|GiB|TiB)\b(?:.*?\bat\s+~?\s*([\d.]+)\s*(B|KiB|MiB|GiB|TiB)/s)?(?:.*?ETA\s+([\d:]+))?",
        )
        .expect("regex de progression valide")
    })
}

fn unit_to_bytes(value: f64, unit: &str) -> u64 {
    let mult = match unit {
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => 1.0,
    };
    (value * mult).round() as u64
}

/// `"01:23"` ou `"1:02:03"` → secondes.
fn eta_to_seconds(eta: &str) -> Option<u64> {
    eta.split(':')
        .try_fold(0u64, |total, part| Some(total * 60 + part.parse::<u64>().ok()?))
}

/// Analyse une ligne de progression yt-dlp (`--newline`).
pub fn parse_progress(line: &str) -> Option<YtProgress> {
    let c = progress_re().captures(line.trim())?;
    let percent: f32 = c[1].parse().ok()?;
    let total_bytes = unit_to_bytes(c[2].parse().ok()?, &c[3]);
    let speed_bps = match (c.get(4), c.get(5)) {
        (Some(v), Some(u)) => v.as_str().parse::<f64>().ok().map(|v| unit_to_bytes(v, u.as_str())),
        _ => None,
    };
    let eta_seconds = c.get(6).and_then(|m| eta_to_seconds(m.as_str()));
    Some(YtProgress {
        percent,
        total_bytes,
        speed_bps,
        eta_seconds,
    })
}

fn path_re() -> &'static [Regex; 3] {
    static RE: OnceLock<[Regex; 3]> = OnceLock::new();
    RE.get_or_init(|| {
        [
            Regex::new(r#"^\[Merger\] Merging formats into "(.+)"\s*$"#).expect("regex valide"),
            Regex::new(r"^\[download\] Destination: (.+?)\s*$").expect("regex valide"),
            Regex::new(r"^\[download\] (.+?) has already been downloaded").expect("regex valide"),
        ]
    })
}

/// Chemin de fichier annoncé par une ligne de yt-dlp (fusion, destination, déjà téléchargé).
pub fn parse_output_path(line: &str) -> Option<PathBuf> {
    let line = line.trim();
    path_re()
        .iter()
        .find_map(|re| re.captures(line).map(|c| PathBuf::from(&c[1])))
}

/// Fichier de travail de yt-dlp (fragment de format ou reste partiel), pas un résultat.
fn is_work_file(name: &str, stem: &str) -> bool {
    let rest = name
        .strip_prefix(stem)
        .map(|r| r.trim_start_matches('.'))
        .unwrap_or(name);
    let lower = name.to_ascii_lowercase();
    let fragment = Regex::new(r"^f[\w-]+\.").map(|re| re.is_match(rest)).unwrap_or(false);
    fragment
        || [".part", ".ytdl", ".temp", ".tmp"]
            .iter()
            .any(|e| lower.ends_with(e))
}

/// Retrouve le fichier produit : le plus récent `<stem>.*` qui n'est pas un fichier de travail.
pub fn find_output(dir: &Path, stem: &str) -> Option<PathBuf> {
    let prefix = format!("{stem}.");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&prefix) && !is_work_file(&name, stem)
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// Arguments yt-dlp pour un média résolu.
pub fn build_args(
    media: &ResolvedMedia,
    output_stem: &Path,
    ffmpeg_dir: Option<&Path>,
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |xs: &[&str]| a.extend(xs.iter().map(|x| x.to_string()));
    push(&[
        "-f",
        "bv*+ba/b",
        "--merge-output-format",
        "mp4",
        "--concurrent-fragments",
        "8",
    ]);
    if let Some(dir) = ffmpeg_dir {
        a.push("--ffmpeg-location".into());
        a.push(dir.to_string_lossy().into_owned());
    }
    a.extend(
        [
            "--no-check-certificates",
            "--newline",
            "--no-playlist",
            "--user-agent",
        ]
        .map(String::from),
    );
    let h = &media.http;
    a.push(if h.user_agent.is_empty() {
        BROWSER_UA.to_string()
    } else {
        h.user_agent.clone()
    });
    if let Some(r) = h.referer.as_deref().filter(|r| !r.is_empty()) {
        a.push("--referer".into());
        a.push(r.to_string());
    }
    if let Some(o) = h.origin.as_deref().filter(|o| !o.is_empty()) {
        a.push("--add-header".into());
        a.push(format!("Origin:{o}"));
    }
    if let Some(c) = h.cookies.as_deref().filter(|c| !c.is_empty()) {
        a.push("--add-header".into());
        a.push(format!("Cookie:{c}"));
    }
    for (k, v) in &h.extra_headers {
        a.push("--add-header".into());
        a.push(format!("{k}:{v}"));
    }
    a.push("-o".into());
    // `%` est spécial dans les modèles de sortie de yt-dlp
    a.push(format!(
        "{}.%(ext)s",
        output_stem.to_string_lossy().replace('%', "%%")
    ));
    a.push("--".into());
    a.push(media.url.clone());
    a
}

/// Découpe un flux d'octets en lignes (`\n`, `\r` ou `\r\n`).
#[derive(Default)]
struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' || b == b'\r' {
                if let Some(l) = self.take() {
                    out.push(l);
                }
            } else {
                self.buf.push(b);
            }
        }
        out
    }

    fn take(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let line = String::from_utf8_lossy(&self.buf).trim().to_string();
        self.buf.clear();
        (!line.is_empty()).then_some(line)
    }

    fn finish(&mut self) -> Option<String> {
        self.take()
    }
}

fn spawn_reader<R>(mut reader: R, is_stderr: bool, tx: mpsc::UnboundedSender<(bool, String)>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut splitter = LineSplitter::default();
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for line in splitter.push(&chunk[..n]) {
                        if tx.send((is_stderr, line)).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        if let Some(line) = splitter.finish() {
            let _ = tx.send((is_stderr, line));
        }
    });
}

/// Télécharge `media.url` avec yt-dlp vers `<output_stem>.<ext>` ; renvoie le fichier final.
pub async fn download(
    media: &ResolvedMedia,
    output_stem: &Path,
    ctx: &JobCtx,
) -> anyhow::Result<PathBuf> {
    ctx.check()?;
    let dir = output_stem
        .parent()
        .context("dossier de sortie invalide")?
        .to_path_buf();
    let stem = output_stem
        .file_name()
        .context("nom de fichier invalide")?
        .to_string_lossy()
        .into_owned();
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("création du dossier {}", dir.display()))?;

    let ffmpeg = tools::tool_path(Tool::Ffmpeg);
    let args = build_args(media, output_stem, ffmpeg.as_deref().and_then(Path::parent));
    let mut cmd = tools::command(Tool::YtDlp)?;
    cmd.args(&args)
        .env("PYTHONIOENCODING", "utf-8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    ctx.info(format!("yt-dlp : {}", media.url));
    let mut child = cmd.spawn().context("lancement de yt-dlp impossible")?;

    let (tx, mut rx) = mpsc::unbounded_channel::<(bool, String)>();
    if let Some(out) = child.stdout.take() {
        spawn_reader(out, false, tx.clone());
    }
    if let Some(err) = child.stderr.take() {
        spawn_reader(err, true, tx.clone());
    }
    drop(tx);

    let mut tail: VecDeque<String> = VecDeque::with_capacity(6);
    let mut last_path: Option<PathBuf> = None;
    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = child.kill().await;
                bail!("Annulé");
            }
            msg = rx.recv() => {
                let Some((is_stderr, line)) = msg else { break };
                if let Some(p) = parse_progress(&line) {
                    ctx.progress(Progress {
                        phase: Some(Phase::Downloading),
                        percent: p.percent.clamp(0.0, 100.0),
                        downloaded_bytes: (p.total_bytes as f64 * f64::from(p.percent) / 100.0) as u64,
                        total_bytes: Some(p.total_bytes),
                        speed_bps: p.speed_bps,
                        eta_seconds: p.eta_seconds,
                        ..Progress::default()
                    });
                    continue;
                }
                if let Some(p) = parse_output_path(&line) {
                    last_path = Some(p);
                }
                if is_stderr {
                    if tail.len() == 5 {
                        tail.pop_front();
                    }
                    tail.push_back(line.clone());
                    let upper = line.trim_start().to_ascii_uppercase();
                    if upper.starts_with("ERROR") || upper.starts_with("WARNING") {
                        ctx.warn(line);
                    } else {
                        ctx.info(line);
                    }
                } else {
                    ctx.info(line);
                }
            }
        }
    }

    let status = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            let _ = child.kill().await;
            bail!("Annulé");
        }
        st = child.wait() => st.context("attente de yt-dlp")?,
    };
    if !status.success() {
        if tail.is_empty() {
            match status.code() {
                Some(code) => bail!("yt-dlp a quitté avec le code {code}"),
                None => bail!("yt-dlp a été interrompu"),
            }
        }
        bail!("{}", tail.into_iter().collect::<Vec<_>>().join("\n"));
    }

    let by_log = last_path.filter(|p| {
        p.is_file()
            && p.file_name()
                .map(|n| !is_work_file(&n.to_string_lossy(), &stem))
                .unwrap_or(false)
    });
    by_log
        .or_else(|| find_output(&dir, &stem))
        .context("yt-dlp a terminé mais le fichier produit est introuvable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{MediaKind, ResolvedVia};
    use crate::model::HttpCtx;

    #[test]
    fn parses_standard_progress_line() {
        let p = parse_progress("[download]  45.3% of ~ 123.45MiB at  2.31MiB/s ETA 00:32 (frag 3/10)")
            .unwrap();
        assert_eq!(p.percent, 45.3);
        assert_eq!(p.total_bytes, (123.45 * 1024.0 * 1024.0_f64).round() as u64);
        assert_eq!(p.speed_bps, Some((2.31 * 1024.0 * 1024.0_f64).round() as u64));
        assert_eq!(p.eta_seconds, Some(32));
    }

    #[test]
    fn parses_hours_in_eta_and_units() {
        let p = parse_progress("[download]   3.0% of    1.50GiB at  500.00KiB/s ETA 1:02:03").unwrap();
        assert_eq!(p.total_bytes, (1.5 * 1024.0 * 1024.0 * 1024.0_f64) as u64);
        assert_eq!(p.speed_bps, Some(512_000));
        assert_eq!(p.eta_seconds, Some(3723));
    }

    #[test]
    fn parses_finished_and_unknown_speed() {
        let p = parse_progress("[download] 100% of   10.00MiB in 00:00:05 at 2.00MiB/s").unwrap();
        assert_eq!(p.percent, 100.0);
        assert_eq!(p.eta_seconds, None);
        assert!(p.speed_bps.is_some());
        let p = parse_progress("[download]  10.0% of 100.00MiB at Unknown B/s ETA Unknown").unwrap();
        assert_eq!(p.speed_bps, None);
        assert_eq!(p.eta_seconds, None);
    }

    #[test]
    fn ignores_other_lines() {
        assert!(parse_progress("[download] Destination: a.mp4").is_none());
        assert!(parse_progress("[info] Available formats").is_none());
        assert!(parse_progress("ERROR: 403 Forbidden").is_none());
    }

    #[test]
    fn parses_output_paths() {
        assert_eq!(
            parse_output_path(r#"[Merger] Merging formats into "C:\v\film.mp4""#),
            Some(PathBuf::from(r"C:\v\film.mp4"))
        );
        assert_eq!(
            parse_output_path("[download] Destination: C:\\v\\film.f137.mp4"),
            Some(PathBuf::from("C:\\v\\film.f137.mp4"))
        );
        assert_eq!(
            parse_output_path("[download] C:\\v\\film.mp4 has already been downloaded"),
            Some(PathBuf::from("C:\\v\\film.mp4"))
        );
        assert_eq!(parse_output_path("[info] rien"), None);
    }

    #[test]
    fn find_output_skips_work_files() {
        let dir = tempfile::tempdir().unwrap();
        for f in [
            "film.f137.mp4",
            "film.f140.m4a",
            "film.mp4.part",
            "film.mp4.ytdl",
            "film.mp4",
            "autre.mp4",
        ] {
            std::fs::write(dir.path().join(f), b"x").unwrap();
        }
        assert_eq!(find_output(dir.path(), "film"), Some(dir.path().join("film.mp4")));
        assert_eq!(find_output(dir.path(), "absent"), None);
    }

    #[test]
    fn line_splitter_handles_cr_and_partial_chunks() {
        let mut s = LineSplitter::default();
        assert_eq!(s.push(b"abc\r\ndef\rgh"), vec!["abc", "def"]);
        assert_eq!(s.push(b"i\n\n"), vec!["ghi"]);
        assert_eq!(s.push(b"fin"), Vec::<String>::new());
        assert_eq!(s.finish().as_deref(), Some("fin"));
    }

    fn media(http: HttpCtx) -> ResolvedMedia {
        ResolvedMedia {
            kind: MediaKind::Ytdlp,
            url: "https://site.example/watch?v=1".into(),
            http,
            via: ResolvedVia::Ytdlp,
            title: None,
            duration: None,
            height: None,
        }
    }

    #[test]
    fn builds_expected_args() {
        let mut http = HttpCtx::for_url("https://site.example/watch?v=1", Some("https://ref.example/p"));
        http.cookies = Some("a=b".into());
        let args = build_args(
            &media(http),
            Path::new("C:/v/100%"),
            Some(Path::new("C:/app")),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-f bv*+ba/b"));
        assert!(joined.contains("--merge-output-format mp4"));
        assert!(joined.contains("--concurrent-fragments 8"));
        assert!(joined.contains("--ffmpeg-location C:/app"));
        assert!(joined.contains("--no-check-certificates --newline --no-playlist"));
        assert!(joined.contains("--referer https://ref.example/p"));
        assert!(joined.contains("--add-header Origin:https://ref.example"));
        assert!(joined.contains("--add-header Cookie:a=b"));
        assert!(joined.contains("-o C:/v/100%%.%(ext)s"));
        assert_eq!(args.last().unwrap(), "https://site.example/watch?v=1");
        let ua_pos = args.iter().position(|a| a == "--user-agent").unwrap();
        assert_eq!(args[ua_pos + 1], BROWSER_UA);
    }

    #[test]
    fn args_without_ffmpeg_or_headers() {
        let args = build_args(&media(HttpCtx::default()), Path::new("C:/v/x"), None);
        assert!(!args.contains(&"--ffmpeg-location".to_string()));
        assert!(!args.contains(&"--referer".to_string()));
        assert!(!args.contains(&"--add-header".to_string()));
    }
}
