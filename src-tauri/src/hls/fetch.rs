// Téléchargement des segments HLS : parallèle, avec reprise, déchiffrement AES-128,
// nettoyage des faux en-têtes et écriture atomique. Aucun segment n'est jamais ignoré :
// un segment qui échoue définitivement fait échouer tout le téléchargement.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes::Aes128;
use anyhow::{anyhow, bail, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures::stream::{self, StreamExt};

use super::playlist::{self, ByteRange, MapInfo, MediaPlaylist, Parsed};
use super::{HlsDownload, HlsRequest, SegmentInfo, TrackInfo, TrackKind};
use crate::model::{HttpCtx, JobCtx, Phase, Progress};

const TS_SYNC: u8 = 0x47; // octet de synchronisation MPEG-TS, répété tous les 188 octets

// ---------------------------------------------------------------------------------------
// Erreurs HTTP, nouvelles tentatives, débit
// ---------------------------------------------------------------------------------------

/// Statut HTTP d'échec (conservé dans la chaîne d'erreur pour décider de réessayer ou non).
#[derive(Debug)]
pub(crate) struct HttpStatusError {
    pub status: u16,
    pub host: String,
}

impl fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} sur {}", self.status, self.host)
    }
}

impl std::error::Error for HttpStatusError {}

/// Statuts pour lesquels réessayer ne sert à rien (jeton expiré, accès refusé, ressource retirée).
pub(crate) fn is_fatal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HttpStatusError>()
        .map(|h| matches!(h.status, 401 | 403 | 410))
        .unwrap_or(false)
}

/// 500 ms, 1 s, 2 s, 4 s, puis 8 s.
pub(crate) fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(500u64 << attempt.min(4))
}

pub(crate) async fn sleep_cancel(ctx: &JobCtx, d: Duration) -> Result<()> {
    tokio::select! {
        _ = tokio::time::sleep(d) => Ok(()),
        _ = ctx.cancel.cancelled() => ctx.check(),
    }
}

/// Exécute `f` jusqu'à `attempts` fois avec backoff. L'annulation interrompt tout.
pub(crate) async fn with_retries<T, F, Fut>(ctx: &JobCtx, attempts: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let attempts = attempts.max(1);
    for n in 0..attempts {
        ctx.check()?;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if ctx.is_cancelled() {
                    return Err(anyhow!("Annulé"));
                }
                if is_fatal(&e) || n + 1 >= attempts {
                    return Err(e);
                }
                ctx.warn(format!(
                    "Tentative {}/{} échouée : {e:#} — nouvel essai",
                    n + 1,
                    attempts
                ));
                sleep_cancel(ctx, backoff(n)).await?;
            }
        }
    }
    Err(anyhow!("Aucune tentative effectuée"))
}

/// Débit lissé (moyenne mobile exponentielle) en octets/s.
pub(crate) struct SpeedEma {
    last_t: Instant,
    last_b: u64,
    ema: Option<f64>,
}

impl SpeedEma {
    pub fn new() -> Self {
        SpeedEma {
            last_t: Instant::now(),
            last_b: 0,
            ema: None,
        }
    }

    pub fn update(&mut self, total_bytes: u64) -> Option<u64> {
        let dt = self.last_t.elapsed().as_secs_f64();
        if dt >= 0.5 {
            let inst = total_bytes.saturating_sub(self.last_b) as f64 / dt;
            self.ema = Some(match self.ema {
                None => inst,
                Some(e) => 0.6 * e + 0.4 * inst,
            });
            self.last_t = Instant::now();
            self.last_b = total_bytes;
        }
        self.ema.map(|v| v as u64)
    }
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

/// Envoie la requête GET (avec Range éventuel) et vérifie le statut.
pub(crate) async fn get_response(
    url: &str,
    http: &HttpCtx,
    range: Option<(u64, Option<u64>)>,
) -> Result<reqwest::Response> {
    let client = HttpCtx::client();
    let mut rb = http.apply(client.get(url));
    if let Some((start, end)) = range {
        let value = match end {
            Some(e) => format!("bytes={start}-{e}"),
            None => format!("bytes={start}-"),
        };
        rb = rb.header(reqwest::header::RANGE, value);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| anyhow!("requête impossible : {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(HttpStatusError {
            status: status.as_u16(),
            host: host_of(url),
        }
        .into());
    }
    Ok(resp)
}

// ---------------------------------------------------------------------------------------
// Progression
// ---------------------------------------------------------------------------------------

/// Soustraction saturante sur un compteur atomique.
fn sat_sub(a: &AtomicU64, n: u64) {
    let mut cur = a.load(Ordering::Relaxed);
    loop {
        match a.compare_exchange_weak(cur, cur.saturating_sub(n), Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(v) => cur = v,
        }
    }
}

struct MeterState {
    last_emit: Instant,
    speed: SpeedEma,
}

pub(crate) struct Meter {
    ctx: JobCtx,
    phase: Phase,
    total: u32,
    done: AtomicU32,
    bytes: AtomicU64,     // octets reçus (y compris segments en cours et déjà présents)
    net: AtomicU64,       // octets réellement transférés (pour le débit)
    completed: AtomicU64, // taille finale des segments terminés (pour l'estimation du total)
    st: Mutex<MeterState>,
}

