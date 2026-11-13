//! File d'attente : un seul téléchargement à la fois, persistante, pause globale.
//! Commandes `queue_*`, réglages, outils, journal ; événements `queue-update`,
//! `download-progress` (4/s max) et `log`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::db::{now_ms, Db, Download, DownloadStatus};
use crate::detect::{self, BrowserStatus, MediaKind, ResolveHint, ResolvedMedia};
use crate::hls::{self, HlsDownload, HlsRequest, TrackKind};
use crate::model::{JobCtx, LogLevel, Phase, Progress};
use crate::settings::Settings;
use crate::tools::{self, Tool};
use crate::verify::{self, GapReport};
use crate::ytdlp;

const LOG_CAP: usize = 500;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const RETRY_DELAY: Duration = Duration::from_secs(3);
/// Tours de réparation de segments au maximum.
const MAX_REPAIR_ROUNDS: usize = 2;
const SEGMENT_RETRIES: u32 = 3;

// ── Types exposés au frontend ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueState {
    pub paused: bool,
    pub items: Vec<Download>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewItem {
    pub url: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub available: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsStatus {
    pub ffmpeg: ToolStatus,
    pub ffprobe: ToolStatus,
    pub ytdlp: ToolStatus,
    pub browser: BrowserStatus,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub id: i64,
    pub phase: Option<Phase>,
    pub progress_percent: f32,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: Option<u64>,
    pub eta_seconds: Option<u64>,
    pub segments_done: Option<u32>,
    pub segments_total: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub ts: i64,
    pub level: LogLevel,
    pub download_id: Option<i64>,
    pub message: String,
}

// ── Noms de fichiers ───────────────────────────────────────────────────────

/// Nettoie un nom de fichier pour Windows (portage de `sanitizeFilename` de l'ancien serveur).
pub fn sanitize_filename(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    let cut: String = replaced
        .trim_end_matches(['.', ' '])
        .chars()
        .take(150)
        .collect();
    let cleaned = cut.trim().trim_end_matches(['.', ' ']).to_string();
    // Noms réservés de Windows (CON, NUL, COM1…)
    let base = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && base.ends_with(|c: char| c.is_ascii_digit()));
    if reserved {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Nom par défaut dérivé de l'URL (dernier segment sans extension), sinon `video-<timestamp>`.
pub fn default_filename(url: &str) -> String {
    if let Ok(parsed) = url::Url::parse(url) {
        if let Some(seg) = parsed
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
        {
            let decoded = urlencoding::decode(seg)
                .map(|c| c.into_owned())
                .unwrap_or_else(|_| seg.to_string());
            let without_ext = match decoded.rfind('.') {
                Some(i)
                    if i > 0
                        && decoded[i + 1..].chars().all(|c| c.is_ascii_alphanumeric())
                        && decoded.len() > i + 1 =>
                {
                    decoded[..i].to_string()
                }
                _ => decoded,
            };
            let name = sanitize_filename(&without_ext);
            if !name.is_empty() {
                return name;
            }
        }
    }
    format!("video-{}", now_ms())
}

fn stem_taken(dir: &Path, stem: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|e| {
        let path = e.path();
        let ext = path
            .extension()
            .map(|x| x.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "part" | "ytdl" | "temp" | "tmp") {
            return false;
        }
        path.file_stem()
            .map(|s| s.to_string_lossy().eq_ignore_ascii_case(stem))
            .unwrap_or(false)
    })
}

/// Premier nom libre dans `dir` : `base`, puis `base_2`, `base_3`…
/// (un nom est pris dès qu'un fichier porte ce nom, quelle que soit l'extension).
pub fn unique_stem(dir: &Path, base: &str) -> String {
    let base = if base.is_empty() { "video" } else { base };
    let mut n = 1u32;
    loop {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            format!("{base}_{n}")
        };
        if !stem_taken(dir, &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Extension du fichier final d'un téléchargement direct : celle de l'URL si c'est une vidéo, sinon mp4.
fn file_ext_from_url(url: &str) -> &'static str {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_default();
    for ext in ["mp4", "mkv", "webm", "mov", "m4v", "avi", "flv", "ts"] {
        if path.ends_with(&format!(".{ext}")) {
            return ext;
        }
    }
    "mp4"
}

/// Valide et prépare un élément ajouté : `(url, nom, nom personnalisé, referer)`.
pub fn prepare_item(item: &NewItem) -> Option<(String, String, bool, Option<String>)> {
    let url = item.url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return None;
    }
    url::Url::parse(url).ok()?;
    let custom = sanitize_filename(item.filename.as_deref().unwrap_or("").trim());
    let is_custom = !custom.is_empty();
    let filename = if is_custom {
        custom
    } else {
        default_filename(url)
    };
    let referer = item
        .referer
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string);
    Some((url.to_string(), filename, is_custom, referer))
}

/// Un 403 signale un jeton de lien mort : inutile d'insister.
pub fn is_403(message: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b403\b|forbidden").expect("regex valide"))
        .is_match(message)
}

