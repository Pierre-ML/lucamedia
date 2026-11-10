//! Localisation et lancement des outils externes (sidecars) : ffmpeg, ffprobe, yt-dlp.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

#[derive(Clone, Copy, Debug)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
    YtDlp,
}

impl Tool {
    /// Nom d'affichage.
    pub fn label(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
            Tool::YtDlp => "yt-dlp",
        }
    }

    fn exe_name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Tool::Ffmpeg, true) => "ffmpeg.exe",
            (Tool::Ffmpeg, false) => "ffmpeg",
            (Tool::Ffprobe, true) => "ffprobe.exe",
            (Tool::Ffprobe, false) => "ffprobe",
            (Tool::YtDlp, true) => "yt-dlp.exe",
            (Tool::YtDlp, false) => "yt-dlp",
        }
    }
}

/// Cherche l'outil à côté de l'exécutable, puis dans le PATH.
pub fn tool_path(t: Tool) -> Option<PathBuf> {
    let name = t.exe_name();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Commande sans fenêtre console, tuée si le futur est abandonné.
pub fn command(t: Tool) -> anyhow::Result<Command> {
    let path = tool_path(t).ok_or_else(|| {
        anyhow::anyhow!(
            "{} est introuvable (ni à côté de l'application, ni dans le PATH)",
            t.label()
        )
    })?;
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    Ok(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_names() {
        assert!(Tool::YtDlp.exe_name().starts_with("yt-dlp"));
        assert!(Tool::Ffmpeg.exe_name().starts_with("ffmpeg"));
        assert!(Tool::Ffprobe.exe_name().starts_with("ffprobe"));
        assert_eq!(Tool::YtDlp.label(), "yt-dlp");
    }

    #[test]
    fn missing_tool_error_is_french() {
        // Ne dépend pas de la présence réelle des outils : si absent, le message est en français.
        match command(Tool::Ffprobe) {
            Ok(_) => {}
            Err(e) => assert!(e.to_string().contains("introuvable")),
        }
    }
}