impl Meter {
    fn new(ctx: &JobCtx, phase: Phase, total: u32) -> Self {
        Meter {
            ctx: ctx.clone(),
            phase,
            total,
            done: AtomicU32::new(0),
            bytes: AtomicU64::new(0),
            net: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            st: Mutex::new(MeterState {
                last_emit: Instant::now() - Duration::from_secs(10),
                speed: SpeedEma::new(),
            }),
        }
    }

    fn add(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
        self.net.fetch_add(n, Ordering::Relaxed);
        self.emit(false);
    }

    /// Annule le comptage d'une tentative échouée.
    fn sub(&self, n: u64) {
        sat_sub(&self.bytes, n);
        sat_sub(&self.net, n);
    }

    fn segment_done(&self, final_len: u64) {
        self.completed.fetch_add(final_len, Ordering::Relaxed);
        self.done.fetch_add(1, Ordering::Relaxed);
        self.emit(false);
    }

    fn segment_resumed(&self, len: u64) {
        self.bytes.fetch_add(len, Ordering::Relaxed);
        self.segment_done(len);
    }

    fn emit(&self, force: bool) {
        let (speed, bytes, done) = {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            if !force && st.last_emit.elapsed() < Duration::from_millis(200) {
                return;
            }
            st.last_emit = Instant::now();
            let net = self.net.load(Ordering::Relaxed);
            (
                st.speed.update(net),
                self.bytes.load(Ordering::Relaxed),
                self.done.load(Ordering::Relaxed),
            )
        };
        let percent = if self.total == 0 {
            100.0
        } else {
            (done as f32 / self.total as f32 * 100.0).min(100.0)
        };
        let total_bytes = if done > 0 {
            let avg = self.completed.load(Ordering::Relaxed) as f64 / done as f64;
            Some(((avg * self.total as f64) as u64).max(bytes))
        } else {
            None
        };
        let eta = match (speed, total_bytes) {
            (Some(s), Some(t)) if s > 0 => Some(t.saturating_sub(bytes) / s),
            _ => None,
        };
        self.ctx.progress(Progress {
            phase: Some(self.phase),
            percent,
            downloaded_bytes: bytes,
            total_bytes,
            speed_bps: speed,
            eta_seconds: eta,
            segments_done: Some(done),
            segments_total: Some(self.total),
        });
    }
}

// ---------------------------------------------------------------------------------------
// Nettoyage et déchiffrement d'un segment
// ---------------------------------------------------------------------------------------

/// Retire un éventuel faux en-tête (PNG/JPEG de quelques octets) devant le flux MPEG-TS : on
/// cherche le premier octet 0x47 suivi de deux autres à 188 octets d'intervalle, dans les
/// 64 premiers Ko. Pas un flux TS (fMP4, clé…) : tampon inchangé.
pub fn strip_fake_header(buf: &[u8]) -> &[u8] {
    if buf.len() <= 376 {
        return buf;
    }
    let limit = (buf.len() - 376).min(65_536);
    for i in 0..limit {
        if buf[i] == TS_SYNC && buf[i + 188] == TS_SYNC && buf[i + 376] == TS_SYNC {
            return &buf[i..];
        }
    }
    buf
}

/// Déchiffrement AES-128-CBC avec padding PKCS7.
pub fn decrypt_aes128(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!(
            "données chiffrées de longueur invalide ({} octets, non multiple de 16)",
            data.len()
        );
    }
    let mut buf = data.to_vec();
    let dec = cbc::Decryptor::<Aes128>::new(key.into(), iv.into());
    let n = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| anyhow!("déchiffrement AES-128 impossible (clé ou IV incorrect)"))?
        .len();
    buf.truncate(n);
    Ok(buf)
}

