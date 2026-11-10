//! Vérification d'intégrité : détecte les « moments sautés » dans une vidéo téléchargée.
//!
//! Deux étapes :
//! - [`verify_segments`] : avant assemblage, contrôle chaque segment (présence, durée mesurée,
//!   continuité des timestamps aux jonctions) ;
//! - [`verify_output`] : sur le fichier final, cherche les trous dans les timestamps des paquets
//!   vidéo puis audio, compare la durée à l'attendu et mesure la dérive audio/vidéo.
//!
//! La logique de décision est pure (fonctions sans E/S, testées plus bas) ; seul l'appel à ffprobe
//! est asynchrone.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::hls::{HlsDownload, SegmentInfo, TrackInfo, TrackKind};
use crate::model::{JobCtx, Phase, Progress};
use crate::tools::{command, Tool};

// ---------------------------------------------------------------------------------------------
// Types publics (contrat)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BadSegment {
    pub track: TrackKind,
    pub index: u32,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gap {
    pub stream: TrackKind,
    /// Position du début du trou, en secondes dans le fichier final.
    pub at: f64,
    /// Durée manquante, en secondes.
    pub missing: f64,
    pub segment_index: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GapReport {
    pub ok: bool,
    pub expected_duration: Option<f64>,
    pub actual_duration: f64,
    pub gaps: Vec<Gap>,
    /// Segments restés fautifs après réparation (rempli par l'appelant).
    pub bad_segments: Vec<BadSegment>,
    /// Écart durée audio - vidéo (s).
    pub av_drift: Option<f64>,
    pub summary: String,
}

// ---------------------------------------------------------------------------------------------
// Constantes de tolérance
// ---------------------------------------------------------------------------------------------

/// Nombre de ffprobe lancés en parallèle pour la vérification des segments.
const PARALLEL_PROBES: usize = 8;
/// Un segment est trop court si sa durée mesurée est inférieure à l'EXTINF de plus de
/// max(0,25 s ; 20 %).
const SHORT_ABS: f64 = 0.25;
const SHORT_REL: f64 = 0.20;
/// Trou toléré aux jonctions : max(2 images ; 0,1 s).
const JUNCTION_FRAMES: f64 = 2.0;
const GAP_MIN: f64 = 0.1;
/// Trou vidéo dans le fichier final : écart de PTS > max(2,5 × image médiane ; 0,1 s).
const VIDEO_GAP_FACTOR: f64 = 2.5;
/// Trou audio : écart de PTS > max(3 paquets médians ; 0,1 s).
const AUDIO_GAP_PACKETS: f64 = 3.0;
/// Durée plus courte que l'attendu de plus de max(1 s ; 0,5 %).
const DURATION_ABS: f64 = 1.0;
const DURATION_REL: f64 = 0.005;
/// Dérive audio/vidéo maximale acceptée (s).
const DRIFT_MAX: f64 = 0.5;
/// Durée d'image supposée quand rien n'est mesurable (25 i/s).
const DEFAULT_FRAME: f64 = 0.04;
/// Nombre maximal de coupures énumérées dans le résumé.
const SUMMARY_MAX_GAPS: usize = 6;

// ---------------------------------------------------------------------------------------------
// Formatage (pur)
// ---------------------------------------------------------------------------------------------

/// Nombre avec virgule décimale française.
fn fr(x: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, x).replace('.', ",")
}

/// Secondes avec une décimale (deux si la valeur serait arrondie à 0,0).
fn fmt_secs(x: f64) -> String {
    let one = fr(x, 1);
    if x > 0.0 && one == "0,0" {
        fr(x, 2)
    } else {
        one
    }
}

/// Timecode `HH:MM:SS` (arrondi à la seconde).
fn timecode(sec: f64) -> String {
    let t = if sec.is_finite() { sec.max(0.0).round() as u64 } else { 0 };
    format!("{:02}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

/// Durée lisible : `1 h 42 min 07 s`, `12 min 05 s`, `42 s`.
fn human_duration(sec: f64) -> String {
    let t = if sec.is_finite() { sec.max(0.0).round() as u64 } else { 0 };
    let (h, m, s) = (t / 3600, (t / 60) % 60, t % 60);
    if h > 0 {
        format!("{h} h {m:02} min {s:02} s")
    } else if m > 0 {
        format!("{m} min {s:02} s")
    } else {
        format!("{s} s")
    }
}

// ---------------------------------------------------------------------------------------------
// Tolérances (pur)
// ---------------------------------------------------------------------------------------------

/// Tolérance sur la durée d'un segment : max(0,25 s ; 20 % de l'EXTINF).
fn short_tolerance(extinf: f64) -> f64 {
    SHORT_ABS.max(SHORT_REL * extinf)
}

/// Vrai si la durée mesurée est inférieure à l'EXTINF de plus de la tolérance.
fn is_too_short(extinf: f64, measured: f64) -> bool {
    extinf > 0.0 && measured < extinf - short_tolerance(extinf)
}

/// Trou toléré entre deux segments consécutifs : max(2 images ; 0,1 s).
fn junction_tolerance(frame_dur: f64) -> f64 {
    (JUNCTION_FRAMES * frame_dur).max(GAP_MIN)
}

/// Vrai si la durée réelle est plus courte que l'attendue de plus de max(1 s ; 0,5 %).
fn duration_shortfall(expected: f64, actual: f64) -> bool {
    expected > 0.0 && expected - actual > DURATION_ABS.max(DURATION_REL * expected)
}

// ---------------------------------------------------------------------------------------------
// Mesure d'un segment (ffprobe JSON -> SegMeasure, pur)
// ---------------------------------------------------------------------------------------------

/// Ce que l'on sait d'un segment mesuré : début, fin et durée du flux principal (secondes).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SegMeasure {
    pub start: f64,
    pub end: f64,
    pub duration: f64,
    /// Durée d'une image (ou d'un paquet audio) médiane, si mesurable.
    pub frame_dur: Option<f64>,
}

/// Lit un nombre ffprobe : chaîne (`"1.234"`) ou nombre ; rejette `N/A` et les valeurs non finies.
fn json_num(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    n.is_finite().then_some(n)
}

/// Médiane d'une liste (triée en place).
fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Durée médiane d'un paquet : médiane des durées déclarées, sinon des écarts de PTS triés.
fn median_frame(sorted_pts: &[f64], durs: &mut [f64]) -> Option<f64> {
    if let Some(m) = median(durs) {
        return Some(m);
    }
    let mut diffs: Vec<f64> = sorted_pts
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 1e-9)
        .collect();
    median(&mut diffs)
}