/// Nombre maximum de tentatives : 3 au plus en cas de 403.
pub fn max_attempts(message: &str, max_retries: u32) -> u32 {
    if is_403(message) {
        max_retries.min(3)
    } else {
        max_retries
    }
}

const HINT_403: &str = "Erreur 403 : lien expiré — ces liens signés ne vivent que quelques heures. Récupérez un lien frais, ou collez plutôt le lien de la page du lecteur : il sera résolu au moment du téléchargement";

// ── État interne ───────────────────────────────────────────────────────────

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Inner {
    app: AppHandle,
    db: Db,
    settings: Mutex<Settings>,
    wake: Notify,
    /// Téléchargement en cours : id et jeton d'annulation.
    current: Mutex<Option<(i64, CancellationToken)>>,
    logs: Mutex<VecDeque<LogLine>>,
    /// Arrêt de l'application : rien ne démarre, l'élément interrompu repart en `pending`.
    shutting_down: AtomicBool,
    work_root: PathBuf,
}

/// État géré par Tauri.
#[derive(Clone)]
pub struct Queue(Arc<Inner>);

#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    phase: Option<Phase>,
}

impl Inner {
    fn settings(&self) -> Settings {
        lock(&self.settings).clone()
    }

    fn snapshot(&self) -> anyhow::Result<QueueState> {
        Ok(QueueState {
            paused: self.db.is_paused(),
            items: self.db.all()?,
        })
    }

    fn emit_queue(&self) {
        if let Ok(state) = self.snapshot() {
            let _ = self.app.emit("queue-update", state);
        }
    }

    fn log(&self, level: LogLevel, download_id: Option<i64>, message: String) {
        let line = LogLine {
            ts: now_ms(),
            level,
            download_id,
            message,
        };
        {
            let mut logs = lock(&self.logs);
            logs.push_back(line.clone());
            while logs.len() > LOG_CAP {
                logs.pop_front();
            }
        }
        let _ = self.app.emit("log", line);
    }

    /// Persiste et émet la progression : au plus 4 fois par seconde (changement de phase immédiat).
    fn on_progress(&self, id: i64, throttle: &Mutex<Throttle>, p: Progress) {
        let now = Instant::now();
        {
            let mut t = lock(throttle);
            let due = t
                .last
                .map_or(true, |l| now.duration_since(l) >= PROGRESS_INTERVAL);
            if !(due || t.phase != p.phase) {
                return;
            }
            t.last = Some(now);
            t.phase = p.phase;
        }
        let p = Progress {
            percent: p.percent.clamp(0.0, 100.0),
            ..p
        };
        let _ = self.db.update_progress(id, &p);
        let _ = self.app.emit(
            "download-progress",
            ProgressEvent {
                id,
                phase: p.phase,
                progress_percent: p.percent,
                downloaded_bytes: p.downloaded_bytes,
                total_bytes: p.total_bytes,
                speed_bps: p.speed_bps,
                eta_seconds: p.eta_seconds,
                segments_done: p.segments_done,
                segments_total: p.segments_total,
            },
        );
    }

    /// Prévient l'interface que le navigateur visible attend (ou n'attend plus) l'utilisateur.
    fn emit_assist(&self, id: i64, active: bool) {
        let _ = self.app.emit("detect-assist", json!({ "id": id, "active": active }));
    }

    fn job_ctx(self: &Arc<Self>, id: i64, cancel: CancellationToken) -> JobCtx {
        let for_log = self.clone();
        let for_progress = self.clone();
        let for_assist = self.clone();
        let throttle = Arc::new(Mutex::new(Throttle::default()));
        JobCtx::new(
            cancel,
            move |level, msg| for_log.log(level, Some(id), msg),
            move |p| for_progress.on_progress(id, &throttle, p),
        )
        .with_assist(move |active| for_assist.emit_assist(id, active))
    }
}

pub fn init(app: &AppHandle) -> anyhow::Result<()> {
    let data_dir = app
        .path()
        .app_data_dir()
        .context("dossier de données introuvable")?;
    std::fs::create_dir_all(&data_dir)?;
    let db = Db::open(&data_dir.join("lucamedia.db"))?;
    let resumed = db.reset_running().unwrap_or(0);
    let settings = Settings::load(&db);
    let dest_error = settings.ensure_dest_dir().err();
    let inner = Arc::new(Inner {
        app: app.clone(),
        db,
        settings: Mutex::new(settings),
        wake: Notify::new(),
        current: Mutex::new(None),
        logs: Mutex::new(VecDeque::new()),
        shutting_down: AtomicBool::new(false),
        work_root: data_dir.join("work"),
    });
    for tool in [Tool::YtDlp, Tool::Ffmpeg, Tool::Ffprobe] {
        if tools::tool_path(tool).is_none() {
            inner.log(
                LogLevel::Error,
                None,
                format!("{} introuvable : certains téléchargements échoueront", tool.label()),
            );
        }
    }
    if let Some(e) = dest_error {
        inner.log(
            LogLevel::Warn,
            None,
            format!("Création du dossier de destination impossible : {e}"),
        );
    }
    if resumed > 0 {
        inner.log(
            LogLevel::Info,
            None,
            format!("{resumed} téléchargement(s) interrompu(s) remis en attente"),
        );
    }
    app.manage(Queue(inner.clone()));
    tauri::async_runtime::spawn(runner(inner));
    Ok(())
}

