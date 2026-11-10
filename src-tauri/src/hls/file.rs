// Téléchargement HTTP direct d'un fichier : flux sur disque, reprise par Range via un fichier
// `.part`, nouvelles tentatives avec backoff, progression, puis renommage final.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use super::fetch::{backoff, get_response, is_fatal, sleep_cancel, SpeedEma};
use crate::model::{HttpCtx, JobCtx, Phase, Progress};

const MAX_ATTEMPTS: u32 = 5;

fn part_path(output: &Path) -> PathBuf {
    let mut p = output.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Taille totale annoncée par `Content-Range: bytes a-b/total` ou `bytes */total`.
fn total_from_content_range(v: &str) -> Option<u64> {
    v.rsplit('/').next()?.trim().parse().ok()
}

/// Une tentative : en cas de succès, le fichier est complet dans le `.part`.
async fn attempt(url: &str, http: &HttpCtx, part: &Path, ctx: &JobCtx) -> Result<()> {
    let mut start = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    let range = (start > 0).then_some((start, None));
    let resp = match get_response(url, http, range).await {
        Ok(r) => r,
        Err(e) => {
            // 416 : la plage demandée dépasse le fichier -> déjà complet ou .part inutilisable
            if let Some(h) = e.downcast_ref::<super::fetch::HttpStatusError>() {
                if h.status == 416 {
                    let _ = tokio::fs::remove_file(part).await;
                    bail!("reprise impossible (plage refusée), redémarrage du téléchargement");
                }
            }
            return Err(e);
        }
    };
    let status = resp.status().as_u16();
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()) {
        if ct.to_lowercase().starts_with("text/html") {
            bail!("le serveur a renvoyé une page web à la place du fichier");
        }
    }
    let content_len = resp.content_length();
    let total = if status == 206 {
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(total_from_content_range)
            .or_else(|| content_len.map(|l| l + start))
    } else {
        content_len
    };
    // Le serveur ignore Range (200) : on repart de zéro
    let mut file = if status == 206 && start > 0 {
        tokio::fs::OpenOptions::new().append(true).open(part).await?
    } else {
        start = 0;
        tokio::fs::File::create(part).await?
    };

    let mut written = start;
    let mut speed = SpeedEma::new();
    let mut last_emit = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let base = start;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        ctx.check()?;
        let chunk = chunk.map_err(|e| anyhow!("lecture interrompue : {e}"))?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
        if last_emit.elapsed() >= std::time::Duration::from_millis(250) {
            last_emit = std::time::Instant::now();
            emit(ctx, written, total, speed.update(written - base));
        }
    }
    file.flush().await?;
    drop(file);
    if let Some(t) = total {
        if written != t {
            bail!("téléchargement incomplet ({written} octets sur {t})");
        }
    }
    if written == 0 {
        bail!("fichier vide");
    }
    emit(ctx, written, total, speed.update(written - base));
    Ok(())
}

fn emit(ctx: &JobCtx, written: u64, total: Option<u64>, speed: Option<u64>) {
    let percent = match total {
        Some(t) if t > 0 => (written as f32 / t as f32 * 100.0).min(100.0),
        _ => 0.0,
    };
    let eta = match (total, speed) {
        (Some(t), Some(s)) if s > 0 => Some(t.saturating_sub(written) / s),
        _ => None,
    };
    ctx.progress(Progress {
        phase: Some(Phase::Downloading),
        percent,
        downloaded_bytes: written,
        total_bytes: total,
        speed_bps: speed,
        eta_seconds: eta,
        segments_done: None,
        segments_total: None,
    });
}

pub async fn download_file(url: &str, http: &HttpCtx, output: &Path, ctx: &JobCtx) -> Result<()> {
    ctx.check()?;
    if let Some(dir) = output.parent() {
        if !dir.as_os_str().is_empty() {
            tokio::fs::create_dir_all(dir).await?;
        }
    }
    let part = part_path(output);
    emit(ctx, 0, None, None);
    let mut last_err = anyhow!("aucune tentative");
    for n in 0..MAX_ATTEMPTS {
        ctx.check()?;
        match attempt(url, http, &part, ctx).await {
            Ok(()) => {
                tokio::fs::rename(&part, output)
                    .await
                    .map_err(|e| anyhow!("Impossible de finaliser {} : {e}", output.display()))?;
                return Ok(());
            }
            Err(e) => {
                if ctx.is_cancelled() {
                    return Err(anyhow!("Annulé"));
                }
                if is_fatal(&e) {
                    return Err(e);
                }
                ctx.warn(format!("Téléchargement interrompu ({}/{MAX_ATTEMPTS}) : {e:#}", n + 1));
                last_err = e;
                if n + 1 < MAX_ATTEMPTS {
                    sleep_cancel(ctx, backoff(n)).await?;
                }
            }
        }
    }
    Err(anyhow!("Échec du téléchargement après {MAX_ATTEMPTS} tentatives : {last_err:#}"))
}

#[cfg(test)]
mod tests {
    use super::super::testsrv::{self, tempdir, Route};
    use super::*;

    fn payload(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn hls_file_telecharge_et_renomme() {
        let data = payload(50_000);
        let srv = testsrv::start(vec![("/f.mp4", Route::ok(data.clone()))]).await;
        let dir = tempdir("file1");
        let out = dir.join("sub").join("f.mp4");
        let ctx = JobCtx::noop();
        download_file(&format!("{}/f.mp4", srv.base), &HttpCtx::default(), &out, &ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        assert!(!part_path(&out).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_file_reprend_depuis_le_part() {
        let data = payload(10_000);
        let srv = testsrv::start(vec![("/f.mp4", Route::ok(data.clone()))]).await;
        let dir = tempdir("file2");
        let out = dir.join("f.mp4");
        std::fs::write(part_path(&out), &data[..4000]).unwrap();
        download_file(&format!("{}/f.mp4", srv.base), &HttpCtx::default(), &out, &JobCtx::noop())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_file_part_deja_complet_ou_trop_grand_redemarre() {
        let data = payload(3_000);
        let srv = testsrv::start(vec![("/f.mp4", Route::ok(data.clone()))]).await;
        let dir = tempdir("file3");
        let out = dir.join("f.mp4");
        // .part plus grand que le fichier : 416 puis redémarrage propre
        std::fs::write(part_path(&out), payload(9_000)).unwrap();
        download_file(&format!("{}/f.mp4", srv.base), &HttpCtx::default(), &out, &JobCtx::noop())
            .await
            .unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), data);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_file_403_echoue_sans_insister() {
        let srv = testsrv::start(vec![(
            "/f.mp4",
            Route { status: 403, content_type: "text/plain", body: b"no".to_vec() },
        )])
        .await;
        let dir = tempdir("file4");
        let out = dir.join("f.mp4");
        let err = download_file(&format!("{}/f.mp4", srv.base), &HttpCtx::default(), &out, &JobCtx::noop())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
        assert_eq!(srv.hits("/f.mp4"), 1);
        assert!(!out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hls_file_content_range() {
        assert_eq!(total_from_content_range("bytes 10-19/100"), Some(100));
        assert_eq!(total_from_content_range("bytes */42"), Some(42));
        assert_eq!(total_from_content_range("bytes 0-1/*"), None);
    }
}
