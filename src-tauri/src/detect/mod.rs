//! Détection du flux vidéo à partir d'un lien collé par l'utilisateur.
//!
//! Ordre des étapes : lien direct (`direct`), yt-dlp (`probe`), puis navigateur
//! piloté par le protocole DevTools (`browser`).

pub mod browser;
pub mod direct;
pub mod probe;

use crate::model::{HttpCtx, JobCtx};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Hls,
    Dash,
    File,
    /// Laisser yt-dlp télécharger l'URL lui-même.
    Ytdlp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolvedVia {
    Direct,
    Ytdlp,
    Browser,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserChoice {
    /// Chrome si installé, sinon Edge.
    #[default]
    Auto,
    Chrome,
    Edge,
}

#[derive(Clone, Debug, Default)]
pub struct ResolveHint {
    pub referer: Option<String>,
    pub browser: BrowserChoice,
    pub show_browser: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedMedia {
    pub kind: MediaKind,
    /// URL du flux (m3u8/mpd/fichier) ou de la page pour `Ytdlp`.
    pub url: String,
    /// En-têtes à utiliser pour télécharger.
    pub http: HttpCtx,
    pub via: ResolvedVia,
    pub title: Option<String>,
    pub duration: Option<f64>,
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserStatus {
    pub available: bool,
    pub name: Option<String>,
    pub path: Option<String>,
}

/// Navigateur qui serait utilisé pour ce choix (nom « Chrome » / « Edge » et chemin).
pub fn browser_status(choice: BrowserChoice) -> BrowserStatus {
    match browser::locate(choice) {
        Some(b) => BrowserStatus {
            available: true,
            name: Some(b.name.to_string()),
            path: Some(b.path.to_string_lossy().into_owned()),
        },
        None => BrowserStatus {
            available: false,
            name: None,
            path: None,
        },
    }
}

/// Message d'erreur quand aucune étape n'a trouvé de flux.
pub(crate) const MSG_RIEN_TROUVE: &str = "Aucun flux vidéo détecté sur cette page. Causes possibles : la page ne contient pas de vidéo détectable, la lecture nécessite une connexion (compte, abonnement), ou le flux est protégé par DRM, ce que le logiciel ne télécharge pas.";

/// Message d'erreur quand le flux est protégé par DRM.
pub(crate) const MSG_DRM: &str = "Flux protégé par DRM : le logiciel ne télécharge pas les contenus protégés.";

/// Trouve le flux à télécharger pour `input_url` : lien direct, puis yt-dlp, puis navigateur.
pub async fn resolve(input_url: &str, hint: &ResolveHint, ctx: &JobCtx) -> anyhow::Result<ResolvedMedia> {
    let input_url = input_url.trim();
    let parsed = reqwest::Url::parse(input_url).map_err(|_| anyhow::anyhow!("Lien invalide : « {input_url} »"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        anyhow::bail!("Lien invalide : seuls les liens http(s) sont pris en charge.");
    }

    // 1. Lien direct
    ctx.check()?;
    ctx.info("Détection : analyse du lien (lien direct)…");
    match direct::resolve(input_url, hint, ctx).await {
        Ok(Some(media)) => {
            ctx.info(format!("Lien direct reconnu : {:?}", media.kind));
            return Ok(media);
        }
        Ok(None) => ctx.info("Ce n'est pas un lien direct vers un flux."),
        Err(e) => {
            ctx.check()?;
            ctx.warn(format!("Analyse du lien impossible : {e}"));
        }
    }

    // 2. yt-dlp
    ctx.check()?;
    ctx.info("Détection : essai avec yt-dlp…");
    match probe::resolve(input_url, hint, ctx).await {
        Ok(media) => {
            ctx.info(format!(
                "yt-dlp a trouvé la vidéo ({:?}){}",
                media.kind,
                media.title.as_deref().map(|t| format!(" : {t}")).unwrap_or_default()
            ));
            return Ok(media);
        }
        Err(e) => {
            ctx.check()?;
            ctx.warn(format!("yt-dlp n'a pas pu extraire la vidéo : {e}"));
        }
    }

    // 3. Navigateur piloté
    ctx.check()?;
    ctx.info("Détection : ouverture de la page dans le navigateur…");
    match browser::resolve(input_url, hint, ctx).await {
        Ok(Some(media)) => {
            ctx.info(format!("Flux détecté dans la page : {}", media.url));
            return Ok(media);
        }
        Ok(None) => ctx.warn("Aucun flux détecté dans la page."),
        Err(e) => {
            ctx.check()?;
            // Erreurs « définitives » (DRM, navigateur absent, page inaccessible) : on les remonte telles quelles.
            return Err(e);
        }
    }

    // 4. Rien trouvé
    anyhow::bail!(MSG_RIEN_TROUVE)
}
