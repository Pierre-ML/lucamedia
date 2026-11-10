// Assemblage final : ffmpeg lit les playlists locales (segments déjà propres) et écrit un MP4
// par copie de flux. Aucun ré-encodage ; la sortie passe par un fichier `.part.mp4`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Result};
use tokio::io::AsyncReadExt;

use super::{HlsDownload, TrackInfo};
use crate::model::{JobCtx, Phase, Progress};
use crate::tools::{command, Tool};

/// `time=00:01:23.45` -> secondes.
fn parse_time(line: &str) -> Option<f64> {
    let idx = line.rfind("time=")?;
    let rest = &line[idx + 5..];
    let token = rest.split_whitespace().next()?;
    let mut parts = token.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

/// `speed=1.5x` -> 1.5
fn parse_speed(line: &str) -> Option<f64> {
    let idx = line.rfind("speed=")?;
    let token = line[idx + 6..].split_whitespace().next()?;
    token.trim_end_matches('x').parse().ok()
}

/// `size=  1234kB` / `Lsize=1234KiB` -> octets.
fn parse_size(line: &str) -> Option<u64> {
    let idx = line.rfind("size=")?;
    let rest = line[idx + 5..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    let n: u64 = rest[..end].parse().ok()?;
    let unit = rest[end..].to_lowercase();
    if unit.starts_with("kib") || unit.starts_with("kb") {
        Some(n * 1024)
    } else if unit.starts_with("mib") || unit.starts_with("mb") {
        Some(n * 1024 * 1024)
    } else {
        Some(n)
    }
}

fn is_progress_line(line: &str) -> bool {
    line.contains("time=") && line.contains("bitrate=")
}

/// Fichier temporaire à côté de la sortie : `nom.part.mp4`.
fn part_path(output: &Path) -> PathBuf {
    output.with_extension("part.mp4")
}

/// Arguments ffmpeg (hors chemin de sortie temporaire).
fn build_args(dl: &HlsDownload, part: &Path, aac_bsf: bool) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-stats", "-y"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut inputs: Vec<&TrackInfo> = vec![&dl.video];
    if let Some(au) = &dl.audio {
        inputs.push(au);
    }
    for t in &inputs {
        a.extend(
            [
                "-fflags",
                "+genpts",
                "-allowed_extensions",
                "ALL",
                "-protocol_whitelist",
                "file,crypto,data",
                "-i",
            ]
            .map(String::from),
        );
        a.push(t.local_playlist.to_string_lossy().into_owned());
    }
    a.extend(["-map", "0:v:0?"].map(String::from));
    a.push("-map".into());
    a.push(if dl.audio.is_some() { "1:a:0?" } else { "0:a:0?" }.into());
    a.extend(["-c", "copy"].map(String::from));
    if aac_bsf {
        a.extend(["-bsf:a", "aac_adtstoasc"].map(String::from));
    }
    a.extend(["-movflags", "+faststart", "-f", "mp4"].map(String::from));
    a.push(part.to_string_lossy().into_owned());
    a
}

/// Codec audio du premier segment (ffprobe) ; `None` si ffprobe est indisponible ou échoue,
/// `Some("")` s'il n'y a pas de flux audio.
async fn probe_audio_codec(segment: &Path) -> Option<String> {
    let mut cmd = command(Tool::Ffprobe).ok()?;
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "a:0",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "csv=p=0",
    ])
    .arg(segment)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    let out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .trim_end_matches(',')
            .to_lowercase(),
    )
}

/// Résultat d'une exécution de ffmpeg.
enum Run {
    Ok,
    Failed(String),
}

async fn run_ffmpeg(args: &[String], expected: f64, ctx: &JobCtx) -> Result<Run> {
    let mut cmd = command(Tool::Ffmpeg)?;
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Impossible de lancer ffmpeg : {e}"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Sortie d'erreur de ffmpeg inaccessible"))?;

    let mut tail: Vec<String> = Vec::new();
    let mut pending = String::new();
    let mut buf = [0u8; 4096];
    let mut last_percent = -1.0f32;

    loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                let _ = child.kill().await;
                bail!("Annulé");
            }
            n = stderr.read(&mut buf) => {
                let n = n.unwrap_or(0);
                if n == 0 { break; }
                pending.push_str(&String::from_utf8_lossy(&buf[..n]));
                // ffmpeg termine ses lignes de progression par \r
                while let Some(pos) = pending.find(['\r', '\n']) {
                    let line: String = pending.drain(..=pos).collect();
                    let line = line.trim();
                    if line.is_empty() { continue; }
                    if is_progress_line(line) {
                        if let Some(t) = parse_time(line) {
                            let percent = if expected > 0.0 {
                                ((t / expected) * 100.0).clamp(0.0, 99.9) as f32
                            } else {
                                0.0
                            };
                            if (percent - last_percent).abs() >= 0.1 {
                                last_percent = percent;
                                let eta = match parse_speed(line) {
                                    Some(sp) if sp > 0.0 && expected > t => Some(((expected - t) / sp) as u64),
                                    _ => None,
                                };
                                ctx.progress(Progress {
                                    phase: Some(Phase::Assembling),
                                    percent,
                                    downloaded_bytes: parse_size(line).unwrap_or(0),
                                    eta_seconds: eta,
                                    ..Default::default()
                                });
                            }
                        }
                    } else {
                        tail.push(line.to_string());
                        if tail.len() > 30 { tail.remove(0); }
                    }
                }
            }
        }
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(Run::Ok)
    } else {
        let start = tail.len().saturating_sub(8);
        let msg = tail[start..].join("\n");
        Ok(Run::Failed(if msg.is_empty() {
            format!("ffmpeg a quitté avec le code {:?}", status.code())
        } else {
            msg
        }))
    }
}