/// Interprète la sortie JSON de ffprobe pour un segment.
/// `kind` indique le flux à privilégier (vidéo pour une piste vidéo, audio pour une piste audio).
pub(crate) fn measure_from_json(json: &str, kind: TrackKind) -> Result<SegMeasure, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|e| format!("sortie de ffprobe illisible ({e})"))?;
    let streams = root.get("streams").and_then(Value::as_array).cloned().unwrap_or_default();
    // Seuls les flux audio/vidéo comptent.
    let av: Vec<&Value> = streams
        .iter()
        .filter(|s| matches!(s.get("codec_type").and_then(Value::as_str), Some("video") | Some("audio")))
        .collect();
    if av.is_empty() {
        return Err("aucun flux audio ou vidéo lisible dans le segment".to_string());
    }
    let wanted = match kind {
        TrackKind::Video => "video",
        TrackKind::Audio => "audio",
    };
    let primary = av
        .iter()
        .find(|s| s.get("codec_type").and_then(Value::as_str) == Some(wanted))
        .or_else(|| av.first())
        .copied()
        .ok_or_else(|| "aucun flux audio ou vidéo lisible dans le segment".to_string())?;
    let primary_idx = json_num(primary.get("index"));

    let mut pts: Vec<f64> = Vec::new();
    let mut durs: Vec<f64> = Vec::new();
    let mut max_end = f64::NEG_INFINITY;
    if let Some(packets) = root.get("packets").and_then(Value::as_array) {
        for p in packets {
            if json_num(p.get("stream_index")) != primary_idx {
                continue;
            }
            let Some(t) = json_num(p.get("pts_time")) else { continue };
            pts.push(t);
            let d = json_num(p.get("duration_time")).filter(|d| *d > 0.0);
            if let Some(d) = d {
                durs.push(d);
            }
            max_end = max_end.max(t + d.unwrap_or(0.0));
        }
    }

    let format = root.get("format");
    if pts.is_empty() {
        // Aucun paquet exploitable : repli sur les métadonnées de flux puis de conteneur.
        let start = json_num(primary.get("start_time"))
            .or_else(|| json_num(format.and_then(|f| f.get("start_time"))))
            .unwrap_or(0.0);
        let duration = json_num(primary.get("duration"))
            .or_else(|| json_num(format.and_then(|f| f.get("duration"))))
            .ok_or_else(|| "durée du segment illisible (aucun paquet décodable)".to_string())?;
        return Ok(SegMeasure { start, end: start + duration, duration, frame_dur: None });
    }

    pts.sort_by(|a, b| a.total_cmp(b));
    let frame = median_frame(&pts, &mut durs);
    let start = pts[0];
    let last = pts[pts.len() - 1];
    let end = max_end.max(last + frame.unwrap_or(0.0));
    Ok(SegMeasure { start, end, duration: (end - start).max(0.0), frame_dur: frame })
}

// ---------------------------------------------------------------------------------------------
// Évaluation des segments et des jonctions (pur)
// ---------------------------------------------------------------------------------------------

/// Résultat de la mesure d'un segment, prêt à être évalué.
#[derive(Clone, Debug)]
pub(crate) struct SegCheck {
    pub index: u32,
    pub extinf: f64,
    pub discontinuity_before: bool,
    /// `Err(raison)` : segment inutilisable (absent, vide, illisible).
    pub result: Result<SegMeasure, String>,
}

/// Décide quels segments sont fautifs : durée trop courte, fichier défectueux, trou aux jonctions.
/// `checks` doit être dans l'ordre de la playlist. Résultat trié par index.
pub(crate) fn evaluate(kind: TrackKind, checks: &[SegCheck]) -> Vec<BadSegment> {
    let mut bad: BTreeMap<u32, Vec<String>> = BTreeMap::new();

    // 1. Fichier défectueux ou durée trop courte.
    for c in checks {
        match &c.result {
            Err(reason) => bad.entry(c.index).or_default().push(capitalize(reason)),
            Ok(m) => {
                if is_too_short(c.extinf, m.duration) {
                    bad.entry(c.index).or_default().push(format!(
                        "Durée mesurée {} s au lieu de {} s annoncées",
                        fr(m.duration, 2),
                        fr(c.extinf, 2)
                    ));
                }
            }
        }
    }

    // 2. Continuité aux jonctions (sauf après une discontinuité déclarée : les timestamps repartent).
    for pair in checks.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if b.discontinuity_before {
            continue;
        }
        let (Ok(ma), Ok(mb)) = (&a.result, &b.result) else { continue };
        let gap = mb.start - ma.end;
        let frame = ma.frame_dur.unwrap_or(DEFAULT_FRAME).max(mb.frame_dur.unwrap_or(DEFAULT_FRAME));
        if gap <= junction_tolerance(frame) {
            continue; // continuité correcte ; un recouvrement (gap < 0) n'est pas une erreur
        }
        let a_short = is_too_short(a.extinf, ma.duration);
        // Fautif : le segment dont la durée est trop courte (sa fin manque), sinon N+1.
        if a_short {
            bad.entry(a.index).or_default().push(format!(
                "Trou de {} s entre ce segment (fin à {} s) et le suivant (début à {} s)",
                fr(gap, 2),
                fr(ma.end, 2),
                fr(mb.start, 2)
            ));
        } else {
            bad.entry(b.index).or_default().push(format!(
                "Trou de {} s entre le segment précédent (fin à {} s) et celui-ci (début à {} s)",
                fr(gap, 2),
                fr(ma.end, 2),
                fr(mb.start, 2)
            ));
        }
    }

    bad.into_iter()
        .map(|(index, reasons)| BadSegment { track: kind, index, reason: reasons.join(" ; ") })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut it = s.chars();
    match it.next() {
        Some(c) => c.to_uppercase().collect::<String>() + it.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------------------------
// Analyse des paquets du fichier final (pur)
// ---------------------------------------------------------------------------------------------

/// Paquets d'un flux, accumulés au fil de la lecture de ffprobe.
#[derive(Debug)]
pub(crate) struct PacketStats {
    pts: Vec<f64>,
    durs: Vec<f64>,
    max_end: f64,
}

impl Default for PacketStats {
    fn default() -> Self {
        Self { pts: Vec::new(), durs: Vec::new(), max_end: f64::NEG_INFINITY }
    }
}

/// Trou brut détecté dans un flux (positions absolues dans les timestamps du fichier).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RawGap {
    pub at: f64,
    pub missing: f64,
}

/// Résultat de l'analyse d'un flux.
#[derive(Clone, Debug)]
#[allow(dead_code)] // `frame_dur` et `packets` ne servent qu'aux tests
pub(crate) struct StreamAnalysis {
    pub first_pts: f64,
    pub end: f64,
    pub frame_dur: f64,
    pub packets: usize,
    pub gaps: Vec<RawGap>,
}

impl PacketStats {
    pub fn push(&mut self, pts: f64, dur: Option<f64>) {
        self.pts.push(pts);
        let d = dur.filter(|d| *d > 0.0);
        if let Some(d) = d {
            self.durs.push(d);
        }
        self.max_end = self.max_end.max(pts + d.unwrap_or(0.0));
    }

    /// Trie par PTS (les B-frames arrivent dans l'ordre de décodage), calcule la durée médiane et
    /// repère les écarts anormaux. `None` si le flux n'a aucun paquet.
    pub fn analyze(mut self, kind: TrackKind) -> Option<StreamAnalysis> {
        if self.pts.is_empty() {
            return None;
        }
        self.pts.sort_by(|a, b| a.total_cmp(b));
        let frame = median_frame(&self.pts, &mut self.durs).unwrap_or(0.0);
        let threshold = match kind {
            TrackKind::Video => (VIDEO_GAP_FACTOR * frame).max(GAP_MIN),
            TrackKind::Audio => (AUDIO_GAP_PACKETS * frame).max(GAP_MIN),
        };
        let mut gaps = Vec::new();
        for w in self.pts.windows(2) {
            let d = w[1] - w[0];
            if d > threshold {
                gaps.push(RawGap { at: w[0] + frame, missing: (d - frame).max(0.0) });
            }
        }
        let first_pts = self.pts[0];
        let last = self.pts[self.pts.len() - 1];
        Some(StreamAnalysis {
            first_pts,
            end: self.max_end.max(last + frame),
            frame_dur: frame,
            packets: self.pts.len(),
            gaps,
        })
    }
}

/// Analyse une liste brute de (PTS, durée) : raccourci utilisé par les tests.
#[cfg(test)]
fn analyze_packets(kind: TrackKind, packets: &[(f64, Option<f64>)]) -> Option<StreamAnalysis> {
    let mut st = PacketStats::default();
    for (p, d) in packets {
        st.push(*p, *d);
    }
    st.analyze(kind)
}

