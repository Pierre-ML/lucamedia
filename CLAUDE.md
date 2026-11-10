## Development

When starting the dev server, use background mode:

```
astro dev --background
```

Manage the background server with `astro dev stop`, `astro dev status`, and `astro dev logs`.

## Documentation

Full documentation: https://docs.astro.build

Consult these guides before working on related tasks:

- [Adding pages, dynamic routes, or middleware](https://docs.astro.build/en/guides/routing/)
- [Working with Astro components](https://docs.astro.build/en/basics/astro-components/)
- [Using React, Vue, Svelte, or other framework components](https://docs.astro.build/en/guides/framework-components/)
- [Adding or managing content](https://docs.astro.build/en/guides/content-collections/)
- [Adding styles or using Tailwind](https://docs.astro.build/en/guides/styling/)
- [Supporting multiple languages](https://docs.astro.build/en/guides/internationalization/)

# Ursa

## Vue d'ensemble

Application desktop **Windows uniquement** (Tauri 2, productName `Ursa`, binaire `ursa`, identifier `com.ursa.app`) de téléchargement de vidéos HLS/m3u8 avec vérification d'intégrité (segments corrompus, trous, dérive audio/vidéo). Sources : HLS direct, fichier direct, DASH et pages via yt-dlp, détection via navigateur piloté. Logiciel de la suite Lucania ; mascotte : un ours.
- `src/` : frontend Astro + Tailwind v4 + daisyUI, TypeScript vanilla
- `src-tauri/` : cœur Rust (file, HLS, vérification, détection), SQLite (`rusqlite`)
- Sidecars : `ffmpeg`, `ffprobe`, `yt-dlp` (`bundle.externalBin`), récupérés par `scripts/fetch-ffmpeg.mjs`
- `scripts/build-updater.mjs`, `name-installers.mjs` : produisent `Ursa-Setup.exe` et `Ursa-Update.exe`
- `src-tauri/windows/` : template NSIS (setup + mode updater) et hooks

## Architecture

```
Frontend (Astro) --invoke(commande)--> Rust (#[tauri::command])
                 <--events (app.emit)-- queue-update / download-progress / log
```

Modules `src-tauri/src/` : `lib.rs` (enregistrement), `model.rs` (HttpCtx, JobCtx, Phase, Progress), `tools.rs` (localisation et lancement sans console des sidecars), `db.rs` (SQLite `<app_data_dir>/ursa.db`), `settings.rs`, `queue.rs` (file persistante, un téléchargement à la fois, pause globale), `ytdlp.rs`, `hls/` (playlist, fetch, assemble, file), `verify.rs` (vérification segments + fichier final, `GapReport`), `detect/` (direct, probe, browser), `updater.rs`.

Pipeline (`queue.rs`) : `Resolving` (`detect::resolve`) puis HLS : `Downloading`, `Verifying` des segments, `Repairing` si besoin (2 tours max), `Assembling`, `Verifying` du fichier (+ un tour de réparation sur les trous localisés) ; File : `download_file` ; Dash/Ytdlp : `ytdlp::download` ; puis `verify_output`. Statut final `done`, `warning` (fichier gardé, rapport stocké) ou `error` (après `maxRetries`, 403 : 3 essais avec re-résolution). Dossier `work/<id>` gardé en cas d'échec (reprise), supprimé après `done`/`warning`.

- Commandes : `get_settings`, `save_settings`, `queue_state`, `queue_add`, `queue_remove`, `queue_retry`, `queue_cancel`, `queue_clear_finished`, `queue_set_paused`, `verify_again`, `reveal_download`, `open_dest_dir`, `tools_status`, `logs_recent`, plus `app_check_update`, `app_install_update`, `app_uninstall`. Toutes renvoient `Result<T, String>`.
- Événements : `queue-update` (`QueueState`), `download-progress` (4/s max), `log` (`LogLine`), `app-update-progress` (`{ downloaded, total }`).
- Données : `<app_data_dir>/ursa.db`, segments dans `<app_data_dir>/work/<id>/`, sortie `<destDir>/<nom>.mp4` (défaut `Vidéos\Ursa`).

## Conventions

- camelCase pour les arguments `invoke` et tout le JSON (`#[serde(rename_all = "camelCase")]`), enums en minuscules.
- Erreurs Rust : `anyhow` en interne, `Result<T, String>` aux frontières des commandes. Pas de `unwrap()` hors tests.
- Tout process externe sans fenêtre console (`CREATE_NO_WINDOW`, via `tools::command`).
- Messages utilisateur et commentaires en français.

## Direction artistique

DA reprise de Lucania : daisyUI + tokens dans `src/styles/global.css`. Thème `lucania` : fonds `#0b0b0c` / `#141416` / `#1f1f22`, texte `#f2f2f3`, primaire blanc cassé, accent rouge `#e5383b` **réservé aux signaux** (erreurs, actions destructives, états critiques). Aucun violet. Classes de composants : `panel`, `field`, `field-label`, `button-primary|ghost|danger`, `pill`, `toggle-switch`, `nav-item`.

Animaux : `src/lib/animals.ts` (viewBox 0 0 64 64) + `src/components/Animal.astro`. Tout en `currentColor`, décoratif (`aria-hidden`), pas d'animation, un animal par section ; le loup reste réservé à Lucania. L'**ours** (propre à Ursa) est le logo et la mascotte.

| Animal | Rôle dans Ursa |
|---|---|
| ours | logo, mascotte, états vides, bandeau de mise à jour, favicon, icône de l'app (`app-icon.svg`) |
| hibou | Vérification / rapport d'intégrité |
| corbeau | Journal |
| cerf | Réglages |

## Lancer en développement / release

```bash
npm install
npm run fetch-ffmpeg -- --from-path   # une fois (ffmpeg, ffprobe, yt-dlp du PATH), ou sans option pour télécharger
npm run desktop:dev                   # = tauri dev
npm run desktop:build                 # tauri build + build-updater + name-installers -> 2 exe
cargo test --manifest-path src-tauri/Cargo.toml
```

Release : chaque push sur `main` dont la version (`tauri.conf.json`, source de vérité) n'a pas de tag `vX.Y.Z` déclenche `ci.yml` (Windows) : tests, `npm run desktop:build`, release GitHub « Ursa vX.Y.Z » avec `Ursa-Setup.exe` et `Ursa-Update.exe` ; sinon simple vérification. Publier : `npm run bump -- patch`, commit, push `main`. Le workflow échoue si les versions (tauri.conf.json, package.json, package-lock.json, Cargo.toml) divergent.

## Points d'attention

- **Installateurs** : `src-tauri/windows/installer.nsi` est un template NSIS personnalisé (modifications marquées `; MDL:`), installMode `currentUser`. Un même script produit le SETUP (`tauri build`, refuse si déjà installé) et l'UPDATE (`build-updater.mjs`, `/DMDL_UPDATER`). Contrat avec `updater.rs` : l'app télécharge `releases/latest/download/Ursa-Update.exe` et le lance avec `/MDL_WAITPID=<pid>`. Ne pas renommer ces assets sans mettre à jour `updater.rs`.
- **Sidecars** : `binaries/<nom>-<triple>.exe` doivent exister avant `tauri dev/build`. À l'exécution : `.exe` à côté de `current_exe()`, sinon le PATH. L'updater annule la file (kill des ffmpeg/yt-dlp qui verrouillent l'installation) via `queue::cancel_all`.
- **Version** : `npm run bump` met à jour package.json, package-lock.json, tauri.conf.json, Cargo.toml/lock.
- `src-tauri/Cargo.toml` : les crates ajoutés par les modules vont sous le commentaire `# deps ajoutées par les modules`.