// ── Boucle principale ──────────────────────────────────────────────────────

async fn runner(q: Arc<Inner>) {
    loop {
        let next = if q.shutting_down.load(Ordering::SeqCst) || q.db.is_paused() {
            None
        } else {
            q.db.next_pending().ok().flatten()
        };
        match next {
            Some(dl) => run_one(&q, dl).await,
            None => q.wake.notified().await,
        }
    }
}

/// Fin d'un téléchargement.
enum End {
    Done(Outcome),
    Cancelled,
    Failed(String),
}

/// Résultat d'une tentative réussie.
struct Outcome {
    output: PathBuf,
    report: Option<GapReport>,
    /// La vérification n'a pas pu s'exécuter.
    verify_error: Option<String>,
}

impl Outcome {
    fn status(&self) -> DownloadStatus {
        match (&self.report, &self.verify_error) {
            (_, Some(_)) => DownloadStatus::Warning,
            (Some(r), None) if !r.ok => DownloadStatus::Warning,
            _ => DownloadStatus::Done,
        }
    }
}

/// État conservé entre les tentatives d'un même téléchargement.
#[derive(Default)]
struct RunState {
    /// Nom de fichier réservé (sans extension), réutilisé aux tentatives suivantes.
    stem: Option<String>,
}

async fn run_one(q: &Arc<Inner>, dl: Download) {
    let id = dl.id;
    let cancel = CancellationToken::new();
    *lock(&q.current) = Some((id, cancel.clone()));
    if let Err(e) = q.db.mark_running(id) {
        q.log(LogLevel::Error, Some(id), format!("{e:#}"));
        *lock(&q.current) = None;
        return;
    }
    q.emit_queue();
    let ctx = q.job_ctx(id, cancel.clone());
    ctx.info(format!("Démarrage : {}", dl.url));

    let mut run = RunState::default();
    let mut attempt = dl.retry_count;
    let end = loop {
        let settings = q.settings();
        match process(q, id, &settings, &ctx, &mut run).await {
            Ok(outcome) => break End::Done(outcome),
            Err(e) => {
                if cancel.is_cancelled() {
                    break End::Cancelled;
                }
                let message = format!("{e:#}");
                attempt += 1;
                // Fenêtre assistée fermée ou délai écoulé : la rouvrir à chaque tentative n'aurait pas de sens.
                let limit = if detect::is_assist_failure(&message) {
                    attempt
                } else {
                    max_attempts(&message, settings.max_retries)
                };
                if attempt >= limit {
                    ctx.error(format!("Échec définitif après {attempt} tentative(s) : {message}"));
                    let full = if is_403(&message) {
                        ctx.warn(HINT_403);
                        format!("{message}\n{HINT_403}")
                    } else {
                        message
                    };
                    break End::Failed(full);
                }
                ctx.warn(format!(
                    "Échec ({message}) — nouvelle tentative {}/{limit} dans {} s",
                    attempt + 1,
                    RETRY_DELAY.as_secs()
                ));
                let _ = q.db.set_retry(id, attempt, &message);
                q.emit_queue();
                tokio::select! {
                    _ = cancel.cancelled() => break End::Cancelled,
                    _ = tokio::time::sleep(RETRY_DELAY) => {}
                }
            }
        }
    };

    let work_dir = q.work_root.join(id.to_string());
    let result = match end {
        End::Done(o) => {
            let status = o.status();
            let output = o.output.to_string_lossy().into_owned();
            let summary = o.report.as_ref().map(|r| r.summary.clone());
            match status {
                DownloadStatus::Done => ctx.info(format!("Terminé : {output}")),
                _ => ctx.warn(format!(
                    "Terminé avec avertissement : {}",
                    o.verify_error.clone().or(summary).unwrap_or_default()
                )),
            }
            let r = q.db.finish(
                id,
                status,
                o.verify_error.as_deref(),
                Some(&output),
                o.report.as_ref(),
            );
            let _ = tokio::fs::remove_dir_all(&work_dir).await;
            r
        }
        End::Cancelled if q.shutting_down.load(Ordering::SeqCst) => q.db.requeue(id),
        End::Cancelled => {
            ctx.warn("Téléchargement annulé");
            q.db.finish(id, DownloadStatus::Cancelled, None, None, None)
        }
        End::Failed(message) => q.db.finish(id, DownloadStatus::Error, Some(&message), None, None),
    };
    if let Err(e) = result {
        q.log(LogLevel::Error, Some(id), format!("Enregistrement du statut : {e:#}"));
    }
    *lock(&q.current) = None;
    q.emit_queue();
}

// ── Pipeline d'un téléchargement ───────────────────────────────────────────