/// Interprète une ligne CSV `pts_time,duration_time,flags` de ffprobe.
fn parse_packet_line(line: &str) -> Option<(f64, Option<f64>)> {
    let mut it = line.trim().split(',');
    let pts = it.next()?.trim().parse::<f64>().ok().filter(|p| p.is_finite())?;
    let dur = it.next().and_then(|d| d.trim().parse::<f64>().ok()).filter(|d| d.is_finite() && *d > 0.0);
    Some((pts, dur))
}

/// Segment correspondant à la position `t` (secondes depuis le début de la piste), en cumulant les
/// EXTINF. `segments` : (index, durée). `None` si la position dépasse nettement la fin.
pub(crate) fn locate_segment(segments: &[(u32, f64)], t: f64) -> Option<u32> {
    if segments.is_empty() || !t.is_finite() {
        return None;
    }
    let t = t.max(0.0);
    let mut start = 0.0;
    for (index, dur) in segments {
        if t < start + dur {
            return Some(*index);
        }
        start += dur;
    }
    // Léger dépassement dû aux arrondis : dernier segment.
    if t <= start + 1.0 {
        segments.last().map(|(i, _)| *i)
    } else {
        None
    }
}

fn track_extinfs(track: &TrackInfo) -> Vec<(u32, f64)> {
    track.segments.iter().map(|s| (s.index, s.duration)).collect()
}

// ---------------------------------------------------------------------------------------------
// Rapport (pur)
// ---------------------------------------------------------------------------------------------

/// Données d'entrée du rapport, déjà extraites de ffprobe.
pub(crate) struct OutputFacts<'a> {
    pub expected_duration: Option<f64>,
    /// Durée du conteneur (`format=duration`), si ffprobe la donne.
    pub container_duration: Option<f64>,
    pub video: Option<StreamAnalysis>,
    pub audio: Option<StreamAnalysis>,
    pub tracks: Option<&'a HlsDownload>,
}

pub(crate) fn build_report(f: OutputFacts<'_>) -> GapReport {
    let origin = match (&f.video, &f.audio) {
        (Some(v), Some(a)) => v.first_pts.min(a.first_pts),
        (Some(v), None) => v.first_pts,
        (None, Some(a)) => a.first_pts,
        (None, None) => 0.0,
    };

    let video_ex = f.tracks.map(|t| track_extinfs(&t.video));
    // Piste audio séparée si elle existe, sinon l'audio est muxé dans les segments vidéo.
    let audio_ex = f.tracks.map(|t| track_extinfs(t.audio.as_ref().unwrap_or(&t.video)));

    let mut gaps: Vec<Gap> = Vec::new();
    for (kind, an, ex) in [
        (TrackKind::Video, &f.video, &video_ex),
        (TrackKind::Audio, &f.audio, &audio_ex),
    ] {
        let Some(an) = an else { continue };
        for g in &an.gaps {
            let at = (g.at - origin).max(0.0);
            // Le milieu du trou tombe dans le segment qui manque (ou qui est tronqué).
            let segment_index = ex.as_ref().and_then(|e| locate_segment(e, at + g.missing / 2.0));
            gaps.push(Gap { stream: kind, at, missing: g.missing, segment_index });
        }
    }
    gaps.sort_by(|a, b| a.at.total_cmp(&b.at));

    let has_streams = f.video.is_some() || f.audio.is_some();
    let ends = [f.video.as_ref().map(|v| v.end), f.audio.as_ref().map(|a| a.end)];
    let actual_duration = f
        .container_duration
        .filter(|d| *d > 0.0)
        .or_else(|| {
            ends.iter().flatten().copied().reduce(f64::max).map(|e| (e - origin).max(0.0))
        })
        .unwrap_or(0.0);

    let short = f.expected_duration.map(|e| duration_shortfall(e, actual_duration)).unwrap_or(false);
    let av_drift = match (&f.video, &f.audio) {
        (Some(v), Some(a)) => Some(a.end - v.end),
        _ => None,
    };
    let drift_bad = av_drift.map(|d| d.abs() > DRIFT_MAX).unwrap_or(false);
    let ok = has_streams && gaps.is_empty() && !short && !drift_bad;

    let segments_count = f.tracks.map(|t| t.video.segments.len());
    let summary = build_summary(&SummaryInput {
        has_streams,
        gaps: &gaps,
        actual_duration,
        expected_duration: f.expected_duration,
        short,
        av_drift: av_drift.filter(|_| drift_bad),
        segments_count,
    });

    GapReport {
        ok,
        expected_duration: f.expected_duration,
        actual_duration,
        gaps,
        bad_segments: Vec::new(),
        av_drift,
        summary,
    }
}

pub(crate) struct SummaryInput<'a> {
    pub has_streams: bool,
    pub gaps: &'a [Gap],
    pub actual_duration: f64,
    pub expected_duration: Option<f64>,
    /// Durée réelle trop courte par rapport à l'attendu.
    pub short: bool,
    /// Dérive A/V, seulement si elle dépasse la tolérance.
    pub av_drift: Option<f64>,
    pub segments_count: Option<usize>,
}

/// Fusionne les trous qui se recouvrent (typiquement le même trou vu en vidéo et en audio) pour le
/// résumé : (début, manquant, uniquement audio).
fn merge_gaps_for_summary(gaps: &[Gap]) -> Vec<(f64, f64, bool)> {
    let mut sorted: Vec<&Gap> = gaps.iter().collect();
    sorted.sort_by(|a, b| a.at.total_cmp(&b.at));
    let mut out: Vec<(f64, f64, bool)> = Vec::new();
    for g in sorted {
        let audio = g.stream == TrackKind::Audio;
        if let Some(last) = out.last_mut() {
            if g.at <= last.0 + last.1 + 0.5 {
                last.1 = last.1.max(g.missing);
                last.2 = last.2 && audio;
                continue;
            }
        }
        out.push((g.at, g.missing, audio));
    }
    out
}

pub(crate) fn build_summary(s: &SummaryInput<'_>) -> String {
    if !s.has_streams {
        return "Aucun flux audio ou vidéo lisible dans le fichier.".to_string();
    }
    let merged = merge_gaps_for_summary(s.gaps);
    let mut out = String::new();
    if merged.is_empty() {
        let mut detail = human_duration(s.actual_duration);
        if let Some(n) = s.segments_count {
            detail.push_str(&format!(", {n} segment{}", if n > 1 { "s" } else { "" }));
        }
        out.push_str(&format!("Aucune coupure détectée ({detail})."));
    } else {
        let n = merged.len();
        out.push_str(&format!("{n} coupure{} détectée{} : ", plural(n), plural(n)));
        let items: Vec<String> = merged
            .iter()
            .take(SUMMARY_MAX_GAPS)
            .enumerate()
            .map(|(i, (at, missing, audio))| {
                let tag = if *audio { " (audio)" } else { "" };
                if i == 0 {
                    format!("{} s manquante à {}{tag}", fmt_secs(*missing), timecode(*at))
                } else {
                    format!("{} s à {}{tag}", fmt_secs(*missing), timecode(*at))
                }
            })
            .collect();
        out.push_str(&items.join(", "));
        if n > SUMMARY_MAX_GAPS {
            let rest = n - SUMMARY_MAX_GAPS;
            out.push_str(&format!(", et {rest} autre{}", plural(rest)));
        }
        out.push('.');
    }
    if s.short {
        if let Some(exp) = s.expected_duration {
            out.push_str(&format!(
                " Durée de {} inférieure aux {} attendues.",
                human_duration(s.actual_duration),
                human_duration(exp)
            ));
        }
    }
    if let Some(d) = s.av_drift {
        out.push_str(&format!(
            " Décalage audio/vidéo de {} s ({}).",
            fmt_secs(d.abs()),
            if d > 0.0 { "audio plus long" } else { "audio plus court" }
        ));
    }
    out
}

fn plural(n: usize) -> &'static str {
    if n > 1 {
        "s"
    } else {
        ""
    }
}

