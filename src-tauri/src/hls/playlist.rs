// Parseur HLS maison : playlist maître (variantes, pistes audio) et playlist média
// (segments, clés, segment d'init, byterange). Aucune dépendance au réseau : tout est
// testable sur du texte. Les URLs relatives sont résolues contre l'URL de la playlist.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Url;

pub const ERR_LIVE: &str = "Flux en direct non pris en charge";
pub const ERR_DRM: &str = "Flux protégé par DRM, téléchargement impossible";

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: Option<u64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub audio_group: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AudioRendition {
    pub group_id: String,
    pub name: Option<String>,
    pub uri: Option<String>,
    pub default: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub audios: Vec<AudioRendition>,
}

/// Choix de la meilleure qualité dans une playlist maître.
#[derive(Clone, Debug, PartialEq)]
pub struct Selection {
    pub variant: Variant,
    pub audio_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    /// URL absolue de la clé AES-128.
    pub uri: String,
    /// IV explicite ; sinon le numéro de séquence du segment est utilisé.
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    pub length: u64,
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapInfo {
    pub url: String,
    pub byterange: Option<ByteRange>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaSegment {
    pub url: String,
    pub duration: f64,
    pub sequence: u64,
    pub discontinuity_before: bool,
    pub key: Option<KeyInfo>,
    pub byterange: Option<ByteRange>,
    pub map: Option<MapInfo>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlaylist {
    pub segments: Vec<MediaSegment>,
    pub media_sequence: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Parsed {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

impl MediaPlaylist {
    pub fn total_duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
}

/// Découpe une liste d'attributs `A=1,B="x,y",C=0x10` en paires clé/valeur (guillemets retirés).
pub fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && (chars[i] == ',' || chars[i].is_whitespace()) {
            i += 1;
        }
        let ks = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ',' {
            i += 1;
        }
        let key: String = chars[ks..i].iter().collect::<String>().trim().to_string();
        if i >= chars.len() || chars[i] != '=' {
            continue; // attribut sans valeur : ignoré
        }
        i += 1;
        let mut val = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                val.push(chars[i]);
                i += 1;
            }
            i += 1; // guillemet fermant
        } else {
            while i < chars.len() && chars[i] != ',' {
                val.push(chars[i]);
                i += 1;
            }
            val = val.trim().to_string();
        }
        if !key.is_empty() {
            out.push((key.to_uppercase(), val));
        }
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// Résout `uri` contre `base` (URL de la playlist, après redirection).
pub fn resolve_url(base: &str, uri: &str) -> Result<String> {
    let uri = uri.trim();
    if let Ok(abs) = Url::parse(uri) {
        return Ok(abs.to_string());
    }
    let b = Url::parse(base).with_context(|| format!("URL de playlist invalide : {base}"))?;
    Ok(b.join(uri)
        .with_context(|| format!("URL relative invalide : {uri}"))?
        .to_string())
}

fn parse_iv(s: &str) -> Result<[u8; 16]> {
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| anyhow!("IV de chiffrement invalide : {s}"))?;
    if hex.is_empty() || hex.len() > 32 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("IV de chiffrement invalide : {s}");
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (k, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[k * 2..k * 2 + 2], 16)
            .map_err(|_| anyhow!("IV de chiffrement invalide : {s}"))?;
    }
    Ok(iv)
}

/// `n[@o]` ; si `o` est absent, la plage suit la précédente (`prev_end`).
fn parse_byterange(s: &str, prev_end: Option<u64>) -> Result<ByteRange> {
    let mut it = s.trim().splitn(2, '@');
    let length: u64 = it
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .map_err(|_| anyhow!("BYTERANGE invalide : {s}"))?;
    let offset = match it.next() {
        Some(o) => o
            .trim()
            .parse()
            .map_err(|_| anyhow!("BYTERANGE invalide : {s}"))?,
        None => prev_end.unwrap_or(0),
    };
    Ok(ByteRange { length, offset })
}

fn parse_resolution(s: &str) -> (Option<u32>, Option<u32>) {
    let mut it = s.split(['x', 'X']);
    let w = it.next().and_then(|v| v.trim().parse().ok());
    let h = it.next().and_then(|v| v.trim().parse().ok());
    (w, h)
}

/// Analyse une playlist (maître ou média). `base_url` = URL finale de la playlist.
pub fn parse(text: &str, base_url: &str) -> Result<Parsed> {
    let text = text.trim_start_matches('\u{feff}');
    let head: String = text.chars().take(200).collect();
    let head_l = head.trim_start().to_lowercase();
    if head_l.starts_with("<!doctype") || head_l.starts_with("<html") || head_l.starts_with('<') {
        bail!("La réponse est une page web et non une playlist HLS");
    }
    if text.contains("#EXT-X-STREAM-INF") {
        parse_master(text, base_url).map(Parsed::Master)
    } else if text.contains("#EXTINF") {
        parse_media(text, base_url).map(Parsed::Media)
    } else {
        bail!("Playlist HLS vide ou invalide (aucun segment ni variante)");
    }
}

pub fn parse_master(text: &str, base_url: &str) -> Result<MasterPlaylist> {
    let mut master = MasterPlaylist::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attrs(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attrs(rest);
            if attr(&a, "TYPE").map(|t| t.eq_ignore_ascii_case("AUDIO")) != Some(true) {
                continue;
            }
            let uri = match attr(&a, "URI") {
                Some(u) if !u.trim().is_empty() => Some(resolve_url(base_url, u)?),
                _ => None,
            };
            master.audios.push(AudioRendition {
                group_id: attr(&a, "GROUP-ID").unwrap_or("").to_string(),
                name: attr(&a, "NAME").map(str::to_string),
                uri,
                default: attr(&a, "DEFAULT").map(|v| v.eq_ignore_ascii_case("YES")) == Some(true),
            });
        } else if line.starts_with('#') {
            continue;
        } else if let Some(a) = pending.take() {
            let (width, height) = attr(&a, "RESOLUTION")
                .map(parse_resolution)
                .unwrap_or((None, None));
            master.variants.push(Variant {
                url: resolve_url(base_url, line)?,
                bandwidth: attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()),
                width,
                height,
                codecs: attr(&a, "CODECS").map(str::to_string),
                audio_group: attr(&a, "AUDIO").map(str::to_string),
            });
        }
    }
    if master.variants.is_empty() {
        bail!("Playlist maître sans variante");
    }
    Ok(master)
}

