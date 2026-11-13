<div align="center"><img src="app-icon.svg" alt="Lucamedia" width="96" /><h1>Lucamedia</h1><p>Un téléchargeur de vidéos HLS / m3u8 qui vérifie que le fichier final est intact.<br/>An HLS / m3u8 video downloader that checks the final file is intact.</p>
<a href="https://github.com/Pierre-ML/telechargment_m3u8/releases/latest/download/Lucamedia-Setup.exe"><b>⬇ Télécharger pour Windows · Download for Windows</b></a><br/><sub>setup pour installer · update pour mettre à jour / setup to install · update to update · <a href="https://github.com/Pierre-ML/telechargment_m3u8/releases/latest">releases</a></sub></div>

<p align="center">
<a href="https://github.com/Pierre-ML/telechargment_m3u8/releases/latest"><img src="https://img.shields.io/github/v/release/Pierre-ML/telechargment_m3u8" alt="Dernière release" /></a>
<img src="https://img.shields.io/badge/plateforme-Windows-blue" alt="Windows" />
<a href="https://github.com/Pierre-ML/telechargment_m3u8/releases"><img src="https://img.shields.io/github/downloads/Pierre-ML/telechargment_m3u8/total" alt="Téléchargements" /></a>
</p>

<p align="center"><a href="#français">Français</a> · <a href="#english">English</a></p>

<!-- TODO: capture d'écran -->

---

# Français

## Présentation

Lucamedia est une application de bureau **Windows** qui télécharge des vidéos **HLS / m3u8** et vérifie que le résultat est intact : segments corrompus, trous dans la durée, dérive entre l'audio et la vidéo. Elle fait partie de la suite Lucania. Son ours est la mascotte.

Le moteur de téléchargement est écrit en Rust. ffmpeg ne sert qu'à assembler le fichier final, sans ré-encodage. Rien ne transite par un serveur tiers.

## Fonctionnalités

- 📥 Téléchargement HLS : playlists maîtresses et média, variante la plus haute, chiffrement AES-128, segments fMP4 ou MPEG-TS, pistes audio séparées
- ⚡ Segments téléchargés en parallèle, avec reprise
- 🔗 Autres sources : fichiers directs, DASH et pages gérées par yt-dlp
- 🔍 Détection automatique du flux, y compris avec un navigateur piloté (Chrome, sinon Edge) quand la page cache l'URL du m3u8
- 🦉 Vérification d'intégrité : segments corrompus, trous, dérive audio/vidéo, avec un rapport lisible
- 🔧 Réparation automatique des segments fautifs
- 📋 File d'attente persistante : un téléchargement à la fois, pause globale, reprise après redémarrage, nouvelle tentative automatique
- 🎞️ Sortie MP4 assemblée par ffmpeg, sans ré-encodage
- 🪵 Journal des opérations
- 🔄 Mise à jour depuis l'application
- 💾 Données stockées localement (SQLite)

## Installation

