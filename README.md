# Ursa

Application de bureau **Windows** (Tauri 2 + Astro + Rust) pour télécharger des vidéos **HLS / m3u8** et vérifier que le résultat est intact. Ursa fait partie de la suite Lucania ; son ours low-poly est la mascotte.

## Fonctionnalités

- **Téléchargement HLS** : playlists maîtresses et média, variante la plus haute, chiffrement AES-128, segments fMP4 ou MPEG-TS, pistes audio séparées, téléchargement parallèle des segments avec reprise.
- **Autres sources** : fichiers directs, DASH et pages gérées par yt-dlp ; détection automatique du flux, y compris via un navigateur (Chrome ou Edge) quand la page cache l'URL du m3u8.
- **Vérification d'intégrité** : segments corrompus, trous dans la durée, dérive audio/vidéo ; rapport lisible (hibou) et réparation automatique des segments fautifs.
- **File d'attente persistante** : un téléchargement à la fois, pause globale, reprise après redémarrage, nouvelle tentative automatique.
- **Sortie MP4** assemblée par ffmpeg sans ré-encodage.

## Direction artistique

Interface sombre reprise de [Lucania](https://github.com/Pierre-ML/Lucania) (daisyUI + Tailwind v4) : fonds `#0b0b0c` / `#141416` / `#1f1f22`, texte `#f2f2f3`, accent rouge `#e5383b` réservé aux signaux. Animaux low-poly en `currentColor` : ours (logo, mascotte, états vides), hibou (vérification), corbeau (journal), cerf (réglages).

## Installation

Windows uniquement. Télécharger `Ursa-Setup.exe` sur la [dernière release](https://github.com/Pierre-ML/telechargment_m3u8/releases/latest) et le lancer : installation pour l'utilisateur courant (sans droits administrateur) dans `%LOCALAPPDATA%\Programs\Ursa`. ffmpeg, ffprobe et yt-dlp sont embarqués.

Les mises à jour se font depuis l'application : elle télécharge `Ursa-Update.exe` (publié avec chaque release), se ferme, puis l'updater copie les fichiers par-dessus l'installation et relance Ursa. Données conservées : base `ursa.db` et réglages dans `%APPDATA%\com.ursa.app`.

Par défaut, les vidéos sont enregistrées dans `Vidéos\Ursa`.

## Développement

Prérequis : Windows 10/11 avec WebView2 et les Build Tools MSVC, Node 22+, Rust stable.

```bash
npm install
npm run fetch-ffmpeg -- --from-path   # copie ffmpeg, ffprobe et yt-dlp du PATH (sans option : téléchargement)
npm run desktop:dev                   # lance l'app en développement (= tauri dev)
npm run desktop:build                 # tauri build + build-updater + name-installers -> 2 exe
cargo test --manifest-path src-tauri/Cargo.toml
```

Résultat de `desktop:build` dans `src-tauri/target/release/bundle/release/` : `Ursa-Setup.exe` (première installation) et `Ursa-Update.exe` (mise à jour, lancée par l'app avec `/MDL_WAITPID=<pid>`). Le template NSIS personnalisé est dans `src-tauri/windows/`.

## Release

Chaque push sur `main` dont la version (`src-tauri/tauri.conf.json`) n'a pas encore de tag `vX.Y.Z` déclenche `ci.yml` (runner Windows) : vérification que toutes les versions concordent, tests, build, puis release GitHub « Ursa vX.Y.Z » avec `Ursa-Setup.exe` et `Ursa-Update.exe`. Pour publier : `npm run bump -- patch`, commit, push sur `main`.