fn has_video_codec(codecs: &str) -> bool {
    codecs.split(',').any(|c| {
        let c = c.trim().to_lowercase();
        ["avc", "hvc", "hev", "vp0", "vp8", "vp9", "av01", "dvh", "dvhe", "mp4v"]
            .iter()
            .any(|p| c.starts_with(p))
    })
}

/// Qualité maximale : plus grande hauteur, puis plus grand BANDWIDTH. La piste audio du groupe
/// référencé est DEFAULT=YES sinon la première ; ignorée si elle n'a pas d'URI (audio muxé).
pub fn pick_best(master: &MasterPlaylist) -> Option<Selection> {
    // On écarte les variantes audio seules, sauf si c'est tout ce qu'il y a
    let video: Vec<&Variant> = master
        .variants
        .iter()
        .filter(|v| v.height.is_some() || v.codecs.as_deref().map(has_video_codec).unwrap_or(true))
        .collect();
    let pool: Vec<&Variant> = if video.is_empty() {
        master.variants.iter().collect()
    } else {
        video
    };
    // `max_by_key` garde le dernier ex æquo : on parcourt à l'envers pour garder le premier
    let best = pool
        .iter()
        .rev()
        .max_by_key(|v| (v.height.unwrap_or(0), v.bandwidth.unwrap_or(0)))?;
    let audio_url = best.audio_group.as_ref().and_then(|g| {
        let group: Vec<&AudioRendition> = master.audios.iter().filter(|a| &a.group_id == g).collect();
        let chosen = group.iter().find(|a| a.default).or_else(|| group.first())?;
        chosen.uri.clone()
    });
    Some(Selection {
        variant: (*best).clone(),
        audio_url,
    })
}