1. Pour une première installation, téléchargez [`Lucamedia-Setup.exe`](https://github.com/Pierre-ML/telechargment_m3u8/releases/latest/download/Lucamedia-Setup.exe) (lien direct, dernière release).
2. Lancez l'installateur. Lucamedia s'installe pour votre compte Windows, dans `%LOCALAPPDATA%\Programs\Lucamedia`. Aucune autorisation administrateur n'est demandée.

Prérequis : Windows 10 ou 11 (64 bits). ffmpeg, ffprobe et yt-dlp sont embarqués.

> **Antivirus.** L'exécutable n'est pas encore signé. Windows SmartScreen ou certains antivirus peuvent donc l'analyser ou afficher un avertissement.

Le nom du fichier ne change jamais : une seule exception dans votre antivirus suffit pour toutes les versions.

## Premiers pas

1. Ouvrez Lucamedia et collez l'adresse d'un flux m3u8, d'un fichier vidéo ou d'une page contenant une vidéo.
2. Lancez le téléchargement. Suivez l'avancement et la vérification dans la file d'attente.
3. Récupérez le fichier MP4 dans `Vidéos\Lucamedia` (dossier modifiable dans les Réglages).

Si la page cache l'URL du flux, une fenêtre de navigateur s'ouvre : lancez la lecture de la vidéo, Lucamedia la récupère automatiquement.

## Mise à jour

Depuis l'application : elle vérifie la dernière release GitHub, télécharge l'updater, se ferme, puis l'updater copie les fichiers par-dessus l'installation et relance Lucamedia.

À la main : téléchargez [`Lucamedia-Update.exe`](https://github.com/Pierre-ML/telechargment_m3u8/releases/latest/download/Lucamedia-Update.exe) (lien direct, dernière release) et lancez-le.

Vos réglages, votre file d'attente et vos vidéos sont conservés. Un téléchargement en cours est annulé avant la mise à jour.

Repartir de zéro : désinstallez Lucamedia depuis les Réglages de l'application ou ceux de Windows (cela supprime la base et les réglages, jamais les vidéos téléchargées), puis relancez le setup.

## Développement

Prérequis : Windows 10 ou 11 avec WebView2 et les Build Tools MSVC, Node 22 ou plus, Rust stable.

| Commande | Rôle |
| --- | --- |
| `npm install` | Installer les dépendances |
| `npm run fetch-ffmpeg -- --from-path` | Copier ffmpeg, ffprobe et yt-dlp du PATH (sans option : téléchargement) |
| `npm run desktop:dev` | App de bureau en dev (= `tauri dev`) |
| `npm run desktop:build` | Produire les deux exe Windows (`Lucamedia-Setup.exe` et `Lucamedia-Update.exe`, dans `src-tauri/target/release/bundle/release/`) |
| `npm run check` | Vérifier les types |
| `npm run bump -- patch` | Mettre à jour la version partout (`patch`, `minor` ou `major`) |
| `cargo test --manifest-path src-tauri/Cargo.toml` | Lancer les tests Rust |

`npm run dev` seul affiche l'interface, mais le moteur est dans l'app de bureau : il faut `npm run desktop:dev`.

Les sidecars (`ffmpeg`, `ffprobe`, `yt-dlp`) doivent exister dans `src-tauri/binaries/` avant `tauri dev` ou `tauri build`.

Version : source unique dans `src-tauri/tauri.conf.json`, identique dans `package.json`, `package-lock.json` et `src-tauri/Cargo.toml`. Chaque push sur `main` dont la version n'a pas encore de tag `vX.Y.Z` déclenche le workflow GitHub (`.github/workflows/ci.yml`, runner Windows) : vérification des versions, tests, build, puis release « Lucamedia vX.Y.Z » avec les deux exe. Pour publier : `npm run bump -- patch`, commit, push sur `main`.

## Stack

Astro · TypeScript vanilla · Tailwind CSS v4 · daisyUI 5 · SQLite (`rusqlite`) · Tauri v2 · Rust · ffmpeg, ffprobe, yt-dlp.

## Structure

- `src` : interface (pages, composants, scripts, animaux en SVG, styles).
- `src-tauri/src` : cœur Rust (file d'attente, HLS, vérification, détection, base SQLite, mise à jour).
- `src-tauri/windows` : template NSIS personnalisé (setup et mise à jour) et hooks.
- `src-tauri/binaries` : sidecars récupérés par `scripts/fetch-ffmpeg.mjs`.
- `scripts` : récupération des sidecars, build de l'updater, nom des installateurs, gestion de la version.

## Confidentialité

Vos vidéos et vos données restent sur votre machine. Lucamedia ne contacte que :

- les sites des vidéos que vous lui demandez de télécharger ;
- GitHub, pour vérifier et télécharger les mises à jour.

Les données sont dans `%APPDATA%\com.lucamedia.app` : base `lucamedia.db`, réglages et dossier de travail des téléchargements en cours (le cache WebView2 est dans `%LOCALAPPDATA%\com.lucamedia.app`).

Lucamedia ne contourne pas les protections DRM.

---

# English

## Overview

Lucamedia is a **Windows** desktop app that downloads **HLS / m3u8** videos and checks the result is intact: corrupted segments, gaps in duration, audio/video drift. It is part of the Lucania suite. Its bear is the mascot.

The download engine is written in Rust. ffmpeg is only used to assemble the final file, without re-encoding. Nothing goes through a third-party server.

## Features

- 📥 HLS download: master and media playlists, highest variant, AES-128 encryption, fMP4 or MPEG-TS segments, separate audio tracks
- ⚡ Segments downloaded in parallel, with resume
- 🔗 Other sources: direct files, DASH and pages handled by yt-dlp
- 🔍 Automatic stream detection, including with a driven browser (Chrome, otherwise Edge) when the page hides the m3u8 URL
- 🦉 Integrity check: corrupted segments, gaps, audio/video drift, with a readable report
- 🔧 Automatic repair of faulty segments
- 📋 Persistent queue: one download at a time, global pause, resume after restart, automatic retry
- 🎞️ MP4 output assembled by ffmpeg, without re-encoding
- 🪵 Operation log
- 🔄 In-app update
- 💾 Data stored locally (SQLite)

## Installation

1. For a first install, download [`Lucamedia-Setup.exe`](https://github.com/Pierre-ML/telechargment_m3u8/releases/latest/download/Lucamedia-Setup.exe) (direct link, latest release).
2. Run the installer. Lucamedia installs for your Windows account, in `%LOCALAPPDATA%\Programs\Lucamedia`. No administrator permission is asked.

Requirements: Windows 10 or 11 (64-bit). ffmpeg, ffprobe and yt-dlp are bundled.

> **Antivirus.** The executable is not signed yet. Windows SmartScreen or some antivirus tools may scan it or show a warning.

The file name never changes: a single exception in your antivirus is enough for all versions.

## Getting started

1. Open Lucamedia and paste the address of an m3u8 stream, a video file or a page containing a video.
2. Start the download. Follow progress and verification in the queue.
3. Find the MP4 file in `Videos\Lucamedia` (folder can be changed in Settings).

If the page hides the stream URL, a browser window opens: start playing the video and Lucamedia picks it up automatically.

## Updating

From the app: it checks the latest GitHub release, downloads the updater, closes, then the updater copies the files over the installation and relaunches Lucamedia.

By hand: download [`Lucamedia-Update.exe`](https://github.com/Pierre-ML/telechargment_m3u8/releases/latest/download/Lucamedia-Update.exe) (direct link, latest release) and run it.

Your settings, queue and videos are kept. A running download is cancelled before the update.

Start from scratch: uninstall Lucamedia from the app Settings or from Windows Settings (this deletes the database and settings, never the downloaded videos), then run the setup again.

## Development

Requirements: Windows 10 or 11 with WebView2 and the MSVC Build Tools, Node 22 or newer, stable Rust.

| Command | Purpose |
| --- | --- |
| `npm install` | Install dependencies |
| `npm run fetch-ffmpeg -- --from-path` | Copy ffmpeg, ffprobe and yt-dlp from the PATH (no option: download) |
| `npm run desktop:dev` | Desktop app in dev (= `tauri dev`) |
| `npm run desktop:build` | Build both Windows exes (`Lucamedia-Setup.exe` and `Lucamedia-Update.exe`, in `src-tauri/target/release/bundle/release/`) |
| `npm run check` | Type check |
| `npm run bump -- patch` | Set the version everywhere (`patch`, `minor` or `major`) |
| `cargo test --manifest-path src-tauri/Cargo.toml` | Run the Rust tests |

`npm run dev` alone shows the interface, but the engine lives in the desktop app: use `npm run desktop:dev`.

The sidecars (`ffmpeg`, `ffprobe`, `yt-dlp`) must exist in `src-tauri/binaries/` before `tauri dev` or `tauri build`.

Version: single source in `src-tauri/tauri.conf.json`, identical in `package.json`, `package-lock.json` and `src-tauri/Cargo.toml`. Every push to `main` whose version has no `vX.Y.Z` tag yet triggers the GitHub workflow (`.github/workflows/ci.yml`, Windows runner): version check, tests, build, then the "Lucamedia vX.Y.Z" release with both exes. To publish: `npm run bump -- patch`, commit, push to `main`.

## Stack

Astro · vanilla TypeScript · Tailwind CSS v4 · daisyUI 5 · SQLite (`rusqlite`) · Tauri v2 · Rust · ffmpeg, ffprobe, yt-dlp.

## Structure

- `src`: interface (pages, components, scripts, SVG animals, styles).
- `src-tauri/src`: Rust core (queue, HLS, verification, detection, SQLite database, update).
- `src-tauri/windows`: custom NSIS template (setup and update) and hooks.
- `src-tauri/binaries`: sidecars fetched by `scripts/fetch-ffmpeg.mjs`.
- `scripts`: sidecar fetching, updater build, installer naming, version management.

## Privacy

Your videos and data stay on your machine. Lucamedia only contacts:

- the sites of the videos you ask it to download;
- GitHub, to check and download updates.

Data lives in `%APPDATA%\com.lucamedia.app`: `lucamedia.db` database, settings and the work folder of running downloads (the WebView2 cache is in `%LOCALAPPDATA%\com.lucamedia.app`).

Lucamedia does not bypass DRM protection.