fn phase(ctx: &JobCtx, phase: Phase) {
    ctx.progress(Progress {
        phase: Some(phase),
        ..Progress::default()
    });
}

async fn process(
    q: &Arc<Inner>,
    id: i64,
    s: &Settings,
    ctx: &JobCtx,
    run: &mut RunState,
) -> anyhow::Result<Outcome> {
    ctx.check()?;
    let dl = q
        .db
        .get(id)?
        .ok_or_else(|| anyhow!("Téléchargement supprimé"))?;

    // 1. Résolution
    phase(ctx, Phase::Resolving);
    let hint = ResolveHint {
        referer: dl.referer.clone(),
        browser: s.browser,
        show_browser: s.show_browser,
    };
    let media = detect::resolve(&dl.url, &hint, ctx).await?;
    ctx.check()?;
    ctx.info(format!("Média résolu : {:?} via {:?}", media.kind, media.via));
    let quality = media.height.map(|h| format!("{h}p"));
    q.db
        .set_media_info(id, Some(media.via), Some(media.kind), quality.as_deref())?;

    // 2. Nom et destination
    let mut filename = dl.filename.clone();
    if !dl.name_is_custom {
        if let Some(title) = media
            .title
            .as_deref()
            .map(sanitize_filename)
            .filter(|t| !t.is_empty())
        {
            filename = title;
        }
    }
    let dest = PathBuf::from(&s.dest_dir);
    tokio::fs::create_dir_all(&dest)
        .await
        .with_context(|| format!("création du dossier {}", dest.display()))?;
    let stem = match &run.stem {
        Some(stem) => stem.clone(),
        None => {
            let stem = unique_stem(&dest, &filename);
            run.stem = Some(stem.clone());
            stem
        }
    };
    if stem != dl.filename {
        let _ = q.db.set_filename(id, &stem);
        q.emit_queue();
    }

    // 3. Téléchargement et vérification
    match media.kind {
        MediaKind::Hls => {
            let output = dest.join(format!("{stem}.mp4"));
            let work_dir = q.work_root.join(id.to_string());
            process_hls(q, id, s, ctx, &media, &work_dir, &output).await
        }
        MediaKind::File => {
            let output = dest.join(format!("{stem}.{}", file_ext_from_url(&media.url)));
            let _ = tokio::fs::remove_file(&output).await;
            phase(ctx, Phase::Downloading);
            hls::file::download_file(&media.url, &media.http, &output, ctx).await?;
            finish_with_verification(s, ctx, output, media.duration, None).await
        }
        MediaKind::Dash | MediaKind::Ytdlp => {
            phase(ctx, Phase::Downloading);
            let output = ytdlp::download(&media, &dest.join(&stem), ctx).await?;
            finish_with_verification(s, ctx, output, media.duration, None).await
        }
    }
}

/// Vérifie le fichier final (si activé). Un échec de la vérification elle-même n'invalide pas le fichier.
async fn finish_with_verification(
    s: &Settings,
    ctx: &JobCtx,
    output: PathBuf,
    expected: Option<f64>,
    tracks: Option<&HlsDownload>,
) -> anyhow::Result<Outcome> {
    if !s.verify_after_download {
        return Ok(Outcome {
            output,
            report: None,
            verify_error: None,
        });
    }
    phase(ctx, Phase::Verifying);
    match verify::verify_output(&output, expected, tracks, ctx).await {
        Ok(report) => Ok(Outcome {
            output,
            report: Some(report),
            verify_error: None,
        }),
        Err(e) if ctx.is_cancelled() => Err(e),
        Err(e) => {
            let msg = format!("Vérification impossible : {e:#}");
            ctx.warn(msg.clone());
            Ok(Outcome {
                output,
                report: None,
                verify_error: Some(msg),
            })
        }
    }
}

/// Regroupe les indices de segments d'une piste (triés, sans doublon).
pub fn group_indices(items: &[(TrackKind, u32)], kind: TrackKind) -> Vec<u32> {
    let mut idx: Vec<u32> = items
        .iter()
        .filter(|(k, _)| *k == kind)
        .map(|(_, i)| *i)
        .collect();
    idx.sort_unstable();
    idx.dedup();
    idx
}

async fn refetch_items(
    req: &HlsRequest,
    dl: &mut HlsDownload,
    items: &[(TrackKind, u32)],
    ctx: &JobCtx,
) -> anyhow::Result<()> {
    for kind in [TrackKind::Video, TrackKind::Audio] {
        let idx = group_indices(items, kind);
        if !idx.is_empty() {
            ctx.info(format!("Re-téléchargement de {} segment(s) {kind:?}", idx.len()));
            hls::refetch_segments(req, dl, kind, &idx, ctx).await?;
        }
    }
    Ok(())
}