fn iv_from_sequence(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn looks_like_html(data: &[u8]) -> bool {
    let head: Vec<u8> = data
        .iter()
        .take(512)
        .skip_while(|b| b.is_ascii_whitespace())
        .take(16)
        .map(|b| b.to_ascii_lowercase())
        .collect();
    head.starts_with(b"<!doctype") || head.starts_with(b"<html") || head.starts_with(b"<head") || head.starts_with(b"<body")
}

/// Segment brut -> octets prêts pour ffmpeg (déchiffré, nettoyé). Un corps vide ou une page
/// HTML sont des échecs.
fn process_segment(raw: Vec<u8>, key: Option<([u8; 16], [u8; 16])>, is_ts: bool) -> Result<Vec<u8>> {
    if raw.is_empty() {
        bail!("corps de réponse vide");
    }
    if looks_like_html(&raw) {
        bail!("page HTML reçue à la place du segment");
    }
    let data = match key {
        Some((k, iv)) => decrypt_aes128(&raw, &k, &iv)?,
        None => raw,
    };
    let out = if is_ts {
        strip_fake_header(&data).to_vec()
    } else {
        data
    };
    if out.is_empty() {
        bail!("segment vide après traitement");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Réseau
// ---------------------------------------------------------------------------------------

async fn read_body(
    resp: reqwest::Response,
    meter: Option<&Meter>,
    ctx: &JobCtx,
    got: &mut u64,
) -> Result<Vec<u8>> {
    let mut stream = resp.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        ctx.check()?;
        let chunk = chunk.map_err(|e| anyhow!("lecture interrompue : {e}"))?;
        *got += chunk.len() as u64;
        if let Some(m) = meter {
            m.add(chunk.len() as u64);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Télécharge une ressource (éventuellement une plage d'octets) en mémoire.
pub(crate) async fn fetch_bytes(
    url: &str,
    http: &HttpCtx,
    range: Option<ByteRange>,
    meter: Option<&Meter>,
    ctx: &JobCtx,
) -> Result<Vec<u8>> {
    let req_range = range.map(|r| (r.offset, Some(r.offset + r.length.saturating_sub(1))));
    let resp = get_response(url, http, req_range).await?;
    let partial = resp.status().as_u16() == 206;
    let mut got = 0u64;
    let body = read_body(resp, meter, ctx, &mut got).await;
    let body = match body {
        Ok(b) => b,
        Err(e) => {
            if let Some(m) = meter {
                m.sub(got);
            }
            return Err(e);
        }
    };
    match range {
        // Le serveur a ignoré l'en-tête Range : il a renvoyé tout le fichier
        Some(r) if !partial => {
            let start = r.offset as usize;
            let end = start.saturating_add(r.length as usize);
            if body.len() < end {
                if let Some(m) = meter {
                    m.sub(got);
                }
                bail!("réponse trop courte pour la plage {}@{}", r.length, r.offset);
            }
            Ok(body[start..end].to_vec())
        }
        _ => Ok(body),
    }
}

/// Télécharge un texte (playlist) ; renvoie aussi l'URL finale après redirections.
async fn fetch_text(url: &str, http: &HttpCtx, retries: u32, ctx: &JobCtx) -> Result<(String, String)> {
    with_retries(ctx, retries, || async {
        let resp = get_response(url, http, None).await?;
        let final_url = resp.url().to_string();
        let mut got = 0;
        let body = read_body(resp, None, ctx, &mut got).await?;
        Ok((String::from_utf8_lossy(&body).into_owned(), final_url))
    })
    .await
}

async fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    tokio::fs::write(&part, data).await?;
    tokio::fs::rename(&part, path).await?;
    Ok(())
}

async fn non_empty_len(path: &Path) -> Option<u64> {
    match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() && m.len() > 0 => Some(m.len()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Plan d'une piste
// ---------------------------------------------------------------------------------------

struct TrackPlan {
    info: TrackInfo,
    media: MediaPlaylist,
    init: Option<(MapInfo, PathBuf)>,
}

fn segment_ext(url: &str, has_map: bool) -> &'static str {
    if has_map {
        return "m4s";
    }
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    if [".m4s", ".mp4", ".m4a", ".m4v", ".cmfv", ".cmfa"]
        .iter()
        .any(|e| path.ends_with(e))
    {
        "m4s"
    } else {
        "ts"
    }
}

fn plan_track(kind: TrackKind, playlist_url: String, media: MediaPlaylist, work_dir: &Path) -> Result<TrackPlan> {
    let dir = work_dir.join(kind.dir_name());
    let first_map = media.segments.iter().find_map(|s| s.map.clone());
    if let Some(m0) = &first_map {
        if media.segments.iter().any(|s| s.map.as_ref() != Some(m0)) {
            bail!(
                "piste {} : plusieurs segments d'initialisation (EXT-X-MAP) différents, non pris en charge",
                kind.label()
            );
        }
    }
    let has_map = first_map.is_some();
    let segments = media
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| SegmentInfo {
            index: i as u32,
            sequence: s.sequence,
            url: s.url.clone(),
            path: dir.join(format!("seg_{:06}.{}", i, segment_ext(&s.url, has_map))),
            duration: s.duration,
            discontinuity_before: s.discontinuity_before,
            bytes: 0,
        })
        .collect();
    let init = first_map.map(|m| (m, dir.join("init.mp4")));
    let info = TrackInfo {
        kind,
        playlist_url,
        local_playlist: work_dir.join(format!("{}.m3u8", kind.dir_name())),
        init_segment: init.as_ref().map(|(_, p)| p.clone()),
        segments,
        expected_duration: media.total_duration(),
    };
    Ok(TrackPlan { info, media, init })
}

fn rel_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Playlist locale : mêmes EXTINF et DISCONTINUITY, chemins relatifs au dossier de travail,
/// `#EXT-X-MAP` local si fMP4, aucune `#EXT-X-KEY`, `#EXT-X-ENDLIST`.
pub fn local_playlist_text(track: &TrackInfo, work_dir: &Path) -> String {
    let target = track
        .segments
        .iter()
        .map(|s| s.duration)
        .fold(0.0_f64, f64::max)
        .ceil()
        .max(1.0) as u64;
    let first_seq = track.segments.first().map(|s| s.sequence).unwrap_or(0);
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str(if track.init_segment.is_some() {
        "#EXT-X-VERSION:7\n"
    } else {
        "#EXT-X-VERSION:3\n"
    });
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first_seq}\n"));
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    if let Some(init) = &track.init_segment {
        out.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", rel_path(init, work_dir)));
    }
    for s in &track.segments {
        if s.discontinuity_before {
            out.push_str("#EXT-X-DISCONTINUITY\n");
        }
        out.push_str(&format!("#EXTINF:{:.6},\n{}\n", s.duration, rel_path(&s.path, work_dir)));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

// ---------------------------------------------------------------------------------------
// Téléchargement des segments
// ---------------------------------------------------------------------------------------

struct Shared {
    http: HttpCtx,
    retries: u32,
    ctx: JobCtx,
    meter: Meter,
    keys: tokio::sync::Mutex<HashMap<String, [u8; 16]>>,
}

async fn get_key(sh: &Shared, uri: &str) -> Result<[u8; 16]> {
    let mut keys = sh.keys.lock().await;
    if let Some(k) = keys.get(uri) {
        return Ok(*k);
    }
    let raw = fetch_bytes(uri, &sh.http, None, None, &sh.ctx)
        .await
        .map_err(|e| anyhow!("clé de chiffrement inaccessible : {e:#}"))?;
    let key: [u8; 16] = raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("clé de chiffrement invalide ({} octets au lieu de 16)", raw.len()))?;
    keys.insert(uri.to_string(), key);
    Ok(key)
}

async fn download_init(plan: &TrackPlan, sh: &Shared) -> Result<()> {
    let Some((map, path)) = &plan.init else {
        return Ok(());
    };
    if non_empty_len(path).await.is_some() {
        return Ok(());
    }
    let label = plan.info.kind.label();
    let data = with_retries(&sh.ctx, sh.retries, || async {
        let d = fetch_bytes(&map.url, &sh.http, map.byterange, Some(&sh.meter), &sh.ctx).await?;
        if d.is_empty() {
            bail!("corps de réponse vide");
        }
        if looks_like_html(&d) {
            bail!("page HTML reçue à la place du segment");
        }
        Ok(d)
    })
    .await
    .map_err(|e| {
        if sh.ctx.is_cancelled() {
            anyhow!("Annulé")
        } else {
            anyhow!("Segment d'initialisation ({label}) : {e:#}")
        }
    })?;
    write_atomic(path, &data).await?;
    Ok(())
}

/// Télécharge un segment ; renvoie la taille du fichier final.
async fn run_segment(plan: &TrackPlan, i: usize, sh: &Shared) -> Result<u64> {
    sh.ctx.check()?;
    let seg = &plan.media.segments[i];
    let info = &plan.info.segments[i];
    // Reprise : un segment déjà présent et non vide est conservé
    if let Some(len) = non_empty_len(&info.path).await {
        sh.meter.segment_resumed(len);
        return Ok(len);
    }
    let is_ts = info.path.extension().and_then(|e| e.to_str()) == Some("ts");
    let result = with_retries(&sh.ctx, sh.retries, || async {
        let key = match &seg.key {
            Some(k) => {
                let key = get_key(sh, &k.uri).await?;
                Some((key, k.iv.unwrap_or_else(|| iv_from_sequence(seg.sequence))))
            }
            None => None,
        };
        let raw = fetch_bytes(&seg.url, &sh.http, seg.byterange, Some(&sh.meter), &sh.ctx).await?;
        process_segment(raw, key, is_ts)
    })
    .await;
    match result {
        Ok(data) => {
            write_atomic(&info.path, &data).await.map_err(|e| {
                anyhow!(
                    "Segment {}/{} ({}) : écriture impossible : {e:#}",
                    i + 1,
                    plan.info.segments.len(),
                    plan.info.kind.label()
                )
            })?;
            sh.meter.segment_done(data.len() as u64);
            Ok(data.len() as u64)
        }
        Err(e) => {
            if sh.ctx.is_cancelled() {
                return Err(anyhow!("Annulé"));
            }
            Err(anyhow!(
                "Segment {}/{} ({}) : {e:#}",
                i + 1,
                plan.info.segments.len(),
                plan.info.kind.label()
            ))
        }
    }
}

/// Lance les téléchargements (parallèles) des segments demandés. Renvoie (piste, index, taille).
async fn run_jobs(
    plans: &Arc<Vec<TrackPlan>>,
    jobs: Vec<(usize, usize)>,
    sh: &Arc<Shared>,
    parallel: usize,
) -> Result<Vec<(usize, usize, u64)>> {
    let mut out = Vec::with_capacity(jobs.len());
    let mut s = stream::iter(jobs)
        .map(|(t, i)| {
            let plans = plans.clone();
            let sh = sh.clone();
            async move { run_segment(&plans[t], i, &sh).await.map(|b| (t, i, b)) }
        })
        .buffer_unordered(parallel.max(1));
    while let Some(r) = s.next().await {
        out.push(r?); // première erreur : le flux est abandonné, les requêtes en cours aussi
    }
    Ok(out)
}

async fn load_media(url: &str, http: &HttpCtx, retries: u32, ctx: &JobCtx) -> Result<(MediaPlaylist, String)> {
    let (text, final_url) = fetch_text(url, http, retries, ctx).await?;
    match playlist::parse(&text, &final_url)? {
        Parsed::Media(m) => Ok((m, final_url)),
        Parsed::Master(_) => bail!("playlist maître inattendue à la place d'une playlist média ({final_url})"),
    }
}

pub async fn download(req: &HlsRequest, ctx: &JobCtx) -> Result<HlsDownload> {
    ctx.check()?;
    let retries = req.segment_retries.max(1);
    ctx.progress(Progress {
        phase: Some(Phase::Downloading),
        ..Default::default()
    });
    ctx.info("Lecture de la playlist HLS");
    let (text, final_url) = fetch_text(&req.url, &req.http, retries, ctx).await?;

    let (video_media, video_url, audio, width, height) = match playlist::parse(&text, &final_url)? {
        Parsed::Media(m) => (m, final_url, None, None, None),
        Parsed::Master(master) => {
            let sel = playlist::pick_best(&master).ok_or_else(|| anyhow!("Playlist maître sans variante exploitable"))?;
            let v = &sel.variant;
            ctx.info(format!(
                "Meilleure qualité : {}{}",
                match (v.width, v.height) {
                    (Some(w), Some(h)) => format!("{w}x{h}"),
                    _ => "résolution inconnue".to_string(),
                },
                v.bandwidth.map(|b| format!(" ({} kb/s)", b / 1000)).unwrap_or_default()
            ));
            let (vm, vu) = load_media(&v.url, &req.http, retries, ctx).await?;
            let audio = match &sel.audio_url {
                Some(a) => {
                    ctx.info("Piste audio séparée détectée");
                    Some(load_media(a, &req.http, retries, ctx).await?)
                }
                None => None,
            };
            (vm, vu, audio, v.width, v.height)
        }
    };

    let mut plans = vec![plan_track(TrackKind::Video, video_url, video_media, &req.work_dir)?];
    if let Some((m, u)) = audio {
        plans.push(plan_track(TrackKind::Audio, u, m, &req.work_dir)?);
    }
    for p in &plans {
        tokio::fs::create_dir_all(req.work_dir.join(p.info.kind.dir_name())).await?;
    }

    let total: u32 = plans.iter().map(|p| p.info.segments.len() as u32).sum();
    ctx.info(format!("{total} segments à télécharger"));
    let sh = Arc::new(Shared {
        http: req.http.clone(),
        retries,
        ctx: ctx.clone(),
        meter: Meter::new(ctx, Phase::Downloading, total),
        keys: tokio::sync::Mutex::new(HashMap::new()),
    });
    sh.meter.emit(true);

    for p in &plans {
        download_init(p, &sh).await?;
    }
    let plans = Arc::new(plans);
    let jobs: Vec<(usize, usize)> = plans
        .iter()
        .enumerate()
        .flat_map(|(t, p)| (0..p.info.segments.len()).map(move |i| (t, i)))
        .collect();
    let results = run_jobs(&plans, jobs, &sh, req.parallel).await?;

    let mut tracks: Vec<TrackInfo> = plans.iter().map(|p| p.info.clone()).collect();
    for (t, i, bytes) in results {
        tracks[t].segments[i].bytes = bytes;
    }
    for t in &tracks {
        tokio::fs::write(&t.local_playlist, local_playlist_text(t, &req.work_dir)).await?;
    }
    sh.meter.emit(true);

    let mut it = tracks.into_iter();
    let video = it.next().ok_or_else(|| anyhow!("piste vidéo absente"))?;
    let audio = it.next();
    ctx.info("Téléchargement des segments terminé");
    Ok(HlsDownload {
        work_dir: req.work_dir.clone(),
        video,
        audio,
        width,
        height,
    })
}

/// Supprime et retélécharge les segments `indices` de la piste `kind`.
pub async fn refetch_segments(
    req: &HlsRequest,
    dl: &mut HlsDownload,
    kind: TrackKind,
    indices: &[u32],
    ctx: &JobCtx,
) -> Result<()> {
    ctx.check()?;
    let retries = req.segment_retries.max(1);
    let track = match kind {
        TrackKind::Video => &dl.video,
        TrackKind::Audio => dl
            .audio
            .as_ref()
            .ok_or_else(|| anyhow!("Aucune piste audio à retélécharger"))?,
    };
    let mut idx: Vec<usize> = indices.iter().map(|&i| i as usize).collect();
    idx.sort_unstable();
    idx.dedup();
    if idx.is_empty() {
        return Ok(());
    }
    if let Some(bad) = idx.iter().find(|&&i| i >= track.segments.len()) {
        bail!("Segment {} inexistant dans la piste {}", bad + 1, kind.label());
    }

    // Les clés et plages ne sont pas conservées : on relit la playlist de la piste
    let (media, url) = load_media(&track.playlist_url, &req.http, retries, ctx).await?;
    if media.segments.len() != track.segments.len() {
        bail!(
            "La playlist a changé ({} segments au lieu de {}) : impossible de retélécharger",
            media.segments.len(),
            track.segments.len()
        );
    }
    let plan = plan_track(kind, url, media, &dl.work_dir)?;
    for &i in &idx {
        let path = &track.segments[i].path;
        let _ = tokio::fs::remove_file(path).await;
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let _ = tokio::fs::remove_file(PathBuf::from(part)).await;
    }

    let sh = Arc::new(Shared {
        http: req.http.clone(),
        retries,
        ctx: ctx.clone(),
        meter: Meter::new(ctx, Phase::Repairing, idx.len() as u32),
        keys: tokio::sync::Mutex::new(HashMap::new()),
    });
    sh.meter.emit(true);
    let plans = Arc::new(vec![plan]);
    let jobs = idx.iter().map(|&i| (0usize, i)).collect();
    let results = run_jobs(&plans, jobs, &sh, req.parallel).await?;

    let track = match kind {
        TrackKind::Video => &mut dl.video,
        TrackKind::Audio => dl
            .audio
            .as_mut()
            .ok_or_else(|| anyhow!("Aucune piste audio à retélécharger"))?,
    };
    for (_, i, bytes) in results {
        track.segments[i].bytes = bytes;
        track.segments[i].url = plans[0].info.segments[i].url.clone();
    }
    sh.meter.emit(true);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::testsrv::{self, tempdir, Route};
    use super::*;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};

    fn ts_packets(n: usize, fill: u8) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..n {
            v.push(TS_SYNC);
            v.extend(std::iter::repeat(fill).take(187));
        }
        v
    }

    fn encrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Vec<u8> {
        let mut buf = vec![0u8; data.len() + 16];
        buf[..data.len()].copy_from_slice(data);
        let enc = cbc::Encryptor::<Aes128>::new(key.into(), iv.into());
        let n = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, data.len())
            .unwrap()
            .len();
        buf.truncate(n);
        buf
    }

    fn test_ctx() -> (JobCtx, Arc<Mutex<Vec<Progress>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        let ctx = JobCtx::new(
            tokio_util::sync::CancellationToken::new(),
            |_, _| {},
            move |p| s2.lock().unwrap().push(p),
        );
        (ctx, seen)
    }

    fn request(base: &str, url: &str, dir: &Path) -> HlsRequest {
        HlsRequest {
            url: format!("{base}{url}"),
            http: HttpCtx::default(),
            work_dir: dir.to_path_buf(),
            parallel: 3,
            segment_retries: 2,
        }
    }

    #[test]
    fn hls_strip_fake_header_retire_le_faux_png() {
        let ts = ts_packets(6, 0xAA);
        let mut fake = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xBB, 0xBB, 0xBB, 0xBB, 0xBB];
        let header_len = fake.len();
        fake.extend_from_slice(&ts);
        let out = strip_fake_header(&fake);
        assert_eq!(out.len(), ts.len());
        assert_eq!(out, ts.as_slice());
        assert_eq!(fake.len() - out.len(), header_len);
    }

    #[test]
    fn hls_strip_fake_header_ts_propre_et_non_ts_inchanges() {
        let ts = ts_packets(6, 0xAA);
        assert_eq!(strip_fake_header(&ts).len(), ts.len());
        let mp4: Vec<u8> = (0..2000u32).map(|i| (i % 7) as u8 + 1).collect();
        assert_eq!(strip_fake_header(&mp4), mp4.as_slice());
        let short = [0x47u8; 100];
        assert_eq!(strip_fake_header(&short).len(), 100);
    }

    #[test]
    fn hls_aes128_dechiffre_un_vecteur() {
        let key = *b"0123456789abcdef";
        let iv = [7u8; 16];
        let clear = ts_packets(4, 0x11);
        let enc = encrypt(&clear, &key, &iv);
        assert_ne!(enc, clear);
        assert_eq!(decrypt_aes128(&enc, &key, &iv).unwrap(), clear);
        assert!(decrypt_aes128(&enc[..enc.len() - 1], &key, &iv).is_err());
        // mauvaise clé : padding invalide (très probablement) ou contenu différent
        let wrong = decrypt_aes128(&enc, b"ffffffffffffffff", &iv);
        assert!(wrong.is_err() || wrong.unwrap() != clear);
    }

    #[test]
    fn hls_iv_par_defaut_est_la_sequence_big_endian() {
        let iv = iv_from_sequence(0x0102);
        assert_eq!(&iv[..14], &[0u8; 14]);
        assert_eq!(iv[14], 1);
        assert_eq!(iv[15], 2);
    }

    #[test]
    fn hls_process_segment_rejette_vide_et_html() {
        assert!(process_segment(vec![], None, true).is_err());
        let e = process_segment(b"  <!DOCTYPE html><html>403</html>".to_vec(), None, true).unwrap_err();
        assert!(e.to_string().contains("HTML"));
        assert!(process_segment(ts_packets(5, 1), None, true).is_ok());
    }

    #[test]
    fn hls_playlist_locale() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXT-X-MAP:URI=\"i.mp4\"\n#EXTINF:4.0,\na.m4s\n#EXT-X-DISCONTINUITY\n#EXTINF:2.5,\nb.m4s\n#EXT-X-ENDLIST\n";
        let Parsed::Media(m) = playlist::parse(text, "http://h/p.m3u8").unwrap() else { panic!() };
        let work = PathBuf::from("W");
        let plan = plan_track(TrackKind::Video, "http://h/p.m3u8".into(), m, &work).unwrap();
        assert_eq!(plan.info.init_segment, Some(work.join("video").join("init.mp4")));
        let out = local_playlist_text(&plan.info, &work);
        assert!(!out.contains("EXT-X-KEY"));
        assert!(out.contains("#EXT-X-MAP:URI=\"video/init.mp4\""));
        assert!(out.contains("#EXTINF:4.000000,\nvideo/seg_000000.m4s\n"));
        assert!(out.contains("#EXT-X-DISCONTINUITY\n#EXTINF:2.500000,\nvideo/seg_000001.m4s\n"));
        assert!(out.trim_end().ends_with("#EXT-X-ENDLIST"));
        assert!((plan.info.expected_duration - 6.5).abs() < 1e-9);
    }

    fn media_playlist(n: usize, extra: &str) -> String {
        let mut s = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n");
        s.push_str(extra);
        for i in 0..n {
            s.push_str(&format!("#EXTINF:4.0,\ns{i}.ts\n"));
        }
        s.push_str("#EXT-X-ENDLIST\n");
        s
    }

    #[tokio::test]
    async fn hls_integration_master_audio_reprise_et_nettoyage() {
        let master = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",DEFAULT=YES,URI=\"aud/a.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=640x360,AUDIO=\"a\"\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=9000,RESOLUTION=1920x1080,AUDIO=\"a\"\nhi/v.m3u8\n";
        let ts = ts_packets(6, 0xAA);
        let mut poisoned = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4, 0xBB, 0xBB];
        poisoned.extend_from_slice(&ts);
        let srv = testsrv::start(vec![
            ("/m/master.m3u8", Route::ok(master)),
            ("/m/hi/v.m3u8", Route::ok(media_playlist(3, ""))),
            ("/m/aud/a.m3u8", Route::ok(media_playlist(2, ""))),
            ("/m/hi/s0.ts", Route::ok(ts.clone())),
            ("/m/hi/s1.ts", Route::ok(poisoned)),
            ("/m/hi/s2.ts", Route::ok(ts.clone())),
            ("/m/aud/s0.ts", Route::ok(ts.clone())),
            ("/m/aud/s1.ts", Route::ok(ts.clone())),
        ])
        .await;
        let dir = tempdir("integ");
        let (ctx, seen) = test_ctx();
        let req = request(&srv.base, "/m/master.m3u8", &dir);
        let dl = download(&req, &ctx).await.unwrap();
        assert_eq!(dl.height, Some(1080));
        assert_eq!(dl.video.segments.len(), 3);
        assert_eq!(dl.audio.as_ref().unwrap().segments.len(), 2);
        assert!((dl.video.expected_duration - 12.0).abs() < 1e-9);
        // faux en-tête retiré
        let cleaned = std::fs::read(&dl.video.segments[1].path).unwrap();
        assert_eq!(cleaned, ts);
        assert_eq!(dl.video.segments[1].bytes, ts.len() as u64);
        assert!(dl.video.segments[1].path.ends_with("video/seg_000001.ts") || dl.video.segments[1].path.to_string_lossy().ends_with("seg_000001.ts"));
        // playlists locales
        let pl = std::fs::read_to_string(&dl.video.local_playlist).unwrap();
        assert!(pl.contains("video/seg_000002.ts"));
        assert!(std::fs::read_to_string(dir.join("audio.m3u8")).unwrap().contains("audio/seg_000001.ts"));
        // progression
        let last = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.phase, Some(Phase::Downloading));
        assert_eq!(last.segments_done, Some(5));
        assert_eq!(last.segments_total, Some(5));
        assert!((last.percent - 100.0).abs() < 0.01);
        assert_eq!(srv.hits("/m/hi/s0.ts"), 1);

        // reprise : tout est déjà présent, aucun segment retéléchargé
        let dl2 = download(&req, &ctx).await.unwrap();
        assert_eq!(dl2.video.segments[0].bytes, ts.len() as u64);
        assert_eq!(srv.hits("/m/hi/s0.ts"), 1);
        assert_eq!(srv.hits("/m/aud/s1.ts"), 1);

        // reprise partielle : un segment supprimé est le seul retéléchargé
        std::fs::remove_file(&dl2.video.segments[2].path).unwrap();
        download(&req, &ctx).await.unwrap();
        assert_eq!(srv.hits("/m/hi/s2.ts"), 2);
        assert_eq!(srv.hits("/m/hi/s0.ts"), 1);

        // refetch ciblé
        let mut dl3 = dl2.clone();
        refetch_segments(&req, &mut dl3, TrackKind::Video, &[0, 0, 1], &ctx).await.unwrap();
        assert_eq!(srv.hits("/m/hi/s0.ts"), 2);
        assert_eq!(srv.hits("/m/hi/s1.ts"), 2);
        assert_eq!(srv.hits("/m/hi/s2.ts"), 2);
        assert_eq!(dl3.video.segments[1].bytes, ts.len() as u64);
        assert!(refetch_segments(&req, &mut dl3, TrackKind::Video, &[9], &ctx).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_integration_segment_manquant_fait_echouer() {
        let ts = ts_packets(6, 0xAA);
        let srv = testsrv::start(vec![
            ("/v.m3u8", Route::ok(media_playlist(3, ""))),
            ("/s0.ts", Route::ok(ts.clone())),
            // s1.ts absent -> 404
            ("/s2.ts", Route::ok(ts.clone())),
        ])
        .await;
        let dir = tempdir("missing");
        let (ctx, _) = test_ctx();
        let req = request(&srv.base, "/v.m3u8", &dir);
        let err = download(&req, &ctx).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Segment 2/3"), "{msg}");
        assert!(msg.contains("404"), "{msg}");
        assert_eq!(srv.hits("/s1.ts"), 2); // 2 tentatives
        // une fois le segment disponible, la reprise termine sans retélécharger les autres
        srv.set("/s1.ts", Route::ok(ts.clone()));
        let dl = download(&req, &ctx).await.unwrap();
        assert_eq!(dl.video.segments.len(), 3);
        assert_eq!(srv.hits("/s0.ts"), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_integration_page_html_a_la_place_du_segment() {
        let ts = ts_packets(6, 0xAA);
        let html = Route {
            status: 200,
            content_type: "text/html",
            body: b"<html><body>Forbidden</body></html>".to_vec(),
        };
        let srv = testsrv::start(vec![
            ("/v.m3u8", Route::ok(media_playlist(2, ""))),
            ("/s0.ts", Route::ok(ts)),
            ("/s1.ts", html),
        ])
        .await;
        let dir = tempdir("html");
        let (ctx, _) = test_ctx();
        let err = download(&request(&srv.base, "/v.m3u8", &dir), &ctx).await.unwrap_err();
        assert!(format!("{err:#}").contains("Segment 2/2"));
        assert!(format!("{err:#}").contains("HTML"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_integration_aes128_iv_explicite_et_sequence() {
        let key = *b"sixteen byte key";
        let ts0 = ts_packets(6, 0x21);
        let ts1 = ts_packets(7, 0x22);
        let iv0 = [3u8; 16];
        let iv1 = iv_from_sequence(11); // séquence 10 + index 1
        let pl = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x03030303030303030303030303030303\n\
#EXTINF:4.0,\ns0.ts\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
#EXTINF:4.0,\ns1.ts\n#EXT-X-ENDLIST\n";
        let srv = testsrv::start(vec![
            ("/v.m3u8", Route::ok(pl)),
            ("/key.bin", Route::ok(key.to_vec())),
            ("/s0.ts", Route::ok(encrypt(&ts0, &key, &iv0))),
            ("/s1.ts", Route::ok(encrypt(&ts1, &key, &iv1))),
        ])
        .await;
        let dir = tempdir("aes");
        let (ctx, _) = test_ctx();
        let dl = download(&request(&srv.base, "/v.m3u8", &dir), &ctx).await.unwrap();
        assert_eq!(std::fs::read(&dl.video.segments[0].path).unwrap(), ts0);
        assert_eq!(std::fs::read(&dl.video.segments[1].path).unwrap(), ts1);
        assert_eq!(srv.hits("/key.bin"), 1, "la clé n'est téléchargée qu'une fois");
        assert!(!std::fs::read_to_string(&dl.video.local_playlist).unwrap().contains("KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_integration_fmp4_init_et_byterange() {
        let init = b"INITDATA-INITDATA".to_vec();
        let media: Vec<u8> = (0..300u32).map(|i| (i % 250) as u8 + 1).collect();
        let pl = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n\
#EXTINF:4.0,\n#EXT-X-BYTERANGE:100@0\nmedia.m4s\n\
#EXTINF:4.0,\n#EXT-X-BYTERANGE:200\nmedia.m4s\n#EXT-X-ENDLIST\n";
        let srv = testsrv::start(vec![
            ("/v.m3u8", Route::ok(pl)),
            ("/init.mp4", Route::ok(init.clone())),
            ("/media.m4s", Route::ok(media.clone())),
        ])
        .await;
        let dir = tempdir("fmp4");
        let (ctx, _) = test_ctx();
        let dl = download(&request(&srv.base, "/v.m3u8", &dir), &ctx).await.unwrap();
        assert_eq!(std::fs::read(dl.video.init_segment.as_ref().unwrap()).unwrap(), init);
        assert_eq!(std::fs::read(&dl.video.segments[0].path).unwrap(), &media[..100]);
        assert_eq!(std::fs::read(&dl.video.segments[1].path).unwrap(), &media[100..300]);
        assert!(dl.video.segments[0].path.to_string_lossy().ends_with(".m4s"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hls_integration_rejette_direct() {
        let pl = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\ns0.ts\n";
        let srv = testsrv::start(vec![("/v.m3u8", Route::ok(pl))]).await;
        let dir = tempdir("live");
        let (ctx, _) = test_ctx();
        let err = download(&request(&srv.base, "/v.m3u8", &dir), &ctx).await.unwrap_err();
        assert_eq!(err.to_string(), "Flux en direct non pris en charge");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