pub fn parse_media(text: &str, base_url: &str) -> Result<MediaPlaylist> {
    let mut segments: Vec<MediaSegment> = Vec::new();
    let mut media_sequence: u64 = 0;
    let mut end_list = false;

    let mut duration: Option<f64> = None;
    let mut discontinuity = false;
    let mut key: Option<KeyInfo> = None;
    let mut map: Option<MapInfo> = None;
    let mut pending_range: Option<String> = None;
    // fin de la dernière plage par ressource (pour les BYTERANGE sans offset)
    let mut last_end: Option<(String, u64)> = None;

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            media_sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let d = rest.split(',').next().unwrap_or("").trim();
            duration = Some(
                d.parse::<f64>()
                    .map_err(|_| anyhow!("Durée EXTINF invalide : {d}"))?,
            );
        } else if line.starts_with("#EXT-X-DISCONTINUITY-SEQUENCE") {
            continue;
        } else if line.starts_with("#EXT-X-DISCONTINUITY") {
            discontinuity = true;
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attrs(rest);
            let method = attr(&a, "METHOD").unwrap_or("NONE").to_uppercase();
            match method.as_str() {
                "NONE" => key = None,
                "AES-128" => {
                    let uri = attr(&a, "URI")
                        .filter(|u| !u.trim().is_empty())
                        .ok_or_else(|| anyhow!("Clé AES-128 sans URI"))?;
                    let iv = match attr(&a, "IV") {
                        Some(v) if !v.is_empty() => Some(parse_iv(v)?),
                        _ => None,
                    };
                    key = Some(KeyInfo {
                        uri: resolve_url(base_url, uri)?,
                        iv,
                    });
                }
                _ => bail!("{ERR_DRM}"),
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attrs(rest);
            let uri = attr(&a, "URI").ok_or_else(|| anyhow!("EXT-X-MAP sans URI"))?;
            let url = resolve_url(base_url, uri)?;
            let byterange = match attr(&a, "BYTERANGE") {
                Some(r) => Some(parse_byterange(r, None)?),
                None => None,
            };
            map = Some(MapInfo { url, byterange });
        } else if line == "#EXT-X-ENDLIST" {
            end_list = true;
        } else if line.starts_with('#') {
            continue;
        } else {
            let d = duration
                .take()
                .ok_or_else(|| anyhow!("Segment sans EXTINF : {line}"))?;
            let url = resolve_url(base_url, line)?;
            let byterange = match pending_range.take() {
                Some(r) => {
                    let prev = last_end
                        .as_ref()
                        .filter(|(u, _)| *u == url)
                        .map(|(_, e)| *e);
                    let br = parse_byterange(&r, prev)?;
                    last_end = Some((url.clone(), br.offset + br.length));
                    Some(br)
                }
                None => None,
            };
            let index = segments.len() as u64;
            segments.push(MediaSegment {
                url,
                duration: d,
                sequence: media_sequence + index,
                discontinuity_before: std::mem::take(&mut discontinuity),
                key: key.clone(),
                byterange,
                map: map.clone(),
            });
        }
    }

    if !end_list {
        bail!("{ERR_LIVE}");
    }
    if segments.is_empty() {
        bail!("Playlist HLS sans aucun segment");
    }
    Ok(MediaPlaylist {
        segments,
        media_sequence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"fr\",DEFAULT=NO,URI=\"audio_fr.m3u8\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"en\",DEFAULT=YES,URI=\"/abs/audio_en.m3u8\"\n\
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"sub\",NAME=\"s\",URI=\"sub.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"\n\
low/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\"\n\
hd/index.m3u8?tok=1,2\n\
#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,AUDIO=\"aud\"\n\
mid/index.m3u8\n\
#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160,URI=\"iframe.m3u8\"\n";

    #[test]
    fn master_choisit_la_plus_haute_qualite_et_l_audio_par_defaut() {
        let p = parse(MASTER, "https://cdn.example.com/v/master.m3u8").unwrap();
        let Parsed::Master(m) = p else { panic!("maître attendu") };
        assert_eq!(m.variants.len(), 3);
        assert_eq!(m.audios.len(), 2);
        let sel = pick_best(&m).unwrap();
        assert_eq!(sel.variant.height, Some(1080));
        assert_eq!(sel.variant.width, Some(1920));
        assert_eq!(sel.variant.bandwidth, Some(5_000_000));
        assert_eq!(sel.variant.url, "https://cdn.example.com/v/hd/index.m3u8?tok=1,2");
        assert_eq!(sel.audio_url.as_deref(), Some("https://cdn.example.com/abs/audio_en.m3u8"));
    }

    #[test]
    fn master_departage_par_bandwidth_et_audio_sans_defaut() {
        let t = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",URI=\"a1.m3u8\"\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"y\",URI=\"a2.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=1000,RESOLUTION=1280x720,AUDIO=\"a\"\n\
p1.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=3000,RESOLUTION=1280x720,AUDIO=\"a\"\n\
p2.m3u8\n";
        let Parsed::Master(m) = parse(t, "http://h/m.m3u8").unwrap() else { panic!() };
        let sel = pick_best(&m).unwrap();
        assert_eq!(sel.variant.url, "http://h/p2.m3u8");
        assert_eq!(sel.audio_url.as_deref(), Some("http://h/a1.m3u8"));
    }

    #[test]
    fn master_audio_sans_uri_ignore_et_variante_audio_seule_ecartee() {
        let t = "#EXTM3U\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",DEFAULT=YES\n\
#EXT-X-STREAM-INF:BANDWIDTH=64000,CODECS=\"mp4a.40.2\"\n\
audio.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"a\"\n\
video.m3u8\n";
        let Parsed::Master(m) = parse(t, "http://h/m.m3u8").unwrap() else { panic!() };
        let sel = pick_best(&m).unwrap();
        assert_eq!(sel.variant.url, "http://h/video.m3u8");
        assert_eq!(sel.audio_url, None);
    }

    const MEDIA: &str = "#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-TARGETDURATION:10\n\
#EXT-X-MEDIA-SEQUENCE:100\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090A0B0C0D0E0F\n\
#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
#EXTINF:9.5,titre\n\
#EXT-X-BYTERANGE:1000@720\n\
media.mp4\n\
#EXTINF:10.0,\n\
#EXT-X-BYTERANGE:2000\n\
media.mp4\n\
#EXT-X-DISCONTINUITY\n\
#EXT-X-KEY:METHOD=NONE\n\
#EXTINF:4.25,\n\
seg3.ts\n\
#EXT-X-ENDLIST\n";

    #[test]
    fn media_sequence_discontinuite_cle_iv_byterange_map() {
        let Parsed::Media(m) = parse(MEDIA, "https://h.test/a/b/list.m3u8?x=1").unwrap() else {
            panic!("média attendu")
        };
        assert_eq!(m.media_sequence, 100);
        assert_eq!(m.segments.len(), 3);
        assert!((m.total_duration() - 23.75).abs() < 1e-9);
        let s0 = &m.segments[0];
        assert_eq!(s0.sequence, 100);
        assert_eq!(s0.url, "https://h.test/a/b/media.mp4");
        assert!(!s0.discontinuity_before);
        let k = s0.key.as_ref().unwrap();
        assert_eq!(k.uri, "https://h.test/a/b/key.bin");
        assert_eq!(k.iv.unwrap()[15], 0x0f);
        assert_eq!(k.iv.unwrap()[0], 0x00);
        assert_eq!(s0.byterange, Some(ByteRange { length: 1000, offset: 720 }));
        let map = s0.map.as_ref().unwrap();
        assert_eq!(map.url, "https://h.test/a/b/init.mp4");
        assert_eq!(map.byterange, Some(ByteRange { length: 720, offset: 0 }));
        // offset implicite : suite de la plage précédente
        assert_eq!(m.segments[1].byterange, Some(ByteRange { length: 2000, offset: 1720 }));
        assert_eq!(m.segments[1].sequence, 101);
        let s2 = &m.segments[2];
        assert!(s2.discontinuity_before);
        assert!(s2.key.is_none());
        assert!(s2.byterange.is_none());
    }

    #[test]
    fn media_iv_court_complete_a_gauche() {
        let iv = parse_iv("0x1F").unwrap();
        assert_eq!(iv[15], 0x1f);
        assert!(iv[..15].iter().all(|b| *b == 0));
        assert!(parse_iv("zz").is_err());
        assert!(parse_iv("0x").is_err());
    }

    #[test]
    fn rejette_le_direct() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:5\n#EXTINF:6.0,\ns5.ts\n#EXTINF:6.0,\ns6.ts\n";
        let e = parse(t, "http://h/l.m3u8").unwrap_err();
        assert_eq!(e.to_string(), "Flux en direct non pris en charge");
    }

    #[test]
    fn rejette_le_drm() {
        for method in ["SAMPLE-AES", "SAMPLE-AES-CTR"] {
            let t = format!(
                "#EXTM3U\n#EXT-X-KEY:METHOD={method},URI=\"skd://k\"\n#EXTINF:6.0,\ns.ts\n#EXT-X-ENDLIST\n"
            );
            let e = parse(&t, "http://h/l.m3u8").unwrap_err();
            assert_eq!(e.to_string(), "Flux protégé par DRM, téléchargement impossible");
        }
    }

    #[test]
    fn rejette_html_et_playlist_vide() {
        assert!(parse("<!DOCTYPE html><html></html>", "http://h/l.m3u8").is_err());
        assert!(parse("#EXTM3U\n", "http://h/l.m3u8").is_err());
    }

    #[test]
    fn attributs_avec_virgules_dans_les_guillemets() {
        let a = parse_attrs("BANDWIDTH=10,CODECS=\"a,b\",NAME=\"x\"");
        assert_eq!(a.len(), 3);
        assert_eq!(attr(&a, "CODECS"), Some("a,b"));
        assert_eq!(attr(&a, "BANDWIDTH"), Some("10"));
    }
}