/// Vérifie les segments téléchargés et répare les fautifs (2 tours au plus).
async fn verify_and_repair_segments(
    req: &HlsRequest,
    dl: &mut HlsDownload,
    auto_repair: bool,
    ctx: &JobCtx,
) -> anyhow::Result<Vec<verify::BadSegment>> {
    for round in 0..=MAX_REPAIR_ROUNDS {
        ctx.check()?;
        phase(ctx, Phase::Verifying);
        let mut bad = Vec::new();
        let checked: anyhow::Result<()> = async {
            bad.extend(verify::verify_segments(&dl.video, ctx).await?);
            if let Some(audio) = &dl.audio {
                bad.extend(verify::verify_segments(audio, ctx).await?);
            }
            Ok(())
        }
        .await;
        if let Err(e) = checked {
            if ctx.is_cancelled() {
                return Err(e);
            }
            ctx.warn(format!("Vérification des segments impossible : {e:#}"));
            return Ok(Vec::new());
        }
        if bad.is_empty() {
            return Ok(bad);
        }
        ctx.warn(format!("{} segment(s) fautif(s) détecté(s)", bad.len()));
        if !auto_repair || round == MAX_REPAIR_ROUNDS {
            return Ok(bad);
        }
        phase(ctx, Phase::Repairing);
        let items: Vec<(TrackKind, u32)> = bad.iter().map(|b| (b.track, b.index)).collect();
        refetch_items(req, dl, &items, ctx).await?;
    }
    Ok(Vec::new())
}

async fn process_hls(
    q: &Arc<Inner>,
    id: i64,
    s: &Settings,
    ctx: &JobCtx,
    media: &ResolvedMedia,
    work_dir: &Path,
    output: &Path,
) -> anyhow::Result<Outcome> {
    let req = HlsRequest {
        url: media.url.clone(),
        http: media.http.clone(),
        work_dir: work_dir.to_path_buf(),
        parallel: s.parallel_segments as usize,
        segment_retries: SEGMENT_RETRIES,
    };
    phase(ctx, Phase::Downloading);
    let mut dl = hls::download(&req, ctx).await?;
    if let (Some(w), Some(h)) = (dl.width, dl.height) {
        let _ = q.db.set_quality(id, &format!("{w}x{h}"));
    }

    // Segments restés fautifs après réparation : reportés dans le rapport final.
    let remaining_bad = if s.verify_after_download {
        verify_and_repair_segments(&req, &mut dl, s.auto_repair, ctx).await?
    } else {
        Vec::new()
    };

    ctx.check()?;
    phase(ctx, Phase::Assembling);
    let _ = tokio::fs::remove_file(output).await;
    hls::assemble(&dl, output, ctx).await?;

    if !s.verify_after_download {
        return Ok(Outcome {
            output: output.to_path_buf(),
            report: None,
            verify_error: None,
        });
    }

    let expected = if dl.video.expected_duration > 0.0 {
        Some(dl.video.expected_duration)
    } else {
        media.duration
    };
    let mut outcome =
        finish_with_verification(s, ctx, output.to_path_buf(), expected, Some(&dl)).await?;

    // Trous localisés dans des segments : un tour de réparation + réassemblage.
    let targets: Vec<(TrackKind, u32)> = outcome
        .report
        .as_ref()
        .filter(|r| !r.ok)
        .map(|r| {
            r.gaps
                .iter()
                // Sans piste audio séparée, l'audio vit dans les segments vidéo.
                .filter_map(|g| {
                    let kind = if dl.audio.is_some() { g.stream } else { TrackKind::Video };
                    g.segment_index.map(|i| (kind, i))
                })
                .collect()
        })
        .unwrap_or_default();
    if s.auto_repair && !targets.is_empty() {
        phase(ctx, Phase::Repairing);
        // Réparation au mieux : si elle échoue, on garde le fichier déjà assemblé et son rapport.
        match refetch_items(&req, &mut dl, &targets, ctx).await {
            Ok(()) => {
                ctx.check()?;
                phase(ctx, Phase::Assembling);
                let _ = tokio::fs::remove_file(output).await;
                hls::assemble(&dl, output, ctx).await?;
                outcome =
                    finish_with_verification(s, ctx, output.to_path_buf(), expected, Some(&dl)).await?;
            }
            Err(e) if ctx.is_cancelled() => return Err(e),
            Err(e) => ctx.warn(format!("Réparation impossible : {e:#}")),
        }
    }
    if let Some(report) = outcome.report.as_mut() {
        report.bad_segments = remaining_bad;
    }
    Ok(outcome)
}

// ── Commandes ──────────────────────────────────────────────────────────────

fn err<E: std::fmt::Display>(e: E) -> String {
    format!("{e:#}")
}

#[tauri::command]
pub async fn get_settings(q: State<'_, Queue>) -> Result<Settings, String> {
    Ok(q.0.settings())
}

#[tauri::command]
pub async fn save_settings(q: State<'_, Queue>, settings: Settings) -> Result<Settings, String> {
    let s = settings.normalized();
    s.ensure_dest_dir()
        .map_err(|e| format!("Dossier de destination invalide : {e}"))?;
    s.save(&q.0.db).map_err(err)?;
    *lock(&q.0.settings) = s.clone();
    Ok(s)
}