/// Assemble les pistes locales en un MP4 (`-c copy`). Écrit dans `output` par un `.part.mp4`.
pub async fn assemble(dl: &HlsDownload, output: &Path, ctx: &JobCtx) -> Result<()> {
    ctx.check()?;
    if let Some(dir) = output.parent() {
        if !dir.as_os_str().is_empty() {
            tokio::fs::create_dir_all(dir).await?;
        }
    }
    let part = part_path(output);
    let _ = tokio::fs::remove_file(&part).await;
    ctx.progress(Progress {
        phase: Some(Phase::Assembling),
        ..Default::default()
    });
    ctx.info("Assemblage du MP4 (copie de flux, sans ré-encodage)");

    // Le filtre aac_adtstoasc n'est utile que pour de l'AAC ADTS dans du MPEG-TS
    let audio_src = dl.audio.as_ref().unwrap_or(&dl.video);
    let is_ts = audio_src.init_segment.is_none();
    let probe = match (is_ts, audio_src.segments.first()) {
        (true, Some(seg)) => probe_audio_codec(&seg.path).await,
        _ => Some(String::new()),
    };
    // Tentatives : [avec/sans bsf selon la sonde] ; sonde indisponible : on essaie les deux
    let attempts: Vec<bool> = if !is_ts {
        vec![false]
    } else {
        match probe.as_deref() {
            Some("aac") => vec![true],
            Some(_) => vec![false],
            None => vec![true, false],
        }
    };

    let expected = dl.video.expected_duration;
    let mut last_err = String::new();
    for (n, bsf) in attempts.iter().enumerate() {
        let args = build_args(dl, &part, *bsf);
        match run_ffmpeg(&args, expected, ctx).await {
            Ok(Run::Ok) => {
                tokio::fs::rename(&part, output).await.map_err(|e| {
                    anyhow!("Impossible de finaliser {} : {e}", output.display())
                })?;
                ctx.progress(Progress {
                    phase: Some(Phase::Assembling),
                    percent: 100.0,
                    ..Default::default()
                });
                return Ok(());
            }
            Ok(Run::Failed(msg)) => {
                last_err = msg;
                let _ = tokio::fs::remove_file(&part).await;
                if n + 1 < attempts.len() {
                    ctx.warn("Assemblage échoué, nouvel essai avec une autre configuration audio");
                }
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        }
    }
    Err(anyhow!("ffmpeg a échoué : {last_err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hls::{SegmentInfo, TrackKind};

    fn track(kind: TrackKind, init: bool) -> TrackInfo {
        TrackInfo {
            kind,
            playlist_url: "http://h/p.m3u8".into(),
            local_playlist: PathBuf::from(format!("W/{}.m3u8", kind.dir_name())),
            init_segment: init.then(|| PathBuf::from("W/video/init.mp4")),
            segments: vec![SegmentInfo {
                index: 0,
                sequence: 0,
                url: "http://h/s".into(),
                path: PathBuf::from("W/video/seg_000000.ts"),
                duration: 4.0,
                discontinuity_before: false,
                bytes: 10,
            }],
            expected_duration: 4.0,
        }
    }

    fn dl(audio: bool) -> HlsDownload {
        HlsDownload {
            work_dir: PathBuf::from("W"),
            video: track(TrackKind::Video, false),
            audio: audio.then(|| track(TrackKind::Audio, false)),
            width: None,
            height: None,
            bandwidth: None,
        }
    }

    #[test]
    fn hls_parse_ffmpeg_progress() {
        let l = "frame=  100 fps=0.0 q=-1.0 size=    1536kB time=00:01:23.45 bitrate= 150.0kbits/s speed=2.5x";
        assert!(is_progress_line(l));
        assert!((parse_time(l).unwrap() - 83.45).abs() < 1e-9);
        assert_eq!(parse_size(l), Some(1536 * 1024));
        assert_eq!(parse_speed(l), Some(2.5));
        assert_eq!(parse_size("Lsize=   10KiB time=00:00:01.00 bitrate=1x"), Some(10 * 1024));
    }

    #[test]
    fn hls_args_sans_audio_separe() {
        let a = build_args(&dl(false), Path::new("out.part.mp4"), false);
        let s = a.join(" ");
        assert!(s.contains("-allowed_extensions ALL -protocol_whitelist file,crypto,data -i W/video.m3u8"));
        assert!(s.contains("-map 0:v:0? -map 0:a:0?"));
        assert!(s.contains("-c copy"));
        assert!(!s.contains("aac_adtstoasc"));
        assert!(s.contains("-movflags +faststart"));
        assert!(s.contains("-fflags +genpts"));
        assert!(s.contains("-y"));
        assert!(s.ends_with("out.part.mp4"));
    }

    #[test]
    fn hls_args_avec_audio_separe_et_bsf() {
        let a = build_args(&dl(true), Path::new("o.part.mp4"), true);
        let s = a.join(" ");
        assert!(s.contains("-i W/video.m3u8"));
        assert!(s.contains("-i W/audio.m3u8"));
        assert!(s.contains("-map 0:v:0? -map 1:a:0?"));
        assert!(s.contains("-bsf:a aac_adtstoasc"));
    }

    /// Génère un petit HLS avec ffmpeg, le sert en local, le télécharge puis l'assemble.
    /// Ignoré (succès) si ffmpeg/ffprobe sont introuvables.
    async fn e2e(fmp4: bool) {
        use crate::hls::testsrv::{self, tempdir, Route};
        use crate::tools::tool_path;
        let (Some(ff), Some(fp)) = (tool_path(Tool::Ffmpeg), tool_path(Tool::Ffprobe)) else {
            eprintln!("ffmpeg/ffprobe absents : test e2e ignoré");
            return;
        };
        let src = tempdir(if fmp4 { "e2e-src-m4s" } else { "e2e-src-ts" });
        let ext = if fmp4 { "m4s" } else { "ts" };
        let mut args: Vec<String> = [
            "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=duration=6:size=160x120:rate=25", "-f", "lavfi", "-i",
            "sine=duration=6", "-c:v", "libx264", "-g", "50", "-pix_fmt", "yuv420p", "-c:a", "aac", "-f", "hls",
            "-hls_time", "2", "-hls_playlist_type", "vod",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if fmp4 {
            args.extend(["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4"].map(String::from));
        }
        args.push("-hls_segment_filename".into());
        args.push(src.join(format!("s%d.{ext}")).to_string_lossy().into_owned());
        args.push(src.join("v.m3u8").to_string_lossy().into_owned());
        let st = std::process::Command::new(&ff).args(&args).current_dir(&src).status().unwrap();
        assert!(st.success(), "génération du HLS de test impossible");

        let mut routes: Vec<(String, Route)> = Vec::new();
        for e in std::fs::read_dir(&src).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            routes.push((format!("/{name}"), Route::ok(std::fs::read(e.path()).unwrap())));
        }
        let srv = testsrv::start(routes.iter().map(|(p, r)| (p.as_str(), r.clone())).collect()).await;
        let work = tempdir("e2e-work");
        let ctx = JobCtx::noop();
        let req = crate::hls::HlsRequest {
            url: format!("{}/v.m3u8", srv.base),
            http: crate::model::HttpCtx::default(),
            work_dir: work.clone(),
            parallel: 4,
            segment_retries: 2,
        };
        let dl = crate::hls::download(&req, &ctx).await.unwrap();
        assert_eq!(dl.video.init_segment.is_some(), fmp4);
        let out = work.join("sortie").join("film.mp4");
        crate::hls::assemble(&dl, &out, &ctx).await.unwrap();
        assert!(out.exists());
        assert!(!part_path(&out).exists());

        let probe = |entries: &str| {
            let o = std::process::Command::new(&fp)
                .args(["-v", "error", "-show_entries", entries, "-of", "csv=p=0"])
                .arg(&out)
                .output()
                .unwrap();
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        let streams = probe("stream=codec_type");
        assert!(streams.contains("video") && streams.contains("audio"), "{streams}");
        let dur: f64 = probe("format=duration").trim().parse().unwrap();
        assert!((dur - 6.0).abs() < 0.5, "durée {dur}");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn hls_e2e_ts_aac_avec_bsf() {
        e2e(false).await;
    }

    #[tokio::test]
    async fn hls_e2e_fmp4() {
        e2e(true).await;
    }

    #[tokio::test]
    async fn hls_assemble_annule() {
        let ctx = JobCtx::noop();
        ctx.cancel.cancel();
        let err = assemble(&dl(false), Path::new("x.mp4"), &ctx).await.unwrap_err();
        assert_eq!(err.to_string(), "Annulé");
    }

    #[test]
    fn hls_chemin_temporaire() {
        assert_eq!(part_path(Path::new("D/film.mp4")), PathBuf::from("D/film.part.mp4"));
    }
}