// ---------------------------------------------------------------------------------------------
// ffprobe : segments
// ---------------------------------------------------------------------------------------------

const SEGMENT_ENTRIES: &str = "stream=index,codec_type,start_time,duration,time_base:format=start_time,duration:packet=stream_index,pts_time,duration_time";

/// Première ligne non vide d'un message d'erreur, tronquée.
fn first_line(s: &str) -> String {
    let l = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("erreur inconnue");
    l.chars().take(200).collect()
}

/// Mesure un segment avec ffprobe. `Err` externe : erreur globale (ffprobe introuvable…) ;
/// `Ok(Err(raison))` : le segment lui-même est défectueux.
async fn probe_segment(
    kind: TrackKind,
    seg: &SegmentInfo,
    init: Option<&Arc<Vec<u8>>>,
) -> Result<Result<SegMeasure, String>> {
    let len = match tokio::fs::metadata(&seg.path).await {
        Ok(m) => m.len(),
        Err(_) => return Ok(Err("fichier du segment absent".to_string())),
    };
    if len == 0 {
        return Ok(Err("fichier du segment vide (0 octet)".to_string()));
    }

    let mut cmd = command(Tool::Ffprobe)?;
    cmd.args(["-v", "error", "-print_format", "json", "-show_entries", SEGMENT_ENTRIES]);

    // fMP4 : le segment seul n'est pas décodable, on envoie init + segment sur stdin.
    let payload: Option<Vec<u8>> = match init {
        Some(init) => {
            let body = match tokio::fs::read(&seg.path).await {
                Ok(b) => b,
                Err(e) => return Ok(Err(format!("lecture du segment impossible ({e})"))),
            };
            let mut data = Vec::with_capacity(init.len() + body.len());
            data.extend_from_slice(init);
            data.extend_from_slice(&body);
            Some(data)
        }
        None => None,
    };
    if payload.is_some() {
        cmd.args(["-i", "pipe:0"]).stdin(Stdio::piped());
    } else {
        cmd.arg("-i").arg(&seg.path).stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().context("Impossible de lancer ffprobe")?;
    let stdin = child.stdin.take();
    let writer = async move {
        if let (Some(mut stdin), Some(data)) = (stdin, payload) {
            // ffprobe peut fermer le tube avant la fin : l'erreur d'écriture est sans importance.
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        }
    };
    let writer = tokio::spawn(writer);
    let output = child.wait_with_output().await;
    let _ = writer.await;
    let output = match output {
        Ok(o) => o,
        Err(e) => return Ok(Err(format!("ffprobe interrompu ({e})"))),
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Ok(Err(format!("ffprobe n'a pas pu lire le segment ({})", first_line(&err))));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    Ok(measure_from_json(&json, kind))
}

/// Vérifie chaque segment de la piste (8 ffprobe en parallèle) et renvoie ceux qui sont fautifs :
/// fichier absent/vide, illisible, trop court par rapport à l'EXTINF, ou trou aux jonctions.
pub async fn verify_segments(track: &TrackInfo, ctx: &JobCtx) -> Result<Vec<BadSegment>> {
    ctx.check()?;
    let total = track.segments.len();
    if total == 0 {
        return Ok(Vec::new());
    }
    let kind = track.kind;

    let init: Option<Arc<Vec<u8>>> = match &track.init_segment {
        Some(p) => Some(Arc::new(
            tokio::fs::read(p)
                .await
                .with_context(|| format!("Segment d'initialisation illisible : {}", p.display()))?,
        )),
        None => None,
    };

    let sem = Arc::new(Semaphore::new(PARALLEL_PROBES));
    let mut set: JoinSet<Result<(usize, Result<SegMeasure, String>)>> = JoinSet::new();
    for (pos, seg) in track.segments.iter().enumerate() {
        let seg = seg.clone();
        let init = init.clone();
        let sem = sem.clone();
        let ctx = ctx.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await?;
            ctx.check()?;
            let r = probe_segment(kind, &seg, init.as_ref()).await?;
            Ok((pos, r))
        });
    }

    let mut results: Vec<Option<Result<SegMeasure, String>>> = vec![None; total];
    let mut done: u32 = 0;
    let mut last_emit = Instant::now() - Duration::from_secs(1);
    emit_progress(ctx, 0, total as u32);
    // Si on sort en erreur (annulation, ffprobe introuvable), le JoinSet est détruit : les tâches
    // sont abandonnées et les ffprobe en cours tués (kill_on_drop).
    while let Some(joined) = set.join_next().await {
        let (pos, r) = joined.map_err(|e| anyhow!("Vérification interrompue : {e}"))??;
        results[pos] = Some(r);
        done += 1;
        if done as usize == total || last_emit.elapsed() >= Duration::from_millis(150) {
            emit_progress(ctx, done, total as u32);
            last_emit = Instant::now();
        }
    }
    ctx.check()?;

    let checks: Vec<SegCheck> = track
        .segments
        .iter()
        .zip(results)
        .map(|(seg, r)| SegCheck {
            index: seg.index,
            extinf: seg.duration,
            discontinuity_before: seg.discontinuity_before,
            result: r.unwrap_or_else(|| Err("segment non vérifié".to_string())),
        })
        .collect();
    let bad = evaluate(kind, &checks);
    if !bad.is_empty() {
        ctx.warn(format!("{} segment{} suspect{} sur {total}", bad.len(), plural(bad.len()), plural(bad.len())));
    }
    Ok(bad)
}

fn emit_progress(ctx: &JobCtx, done: u32, total: u32) {
    ctx.progress(Progress {
        phase: Some(Phase::Verifying),
        percent: if total == 0 { 100.0 } else { done as f32 * 100.0 / total as f32 },
        segments_done: Some(done),
        segments_total: Some(total),
        ..Default::default()
    });
}

// ---------------------------------------------------------------------------------------------
// ffprobe : fichier final
// ---------------------------------------------------------------------------------------------

/// Lit les paquets d'un flux (`v:0` ou `a:0`) en CSV, ligne par ligne (pas de JSON complet en
/// mémoire : un film de plusieurs heures compte des centaines de milliers de paquets).
async fn scan_packets(output: &Path, selector: &str, ctx: &JobCtx) -> Result<PacketStats> {
    let mut cmd = command(Tool::Ffprobe)?;
    cmd.args(["-v", "error", "-select_streams", selector, "-show_entries", "packet=pts_time,duration_time,flags", "-of", "csv=print_section=0"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("Impossible de lancer ffprobe")?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("Sortie de ffprobe indisponible"))?;
    let mut stderr = child.stderr.take().ok_or_else(|| anyhow!("Erreurs de ffprobe indisponibles"))?;
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = (&mut stderr).take(64 * 1024).read_to_string(&mut buf).await;
        buf
    });

    let mut stats = PacketStats::default();
    let mut lines = BufReader::new(stdout).lines();
    let mut n: u64 = 0;
    while let Some(line) = lines.next_line().await.context("Lecture de la sortie de ffprobe")? {
        if let Some((pts, dur)) = parse_packet_line(&line) {
            stats.push(pts, dur);
        }
        n += 1;
        if n.is_multiple_of(8192) {
            ctx.check()?;
        }
    }
    let status = child.wait().await.context("Attente de ffprobe")?;
    let err = err_task.await.unwrap_or_default();
    if !status.success() {
        return Err(anyhow!("ffprobe n'a pas pu lire le fichier ({})", first_line(&err)));
    }
    Ok(stats)
}

/// Durée du conteneur (`format=duration`).
async fn container_duration(output: &Path) -> Result<Option<f64>> {
    let mut cmd = command(Tool::Ffprobe)?;
    cmd.args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=print_section=0"])
        .arg(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.spawn().context("Impossible de lancer ffprobe")?.wait_with_output().await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("ffprobe n'a pas pu lire le fichier ({})", first_line(&err)));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().find_map(|l| l.trim().parse::<f64>().ok()).filter(|d| d.is_finite()))
}