#[tauri::command]
pub async fn queue_state(q: State<'_, Queue>) -> Result<QueueState, String> {
    q.0.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_add(q: State<'_, Queue>, items: Vec<NewItem>) -> Result<QueueState, String> {
    let inner = &q.0;
    let mut added = 0usize;
    for item in &items {
        match prepare_item(item) {
            Some((url, filename, custom, referer)) => {
                inner
                    .db
                    .add(&url, &filename, custom, referer.as_deref())
                    .map_err(err)?;
                added += 1;
            }
            None => inner.log(
                LogLevel::Warn,
                None,
                format!("URL ignorée (http:// ou https:// requis) : {}", item.url.trim()),
            ),
        }
    }
    if added == 0 {
        return Err("Aucune URL valide : seules les adresses http:// et https:// sont acceptées".into());
    }
    inner.log(LogLevel::Info, None, format!("{added} élément(s) ajouté(s) à la file"));
    inner.emit_queue();
    inner.wake.notify_one();
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_remove(q: State<'_, Queue>, id: i64) -> Result<QueueState, String> {
    let inner = &q.0;
    let dl = inner
        .db
        .get(id)
        .map_err(err)?
        .ok_or("Élément introuvable")?;
    if dl.status == DownloadStatus::Running {
        return Err("Téléchargement en cours : annulez-le avant de le retirer".into());
    }
    inner.db.delete(id).map_err(err)?;
    let _ = tokio::fs::remove_dir_all(inner.work_root.join(id.to_string())).await;
    inner.emit_queue();
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_retry(q: State<'_, Queue>, id: i64) -> Result<QueueState, String> {
    let inner = &q.0;
    let dl = inner
        .db
        .get(id)
        .map_err(err)?
        .ok_or("Élément introuvable")?;
    if matches!(dl.status, DownloadStatus::Running | DownloadStatus::Pending) {
        return Err("Cet élément est déjà en file ou en cours".into());
    }
    inner.db.reset_for_retry(id).map_err(err)?;
    inner.emit_queue();
    inner.wake.notify_one();
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_cancel(q: State<'_, Queue>, id: i64) -> Result<QueueState, String> {
    let inner = &q.0;
    let dl = inner
        .db
        .get(id)
        .map_err(err)?
        .ok_or("Élément introuvable")?;
    match dl.status {
        DownloadStatus::Pending => {
            inner
                .db
                .finish(id, DownloadStatus::Cancelled, None, None, None)
                .map_err(err)?;
            inner.emit_queue();
        }
        DownloadStatus::Running => {
            // Le runner passe le statut à « cancelled » à la fin de la tâche.
            if let Some((cur, token)) = lock(&inner.current).as_ref() {
                if *cur == id {
                    token.cancel();
                }
            }
        }
        _ => {}
    }
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_clear_finished(q: State<'_, Queue>) -> Result<QueueState, String> {
    let inner = &q.0;
    inner.db.clear_finished().map_err(err)?;
    inner.emit_queue();
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn queue_set_paused(q: State<'_, Queue>, paused: bool) -> Result<QueueState, String> {
    let inner = &q.0;
    inner.db.set_paused(paused).map_err(err)?;
    inner.log(
        LogLevel::Info,
        None,
        if paused { "File en pause" } else { "File relancée" }.to_string(),
    );
    inner.emit_queue();
    if !paused {
        inner.wake.notify_one();
    }
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn verify_again(q: State<'_, Queue>, id: i64) -> Result<QueueState, String> {
    let inner = q.0.clone();
    let dl = inner
        .db
        .get(id)
        .map_err(err)?
        .ok_or("Élément introuvable")?;
    if matches!(dl.status, DownloadStatus::Running | DownloadStatus::Pending) {
        return Err("Cet élément est en file ou en cours de téléchargement".into());
    }
    let path = dl.output_path.clone().ok_or("Aucun fichier à vérifier")?;
    if !Path::new(&path).is_file() {
        return Err(format!("Fichier introuvable : {path}"));
    }
    let ctx = inner.job_ctx(id, CancellationToken::new());
    ctx.info(format!("Nouvelle vérification : {path}"));
    let expected = dl.report.as_ref().and_then(|r| r.expected_duration);
    let report = verify::verify_output(Path::new(&path), expected, None, &ctx)
        .await
        .map_err(err)?;
    let status = if report.ok {
        DownloadStatus::Done
    } else {
        DownloadStatus::Warning
    };
    ctx.info(report.summary.clone());
    inner.db.set_report(id, status, &report).map_err(err)?;
    inner.emit_queue();
    inner.snapshot().map_err(err)
}

#[tauri::command]
pub async fn reveal_download(q: State<'_, Queue>, id: i64) -> Result<(), String> {
    let inner = &q.0;
    let dl = inner
        .db
        .get(id)
        .map_err(err)?
        .ok_or("Élément introuvable")?;
    if let Some(path) = dl.output_path.as_deref().map(PathBuf::from).filter(|p| p.is_file()) {
        return tauri_plugin_opener::reveal_item_in_dir(path).map_err(err);
    }
    open_dir(&inner.settings().dest_dir)
}

#[tauri::command]
pub async fn open_dest_dir(q: State<'_, Queue>) -> Result<(), String> {
    open_dir(&q.0.settings().dest_dir)
}

fn open_dir(dir: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Dossier de destination inaccessible : {e}"))?;
    tauri_plugin_opener::open_path(dir, None::<&str>).map_err(err)
}

/// Extrait le numéro de version de la sortie de `-version` / `--version`.
pub fn parse_tool_version(tool: Tool, stdout: &str) -> Option<String> {
    let first = stdout.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    match tool {
        Tool::YtDlp => Some(first.to_string()),
        Tool::Ffmpeg | Tool::Ffprobe => first
            .split_once(" version ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .map(str::to_string)
            .or_else(|| Some(first.to_string())),
    }
}

async fn tool_status(tool: Tool) -> ToolStatus {
    let Some(path) = tools::tool_path(tool) else {
        return ToolStatus {
            available: false,
            path: None,
            version: None,
        };
    };
    let arg = if matches!(tool, Tool::YtDlp) { "--version" } else { "-version" };
    let version = match tools::command(tool) {
        Ok(mut cmd) => {
            cmd.arg(arg)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            match tokio::time::timeout(Duration::from_secs(8), cmd.output()).await {
                Ok(Ok(out)) if out.status.success() => {
                    parse_tool_version(tool, &String::from_utf8_lossy(&out.stdout))
                }
                _ => None,
            }
        }
        Err(_) => None,
    };
    ToolStatus {
        available: version.is_some(),
        path: Some(path.to_string_lossy().into_owned()),
        version,
    }
}

#[tauri::command]
pub async fn tools_status(q: State<'_, Queue>) -> Result<ToolsStatus, String> {
    let browser_choice = q.0.settings().browser;
    let (ffmpeg, ffprobe, ytdlp) = tokio::join!(
        tool_status(Tool::Ffmpeg),
        tool_status(Tool::Ffprobe),
        tool_status(Tool::YtDlp)
    );
    Ok(ToolsStatus {
        ffmpeg,
        ffprobe,
        ytdlp,
        browser: detect::browser_status(browser_choice),
    })
}

#[tauri::command]
pub async fn logs_recent(q: State<'_, Queue>) -> Result<Vec<LogLine>, String> {
    Ok(lock(&q.0.logs).iter().cloned().collect())
}

/// Arrêt propre avant mise à jour / désinstallation : annule le téléchargement en cours (les
/// process ffmpeg / yt-dlp meurent à l'abandon des futurs, `kill_on_drop`), empêche le suivant de
/// démarrer et attend au plus `timeout` la fin de la tâche. L'élément interrompu repart en
/// `pending` au prochain démarrage.
pub async fn cancel_all(app: &AppHandle, timeout: Duration) {
    let Some(state) = app.try_state::<Queue>() else {
        return;
    };
    let q = state.0.clone();
    q.shutting_down.store(true, Ordering::SeqCst);
    if let Some((_, token)) = lock(&q.current).as_ref() {
        token.cancel();
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if lock(&q.current).is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify::Gap;

    #[test]
    fn sanitize_replaces_forbidden_chars() {
        assert_eq!(sanitize_filename(r#"a<b>c:d"e/f\g|h?i*j"#), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize_filename("tab\there\u{1}"), "tab_here_");
    }

    #[test]
    fn sanitize_trims_and_truncates() {
        assert_eq!(sanitize_filename("  film. . "), "film");
        assert_eq!(sanitize_filename("..."), "");
        assert_eq!(sanitize_filename(&"a".repeat(300)).chars().count(), 150);
        // La coupe ne doit pas laisser un point final
        let name = format!("{}.{}", "a".repeat(149), "b".repeat(20));
        assert_eq!(sanitize_filename(&name), "a".repeat(149));
        assert_eq!(sanitize_filename("épisode 1 – été"), "épisode 1 – été");
    }

    #[test]
    fn sanitize_avoids_reserved_names() {
        assert_eq!(sanitize_filename("con"), "_con");
        assert_eq!(sanitize_filename("NUL.txt"), "_NUL.txt");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("console"), "console");
    }

    #[test]
    fn default_filename_from_url() {
        assert_eq!(default_filename("https://cdn.example.com/a/b/master.m3u8?t=1"), "master");
        assert_eq!(default_filename("https://x.example/vid%20eo.mp4"), "vid eo");
        assert_eq!(default_filename("https://x.example/watch"), "watch");
        assert_eq!(default_filename("https://x.example/a/archive.tar.gz"), "archive.tar");
        assert!(default_filename("https://x.example/").starts_with("video-"));
        assert!(default_filename("n'importe quoi").starts_with("video-"));
    }

    #[test]
    fn unique_stem_adds_suffix_on_collision() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_stem(dir.path(), "film"), "film");
        std::fs::write(dir.path().join("film.mp4"), b"x").unwrap();
        assert_eq!(unique_stem(dir.path(), "film"), "film_2");
        std::fs::write(dir.path().join("film_2.mkv"), b"x").unwrap();
        assert_eq!(unique_stem(dir.path(), "film"), "film_3");
        // insensible à la casse, et les restes yt-dlp ne comptent pas
        std::fs::write(dir.path().join("Autre.MP4"), b"x").unwrap();
        assert_eq!(unique_stem(dir.path(), "autre"), "autre_2");
        std::fs::write(dir.path().join("zzz.mp4.part"), b"x").unwrap();
        assert_eq!(unique_stem(dir.path(), "zzz"), "zzz");
        assert_eq!(unique_stem(dir.path(), ""), "video");
    }

    #[test]
    fn detects_403() {
        assert!(is_403("ERROR: unable to download: HTTP Error 403: Forbidden"));
        assert!(is_403("Segment 12 : statut 403"));
        assert!(is_403("access FORBIDDEN"));
        assert!(!is_403("HTTP Error 404: Not Found"));
        assert!(!is_403("14037 octets reçus"));
    }

    #[test]
    fn attempts_are_capped_on_403() {
        assert_eq!(max_attempts("HTTP Error 403: Forbidden", 10), 3);
        assert_eq!(max_attempts("HTTP Error 403: Forbidden", 2), 2);
        assert_eq!(max_attempts("timeout", 10), 10);
    }

    #[test]
    fn rejects_non_http_urls() {
        let item = |url: &str, name: Option<&str>| NewItem {
            url: url.into(),
            filename: name.map(str::to_string),
            referer: None,
        };
        assert!(prepare_item(&item("ftp://x.example/a", None)).is_none());
        assert!(prepare_item(&item("javascript:alert(1)", None)).is_none());
        assert!(prepare_item(&item("", None)).is_none());
        assert!(prepare_item(&item("http://", None)).is_none());

        let (url, name, custom, referer) =
            prepare_item(&item("  HTTPS://x.example/v/film.m3u8 ", None)).unwrap();
        assert_eq!(url, "HTTPS://x.example/v/film.m3u8");
        assert_eq!((name.as_str(), custom, referer), ("film", false, None));

        let mut it = item("https://x.example/a.m3u8", Some(" Mon: film "));
        it.referer = Some(" https://ref.example/ ".into());
        let (_, name, custom, referer) = prepare_item(&it).unwrap();
        assert_eq!(name, "Mon_ film");
        assert!(custom);
        assert_eq!(referer.as_deref(), Some("https://ref.example/"));

        // nom vide après nettoyage : retombe sur le nom par défaut
        let (_, name, custom, _) = prepare_item(&item("https://x.example/a.m3u8", Some("..."))).unwrap();
        assert_eq!((name.as_str(), custom), ("a", false));
    }

    #[test]
    fn groups_segment_indices_per_track() {
        let items = [
            (TrackKind::Video, 5),
            (TrackKind::Audio, 2),
            (TrackKind::Video, 1),
            (TrackKind::Video, 5),
        ];
        assert_eq!(group_indices(&items, TrackKind::Video), vec![1, 5]);
        assert_eq!(group_indices(&items, TrackKind::Audio), vec![2]);
        assert!(group_indices(&[], TrackKind::Video).is_empty());
    }

    #[test]
    fn parses_tool_versions() {
        assert_eq!(
            parse_tool_version(Tool::Ffmpeg, "ffmpeg version 7.1-full_build-www.gyan.dev Copyright (c) 2000"),
            Some("7.1-full_build-www.gyan.dev".into())
        );
        assert_eq!(
            parse_tool_version(Tool::Ffprobe, "ffprobe version N-1234-gabc Copyright"),
            Some("N-1234-gabc".into())
        );
        assert_eq!(parse_tool_version(Tool::YtDlp, "2025.01.15\n"), Some("2025.01.15".into()));
        assert_eq!(parse_tool_version(Tool::YtDlp, ""), None);
    }

    fn report(ok: bool) -> GapReport {
        GapReport {
            ok,
            expected_duration: None,
            actual_duration: 1.0,
            gaps: Vec::<Gap>::new(),
            bad_segments: vec![],
            av_drift: None,
            summary: String::new(),
        }
    }

    #[test]
    fn outcome_status() {
        let mk = |report: Option<GapReport>, verify_error: Option<String>| Outcome {
            output: PathBuf::from("x.mp4"),
            report,
            verify_error,
        };
        assert_eq!(mk(None, None).status(), DownloadStatus::Done);
        assert_eq!(mk(Some(report(true)), None).status(), DownloadStatus::Done);
        assert_eq!(mk(Some(report(false)), None).status(), DownloadStatus::Warning);
        assert_eq!(mk(None, Some("x".into())).status(), DownloadStatus::Warning);
    }

    #[test]
    fn file_extension_from_url() {
        assert_eq!(file_ext_from_url("https://x.example/a/b.MKV?x=1"), "mkv");
        assert_eq!(file_ext_from_url("https://x.example/a/b.mp4"), "mp4");
        assert_eq!(file_ext_from_url("https://x.example/download?id=3"), "mp4");
    }
}