/// Vérifie le fichier final : trous vidéo/audio, durée, dérive audio/vidéo.
/// `tracks` permet de rattacher chaque trou à un segment (`segment_index`).
pub async fn verify_output(
    output: &Path,
    expected_duration: Option<f64>,
    tracks: Option<&HlsDownload>,
    ctx: &JobCtx,
) -> Result<GapReport> {
    ctx.check()?;
    if !output.is_file() {
        return Err(anyhow!("Fichier à vérifier introuvable : {}", output.display()));
    }
    let report_progress = |percent: f32| {
        ctx.progress(Progress { phase: Some(Phase::Verifying), percent, ..Default::default() });
    };
    report_progress(2.0);
    let video = scan_packets(output, "v:0", ctx).await?.analyze(TrackKind::Video);
    report_progress(45.0);
    let audio = scan_packets(output, "a:0", ctx).await?.analyze(TrackKind::Audio);
    report_progress(90.0);
    let duration = container_duration(output).await?;
    ctx.check()?;
    let report = build_report(OutputFacts { expected_duration, container_duration: duration, video, audio, tracks });
    report_progress(100.0);
    Ok(report)
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    /// Paquets à cadence fixe : `n` images de durée `dur` à partir de `start`.
    fn regular(start: f64, dur: f64, n: usize) -> Vec<(f64, Option<f64>)> {
        (0..n).map(|i| (start + i as f64 * dur, Some(dur))).collect()
    }

    // ----- trous dans une liste de PTS --------------------------------------------------------

    #[test]
    fn aucun_trou_video() {
        let an = analyze_packets(TrackKind::Video, &regular(0.0, 0.04, 250)).expect("analyse");
        assert!(an.gaps.is_empty());
        assert!(approx(an.end, 10.0, 1e-6));
        assert!(approx(an.frame_dur, 0.04, 1e-9));
    }

    #[test]
    fn un_trou_video_de_deux_secondes() {
        // 0..4 s puis 6..10 s à 25 i/s.
        let mut p = regular(0.0, 0.04, 100);
        p.extend(regular(6.0, 0.04, 100));
        let an = analyze_packets(TrackKind::Video, &p).expect("analyse");
        assert_eq!(an.gaps.len(), 1);
        assert!(approx(an.gaps[0].at, 4.0, 1e-6), "at = {}", an.gaps[0].at);
        assert!(approx(an.gaps[0].missing, 2.0, 1e-6), "missing = {}", an.gaps[0].missing);
    }

    #[test]
    fn bframes_dans_l_ordre_de_decodage_sans_faux_positif() {
        // Ordre de décodage typique I P B B : PTS 0, 3, 1, 2, 6, 4, 5 (x 0,04 s).
        let order = [0, 3, 1, 2, 6, 4, 5, 9, 7, 8];
        let p: Vec<(f64, Option<f64>)> = order.iter().map(|i| (*i as f64 * 0.04, Some(0.04))).collect();
        let an = analyze_packets(TrackKind::Video, &p).expect("analyse");
        assert!(an.gaps.is_empty(), "{:?}", an.gaps);
        assert!(approx(an.first_pts, 0.0, 1e-9));
    }

    #[test]
    fn bframes_desordonnees_avec_un_vrai_trou() {
        let mut p: Vec<(f64, Option<f64>)> =
            [3, 1, 2, 0].iter().map(|i| (*i as f64 * 0.04, Some(0.04))).collect();
        // saut d'une seconde après la 4e image
        p.extend([30, 28, 29, 31].iter().map(|i| (*i as f64 * 0.04, Some(0.04))));
        let an = analyze_packets(TrackKind::Video, &p).expect("analyse");
        assert_eq!(an.gaps.len(), 1);
        assert!(approx(an.gaps[0].at, 0.16, 1e-9));
        assert!(approx(an.gaps[0].missing, 28.0 * 0.04 - 0.16, 1e-9));
    }

    #[test]
    fn vfr_leger_sans_faux_positif() {
        // Durées entre 30 et 40 ms, déterministes ; durées déclarées absentes sur une partie.
        let mut t = 0.0;
        let mut p = Vec::new();
        for i in 0..400 {
            let d = if i % 3 == 0 { 0.030 } else if i % 3 == 1 { 0.040 } else { 0.0345 };
            p.push((t, if i % 5 == 0 { None } else { Some(d) }));
            t += d;
        }
        let an = analyze_packets(TrackKind::Video, &p).expect("analyse");
        assert!(an.gaps.is_empty(), "{:?}", an.gaps);
    }

    #[test]
    fn durees_absentes_mediane_des_ecarts() {
        let p: Vec<(f64, Option<f64>)> = (0..100).map(|i| (i as f64 * 0.04, None)).collect();
        let an = analyze_packets(TrackKind::Video, &p).expect("analyse");
        assert!(approx(an.frame_dur, 0.04, 1e-9));
        assert!(an.gaps.is_empty());
        assert!(approx(an.end, 4.0, 1e-9));
    }

    #[test]
    fn seuil_video_deux_images_et_demie_et_plancher_a_0_1_s() {
        // 25 i/s : seuil = max(0,1 ; 0,1) = 0,1 ; un saut de 3 images (0,12 s) est détecté,
        // un saut de 2 images (0,08 s, image dupliquée/perdue) non.
        let mut p = regular(0.0, 0.04, 10);
        let last = 9.0 * 0.04;
        p.push((last + 0.08, Some(0.04)));
        assert!(analyze_packets(TrackKind::Video, &p).expect("a").gaps.is_empty());
        let mut q = regular(0.0, 0.04, 10);
        q.push((last + 0.12, Some(0.04)));
        assert_eq!(analyze_packets(TrackKind::Video, &q).expect("a").gaps.len(), 1);
        // 5 i/s : seuil = 2,5 x 0,2 = 0,5 s ; un saut de 0,4 s n'est pas un trou.
        let mut r = regular(0.0, 0.2, 10);
        r.push((9.0 * 0.2 + 0.4, Some(0.2)));
        assert!(analyze_packets(TrackKind::Video, &r).expect("a").gaps.is_empty());
    }

    #[test]
    fn trou_audio_seuil_trois_paquets_ou_0_1_s() {
        // AAC 1024/44100 = 23,2 ms : seuil = 0,1 s.
        let d = 1024.0 / 44100.0;
        let mut p = regular(0.0, d, 100);
        let end = 100.0 * d;
        p.extend(regular(end + 0.8, d, 100));
        let an = analyze_packets(TrackKind::Audio, &p).expect("analyse");
        assert_eq!(an.gaps.len(), 1);
        assert!(approx(an.gaps[0].at, end, 1e-6));
        assert!(approx(an.gaps[0].missing, 0.8, 1e-6));
        // Un saut d'un paquet ne déclenche rien.
        let mut q = regular(0.0, d, 100);
        q.extend(regular(end + d, d, 100));
        assert!(analyze_packets(TrackKind::Audio, &q).expect("a").gaps.is_empty());
        // Grands paquets (60 ms) : seuil = 0,18 s.
        let mut r = regular(0.0, 0.06, 20);
        r.extend(regular(20.0 * 0.06 + 0.1, 0.06, 20));
        assert!(analyze_packets(TrackKind::Audio, &r).expect("a").gaps.is_empty());
    }

    #[test]
    fn flux_vide() {
        assert!(analyze_packets(TrackKind::Audio, &[]).is_none());
    }

    #[test]
    fn lecture_lignes_csv() {
        assert_eq!(parse_packet_line("0.040000,0.040000,K__"), Some((0.04, Some(0.04))));
        assert_eq!(parse_packet_line("1.5,N/A,___\r"), Some((1.5, None)));
        assert_eq!(parse_packet_line("N/A,0.04,K__"), None);
        assert_eq!(parse_packet_line(""), None);
        assert_eq!(parse_packet_line("2.0"), Some((2.0, None)));
    }

    // ----- mesure d'un segment depuis le JSON ffprobe ----------------------------------------

    fn json_segment(start: f64, frames: usize, dur: f64) -> String {
        let packets: Vec<String> = (0..frames)
            .map(|i| {
                format!(
                    r#"{{"stream_index":0,"pts_time":"{:.6}","duration_time":"{:.6}"}}"#,
                    start + i as f64 * dur,
                    dur
                )
            })
            .collect();
        format!(
            r#"{{"streams":[{{"index":0,"codec_type":"video","start_time":"{start}","duration":"N/A"}},{{"index":1,"codec_type":"audio"}}],
               "packets":[{}, {{"stream_index":1,"pts_time":"{start}","duration_time":"0.02"}}],
               "format":{{"start_time":"{start}","duration":"6.0"}}}}"#,
            packets.join(",")
        )
    }

    #[test]
    fn mesure_json_segment_normal() {
        let m = measure_from_json(&json_segment(12.0, 150, 0.04), TrackKind::Video).expect("mesure");
        assert!(approx(m.start, 12.0, 1e-6));
        assert!(approx(m.end, 18.0, 1e-6));
        assert!(approx(m.duration, 6.0, 1e-6));
        assert!(approx(m.frame_dur.unwrap_or(0.0), 0.04, 1e-6));
    }

    #[test]
    fn mesure_json_privilegie_le_flux_de_la_piste() {
        let json = json_segment(0.0, 10, 0.04);
        let a = measure_from_json(&json, TrackKind::Audio).expect("mesure");
        // un seul paquet audio de 0,02 s
        assert!(approx(a.duration, 0.02, 1e-6), "{a:?}");
    }

    #[test]
    fn mesure_json_sans_flux_ou_sans_paquets() {
        assert!(measure_from_json(r#"{"streams":[]}"#, TrackKind::Video).is_err());
        assert!(measure_from_json("pas du json", TrackKind::Video).is_err());
        // Repli sur la durée du conteneur.
        let m = measure_from_json(
            r#"{"streams":[{"index":0,"codec_type":"audio","start_time":"1.5"}],"format":{"duration":"5.0"}}"#,
            TrackKind::Audio,
        )
        .expect("mesure");
        assert!(approx(m.start, 1.5, 1e-9) && approx(m.duration, 5.0, 1e-9));
        // Aucune durée du tout.
        assert!(measure_from_json(r#"{"streams":[{"index":0,"codec_type":"audio"}]}"#, TrackKind::Audio).is_err());
    }

    // ----- tolérances de durée ----------------------------------------------------------------

    #[test]
    fn tolerance_segment_trop_court() {
        // EXTINF 6 s : tolérance max(0,25 ; 1,2) = 1,2 s.
        assert!(!is_too_short(6.0, 4.9));
        assert!(is_too_short(6.0, 4.7));
        // EXTINF 1 s : tolérance 0,25 s.
        assert!(!is_too_short(1.0, 0.8));
        assert!(is_too_short(1.0, 0.7));
        // Segment plus long que prévu : jamais fautif.
        assert!(!is_too_short(6.0, 9.0));
        // EXTINF inconnu.
        assert!(!is_too_short(0.0, 0.0));
    }

    #[test]
    fn tolerance_duree_finale() {
        // 2 h : 0,5 % = 36 s.
        assert!(!duration_shortfall(7200.0, 7170.0));
        assert!(duration_shortfall(7200.0, 7150.0));
        // 60 s : plancher 1 s.
        assert!(!duration_shortfall(60.0, 59.2));
        assert!(duration_shortfall(60.0, 58.5));
        // Plus long que prévu : pas un problème.
        assert!(!duration_shortfall(60.0, 70.0));
        assert!(!duration_shortfall(0.0, 0.0));
    }

    #[test]
    fn tolerance_jonction() {
        assert!(approx(junction_tolerance(0.04), 0.1, 1e-12));
        assert!(approx(junction_tolerance(0.2), 0.4, 1e-12));
    }

    // ----- continuité aux jonctions -----------------------------------------------------------

    fn check(index: u32, start: f64, dur: f64, extinf: f64, disc: bool) -> SegCheck {
        SegCheck {
            index,
            extinf,
            discontinuity_before: disc,
            result: Ok(SegMeasure { start, end: start + dur, duration: dur, frame_dur: Some(0.04) }),
        }
    }

    #[test]
    fn jonctions_continues_aucun_fautif() {
        let c = vec![
            check(0, 0.0, 6.0, 6.0, false),
            check(1, 6.0, 6.0, 6.0, false),
            check(2, 12.0, 6.0, 6.0, false),
        ];
        assert!(evaluate(TrackKind::Video, &c).is_empty());
    }

    #[test]
    fn jonction_trou_designe_le_segment_trop_court() {
        // Le segment 1 ne dure que 3,5 s sur 6 : sa fin laisse un trou de 2,5 s avant le 2.
        let c = vec![
            check(0, 0.0, 6.0, 6.0, false),
            check(1, 6.0, 3.5, 6.0, false),
            check(2, 12.0, 6.0, 6.0, false),
        ];
        let bad = evaluate(TrackKind::Video, &c);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].index, 1);
        assert!(bad[0].reason.contains("3,50"), "{}", bad[0].reason);
        assert!(bad[0].reason.contains("Trou de 2,50 s"), "{}", bad[0].reason);
        assert_eq!(bad[0].track, TrackKind::Video);
    }

    #[test]
    fn jonction_trou_sans_segment_court_designe_le_suivant() {
        // Les deux durées sont correctes mais le suivant commence 1,2 s trop tard.
        let c = vec![check(4, 24.0, 6.0, 6.0, false), check(5, 31.2, 6.0, 6.0, false)];
        let bad = evaluate(TrackKind::Audio, &c);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].index, 5);
        assert_eq!(bad[0].track, TrackKind::Audio);
        assert!(bad[0].reason.contains("Trou de 1,20 s"), "{}", bad[0].reason);
    }

    #[test]
    fn jonction_trou_dans_la_tolerance() {
        // 2 images de 0,04 s = 0,08 < 0,1 s : toléré. 0,12 s : fautif.
        let ok = vec![check(0, 0.0, 6.0, 6.0, false), check(1, 6.08, 6.0, 6.0, false)];
        assert!(evaluate(TrackKind::Video, &ok).is_empty());
        let ko = vec![check(0, 0.0, 6.0, 6.0, false), check(1, 6.12, 6.0, 6.0, false)];
        assert_eq!(evaluate(TrackKind::Video, &ko).len(), 1);
    }

    #[test]
    fn jonction_recouvrement_n_est_pas_une_erreur() {
        let c = vec![check(0, 0.0, 6.0, 6.0, false), check(1, 5.5, 6.0, 6.0, false)];
        assert!(evaluate(TrackKind::Video, &c).is_empty());
        // Timestamps qui repartent de zéro sans discontinuité déclarée : recouvrement massif.
        let c = vec![check(0, 100.0, 6.0, 6.0, false), check(1, 0.0, 6.0, 6.0, false)];
        assert!(evaluate(TrackKind::Video, &c).is_empty());
    }

    #[test]
    fn jonction_discontinuite_declaree_ignoree() {
        let c = vec![check(0, 0.0, 6.0, 6.0, false), check(1, 500.0, 6.0, 6.0, true)];
        assert!(evaluate(TrackKind::Video, &c).is_empty());
    }

    #[test]
    fn segment_absent_vide_ou_illisible() {
        let mut c = vec![
            check(0, 0.0, 6.0, 6.0, false),
            check(1, 0.0, 0.0, 6.0, false),
            check(2, 18.0, 6.0, 6.0, false),
            check(3, 24.0, 6.0, 6.0, false),
        ];
        c[1].result = Err("fichier du segment absent".to_string());
        let bad = evaluate(TrackKind::Video, &c);
        // 1 est fautif ; aucune comparaison de jonction à travers un segment inmesurable.
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].index, 1);
        assert_eq!(bad[0].reason, "Fichier du segment absent");
    }

    #[test]
    fn segment_trop_court_sans_trou() {
        // Durée mesurée 2,0 s pour 6 s annoncées, mais le suivant colle (timestamps recalés).
        let c = vec![check(0, 0.0, 2.0, 6.0, false), check(1, 2.0, 6.0, 6.0, false)];
        let bad = evaluate(TrackKind::Video, &c);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].index, 0);
        assert!(bad[0].reason.contains("2,00 s au lieu de 6,00 s"), "{}", bad[0].reason);
    }

    #[test]
    fn raisons_cumulees_sur_un_meme_segment() {
        let c = vec![check(0, 0.0, 6.0, 6.0, false), check(1, 6.0, 2.0, 6.0, false), check(2, 12.0, 6.0, 6.0, false)];
        let bad = evaluate(TrackKind::Video, &c);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].reason.contains("Durée mesurée") && bad[0].reason.contains("Trou de"), "{}", bad[0].reason);
    }

    // ----- association trou -> segment --------------------------------------------------------

    #[test]
    fn association_trou_segment() {
        let segs: Vec<(u32, f64)> = (0..10).map(|i| (i, 6.0)).collect();
        assert_eq!(locate_segment(&segs, 0.0), Some(0));
        assert_eq!(locate_segment(&segs, 5.99), Some(0));
        assert_eq!(locate_segment(&segs, 6.0), Some(1));
        assert_eq!(locate_segment(&segs, 31.0), Some(5));
        assert_eq!(locate_segment(&segs, 59.9), Some(9));
        assert_eq!(locate_segment(&segs, 60.4), Some(9)); // arrondis
        assert_eq!(locate_segment(&segs, 75.0), None);
        assert_eq!(locate_segment(&[], 1.0), None);
        assert_eq!(locate_segment(&segs, -0.2), Some(0));
        // Index non contigus et durées variables.
        let v = vec![(10, 2.0), (11, 4.0), (12, 3.0)];
        assert_eq!(locate_segment(&v, 1.0), Some(10));
        assert_eq!(locate_segment(&v, 2.0), Some(11));
        assert_eq!(locate_segment(&v, 6.5), Some(12));
    }

    fn facts(
        video: Option<StreamAnalysis>,
        audio: Option<StreamAnalysis>,
        expected: Option<f64>,
        container: Option<f64>,
    ) -> OutputFacts<'static> {
        OutputFacts { expected_duration: expected, container_duration: container, video, audio, tracks: None }
    }

    #[test]
    fn rapport_avec_trou_video_et_position_relative_au_debut() {
        // Fichier démarrant à 1,4 s (ex. MPEG-TS) : `at` est exprimé depuis le début du fichier.
        let mut p = regular(1.4, 0.04, 100);
        p.extend(regular(1.4 + 6.0, 0.04, 100));
        let v = analyze_packets(TrackKind::Video, &p);
        let r = build_report(facts(v, None, Some(10.0), Some(10.0)));
        assert!(!r.ok);
        assert_eq!(r.gaps.len(), 1);
        assert!(approx(r.gaps[0].at, 4.0, 1e-6));
        assert!(approx(r.gaps[0].missing, 2.0, 1e-6));
        assert_eq!(r.gaps[0].stream, TrackKind::Video);
        assert_eq!(r.gaps[0].segment_index, None);
        assert!(r.summary.starts_with("1 coupure détectée : 2,0 s manquante à 00:00:04"), "{}", r.summary);
        assert!(r.bad_segments.is_empty());
        assert_eq!(r.av_drift, None);
    }

    #[test]
    fn rapport_ok_et_resume() {
        let v = analyze_packets(TrackKind::Video, &regular(0.0, 0.04, 250));
        let a = analyze_packets(TrackKind::Audio, &regular(0.0, 0.0232, 430));
        let r = build_report(facts(v, a, Some(10.0), Some(10.0)));
        assert!(r.ok, "{}", r.summary);
        assert!(r.gaps.is_empty());
        assert!(r.av_drift.is_some());
        assert_eq!(r.summary, "Aucune coupure détectée (10 s).");
    }

    #[test]
    fn rapport_sans_audio_ou_sans_video_ne_plante_pas() {
        let v = analyze_packets(TrackKind::Video, &regular(0.0, 0.04, 100));
        let r = build_report(facts(v, None, None, Some(4.0)));
        assert!(r.ok);
        assert_eq!(r.av_drift, None);
        let a = analyze_packets(TrackKind::Audio, &regular(0.0, 0.0232, 200));
        let r = build_report(facts(None, a, None, None));
        assert!(r.ok);
        assert!(approx(r.actual_duration, 200.0 * 0.0232, 1e-6));
        let r = build_report(facts(None, None, Some(10.0), None));
        assert!(!r.ok);
        assert_eq!(r.summary, "Aucun flux audio ou vidéo lisible dans le fichier.");
    }

    #[test]
    fn rapport_duree_trop_courte_et_derive() {
        let v = analyze_packets(TrackKind::Video, &regular(0.0, 0.04, 250)); // fin à 10 s
        let a = analyze_packets(TrackKind::Audio, &regular(0.0, 0.0232, 560)); // fin à 12,99 s
        let r = build_report(facts(v, a, Some(7320.0), Some(10.0)));
        assert!(!r.ok);
        assert!(r.gaps.is_empty());
        assert!(r.av_drift.map(|d| d > 0.5).unwrap_or(false));
        assert!(r.summary.contains("Durée de 10 s inférieure aux 2 h 02 min 00 s attendues."), "{}", r.summary);
        assert!(r.summary.contains("Décalage audio/vidéo de 3,0 s (audio plus long)."), "{}", r.summary);
        // Dérive faible : acceptée.
        let v = analyze_packets(TrackKind::Video, &regular(0.0, 0.04, 250));
        let a = analyze_packets(TrackKind::Audio, &regular(0.0, 0.0232, 450)); // 10,44 s
        let r = build_report(facts(v, a, Some(10.0), Some(10.4)));
        assert!(r.ok, "{}", r.summary);
    }

    #[test]
    fn rapport_associe_les_trous_aux_segments() {
        use std::path::PathBuf;
        let seg = |i: u32| SegmentInfo {
            index: i,
            sequence: i as u64,
            url: String::new(),
            path: PathBuf::new(),
            duration: 6.0,
            discontinuity_before: false,
            bytes: 0,
        };
        let track = |kind: TrackKind, n: u32| TrackInfo {
            kind,
            playlist_url: String::new(),
            local_playlist: PathBuf::new(),
            init_segment: None,
            segments: (0..n).map(seg).collect(),
            expected_duration: n as f64 * 6.0,
        };
        let dl = HlsDownload {
            work_dir: PathBuf::new(),
            video: track(TrackKind::Video, 10),
            audio: Some(track(TrackKind::Audio, 10)),
            width: None,
            height: None,
            bandwidth: None,
        };
        // Trou vidéo de 2 s à 30 s (segment 5) ; trou audio de 1 s à 13 s (segment 2).
        let mut p = regular(0.0, 0.04, 750);
        p.extend(regular(32.0, 0.04, 700));
        let v = analyze_packets(TrackKind::Video, &p);
        let mut q = regular(0.0, 0.0232, 560);
        q.extend(regular(13.0 + 1.0, 0.0232, 1900));
        let a = analyze_packets(TrackKind::Audio, &q);
        let r = build_report(OutputFacts {
            expected_duration: Some(60.0),
            container_duration: Some(60.0),
            video: v,
            audio: a,
            tracks: Some(&dl),
        });
        assert_eq!(r.gaps.len(), 2, "{:?}", r.gaps);
        let g_audio = r.gaps.iter().find(|g| g.stream == TrackKind::Audio).expect("trou audio");
        let g_video = r.gaps.iter().find(|g| g.stream == TrackKind::Video).expect("trou vidéo");
        assert_eq!(g_video.segment_index, Some(5));
        assert_eq!(g_audio.segment_index, Some(2));
        assert!(!r.ok);
        assert!(r.summary.contains("2 coupures détectées"), "{}", r.summary);
    }

    // ----- formatage --------------------------------------------------------------------------

    #[test]
    fn formatage_timecodes() {
        assert_eq!(timecode(0.0), "00:00:00");
        assert_eq!(timecode(751.2), "00:12:31");
        assert_eq!(timecode(3790.0), "01:03:10");
        assert_eq!(timecode(36000.4), "10:00:00");
        assert_eq!(timecode(-5.0), "00:00:00");
        assert_eq!(timecode(f64::NAN), "00:00:00");
    }

    #[test]
    fn formatage_durees() {
        assert_eq!(human_duration(6127.0), "1 h 42 min 07 s");
        assert_eq!(human_duration(725.0), "12 min 05 s");
        assert_eq!(human_duration(42.4), "42 s");
        assert_eq!(human_duration(3600.0), "1 h 00 min 00 s");
        assert_eq!(fmt_secs(0.8), "0,8");
        assert_eq!(fmt_secs(2.0), "2,0");
        assert_eq!(fmt_secs(0.04), "0,04");
        assert!(fr(1.005, 2).contains(','));
    }

    fn gap(stream: TrackKind, at: f64, missing: f64) -> Gap {
        Gap { stream, at, missing, segment_index: None }
    }

    fn summary(gaps: &[Gap], count: Option<usize>) -> String {
        build_summary(&SummaryInput {
            has_streams: true,
            gaps,
            actual_duration: 6127.0,
            expected_duration: None,
            short: false,
            av_drift: None,
            segments_count: count,
        })
    }

    #[test]
    fn resume_sans_coupure() {
        assert_eq!(summary(&[], Some(6127)), "Aucune coupure détectée (1 h 42 min 07 s, 6127 segments).");
        assert_eq!(summary(&[], Some(1)), "Aucune coupure détectée (1 h 42 min 07 s, 1 segment).");
        assert_eq!(summary(&[], None), "Aucune coupure détectée (1 h 42 min 07 s).");
    }

    #[test]
    fn resume_avec_coupures() {
        let g = [gap(TrackKind::Video, 751.0, 0.8), gap(TrackKind::Video, 3790.0, 2.0)];
        assert_eq!(summary(&g, Some(10)), "2 coupures détectées : 0,8 s manquante à 00:12:31, 2,0 s à 01:03:10.");
        let one = [gap(TrackKind::Video, 751.0, 0.8)];
        assert_eq!(summary(&one, None), "1 coupure détectée : 0,8 s manquante à 00:12:31.");
    }

    #[test]
    fn resume_fusionne_le_meme_trou_video_et_audio() {
        let g = [
            gap(TrackKind::Audio, 751.05, 0.78),
            gap(TrackKind::Video, 751.0, 0.8),
            gap(TrackKind::Audio, 4000.0, 1.0),
        ];
        assert_eq!(
            summary(&g, None),
            "2 coupures détectées : 0,8 s manquante à 00:12:31, 1,0 s à 01:06:40 (audio)."
        );
    }

    #[test]
    fn resume_tronque_les_longues_listes() {
        let g: Vec<Gap> = (0..9).map(|i| gap(TrackKind::Video, 100.0 * (i + 1) as f64, 1.0)).collect();
        let s = summary(&g, None);
        assert!(s.starts_with("9 coupures détectées : "), "{s}");
        assert!(s.ends_with(", et 3 autres."), "{s}");
    }

    // ----- intégration ffmpeg (ignorée si les outils sont absents) ------------------------------

    fn tool_available(name: &str) -> bool {
        std::process::Command::new(name)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run_ffmpeg(args: &[&str]) -> bool {
        let out = std::process::Command::new("ffmpeg").args(["-y", "-v", "error"]).args(args).output();
        match out {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                eprintln!("ffmpeg {:?} a échoué : {}", args, String::from_utf8_lossy(&o.stderr));
                false
            }
            Err(e) => {
                eprintln!("ffmpeg introuvable : {e}");
                false
            }
        }
    }

    /// Génère une vidéo de 10 s puis une copie dont on a retiré 2 s (4 s -> 6 s), et vérifie que
    /// seule la seconde est signalée, vers 4 s. Sans ffmpeg/ffprobe dans le PATH, le test est
    /// sauté (message sur stderr) ; `cargo test -- --ignored` n'est pas nécessaire.
    #[test]
    fn integration_ffmpeg_trou_de_deux_secondes() {
        if !tool_available("ffmpeg") || !tool_available("ffprobe") {
            eprintln!("SAUTÉ : ffmpeg/ffprobe absents du PATH");
            return;
        }
        let dir = std::env::temp_dir().join(format!("ursa_verify_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = |n: &str| dir.join(n).to_string_lossy().to_string();
        let (full, a, b, holed) = (p("full.mp4"), p("a.ts"), p("b.ts"), p("holed.mp4"));

        assert!(run_ffmpeg(&[
            "-f", "lavfi", "-i", "testsrc=duration=10:size=320x240:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=10",
            "-c:v", "libx264", "-g", "25", "-bf", "0", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-shortest", &full,
        ]));
        // Deux morceaux (0-4 s et 6-10 s) en conservant les timestamps d'origine.
        assert!(run_ffmpeg(&["-i", &full, "-t", "4", "-c", "copy", "-copyts", &a]));
        assert!(run_ffmpeg(&["-ss", "6", "-i", &full, "-c", "copy", "-copyts", &b]));
        assert!(run_ffmpeg(&["-i", &format!("concat:{a}|{b}"), "-c", "copy", "-copyts", &holed]));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let ctx = JobCtx::noop();
        let ok_report = rt
            .block_on(verify_output(Path::new(&full), Some(10.0), None, &ctx))
            .expect("verify_output (complet)");
        eprintln!("complet : {ok_report:?}");
        assert!(ok_report.ok, "la vidéo complète doit être ok : {}", ok_report.summary);
        assert!(ok_report.gaps.is_empty());
        assert!(approx(ok_report.actual_duration, 10.0, 0.3), "{}", ok_report.actual_duration);

        let ko_report = rt
            .block_on(verify_output(Path::new(&holed), Some(10.0), None, &ctx))
            .expect("verify_output (troué)");
        eprintln!("troué : {ko_report:?}");
        assert!(!ko_report.ok, "la vidéo trouée doit être signalée : {}", ko_report.summary);
        let g = ko_report
            .gaps
            .iter()
            .find(|g| g.stream == TrackKind::Video)
            .expect("un trou vidéo");
        assert!(approx(g.at, 4.0, 0.3), "position du trou : {}", g.at);
        // Selon la version de ffmpeg, `-ss 6 -c copy -copyts` démarre le second morceau à 5 s ou
        // à 6 s (image clé retenue) : le trou réel fait donc 1 s ou 2 s.
        assert!((0.7..=2.3).contains(&g.missing), "durée manquante : {}", g.missing);
        assert!(ko_report.summary.contains("coupure"), "{}", ko_report.summary);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
