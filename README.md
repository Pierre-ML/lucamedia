# Téléchargeur m3u8

Application **100 % locale** de gestion de file d'attente pour télécharger des vidéos HLS (`.m3u8`) via [yt-dlp](https://github.com/yt-dlp/yt-dlp). Aucune donnée n'est envoyée en ligne : le frontend (Astro + Tailwind) et le backend (Express + SQLite) tournent sur votre machine.

## Fonctionnalités

- File d'attente persistante (SQLite, fichier `queue.db`) — survit à la fermeture de l'app et du navigateur
- Téléchargement **séquentiel** (un seul à la fois), avec progression en temps réel (SSE) : %, poids estimé, temps restant
- **Pause / Reprendre** global : la pause n'interrompt pas le téléchargement en cours, elle empêche seulement le suivant de démarrer ; l'état survit à un redémarrage
- **Retry automatique** : si yt-dlp échoue (exit code ≠ 0), la même entrée est relancée jusqu'à 10 fois avant de passer en erreur, puis la file continue avec l'élément suivant
- Ajout en masse (une URL par ligne), suppression des éléments en attente, boutons Réessayer / Supprimer sur les erreurs, historique des téléchargements terminés
- Case à cocher **« Vidéo uqload »** : ajoute automatiquement le Referer nécessaire pour les liens uqload
- **Console yt-dlp intégrée** : la sortie du process s'affiche en direct dans l'interface (masquable, bouton Effacer)
- **Qualité maximale forcée** : chaque téléchargement passe `-f "bv*+ba/b"` à yt-dlp (meilleure vidéo + meilleur audio disponibles)
- **Certificats TLS cassés tolérés** (`--no-check-certificates`) : indispensable avec la plupart des hébergeurs de streaming, dont les certificats provoquent sinon des erreurs `CERTIFICATE_VERIFY_FAILED`

## Prérequis

1. **Node.js ≥ 22.12**
2. **yt-dlp** installé et **dans le PATH** :
   - via winget : `winget install yt-dlp.yt-dlp`
   - vérifiez avec `yt-dlp --version` dans un nouveau terminal
3. (Recommandé) **ffmpeg** dans le PATH — yt-dlp s'en sert pour assembler les segments HLS : `winget install Gyan.FFmpeg`

## Installation

```bash
npm install
```

Aucune configuration : le backend écoute sur le port **3000** et les vidéos vont dans le dossier **`downloads/`** du projet. Les quelques constantes (port, dossier, Referer uqload, nombre de tentatives) sont regroupées dans [`server/config.js`](server/config.js) si besoin de les changer — notamment `uqloadReferer` si le domaine uqload change un jour (actuellement `uqload.cx`).

## Lancer l'app

### Développement (rechargement à chaud)

```bash
npm run dev
```

Lance en parallèle le backend Express (port 3000) et le serveur de dev Astro : ouvrez **http://localhost:4321** (les appels `/api` sont proxifiés vers le backend).

### Production (un seul process)

```bash
npm run start
```

Construit le frontend puis le sert avec l'API depuis le même serveur : ouvrez **http://localhost:3000**.

> Le backend doit tourner en continu pour que la file avance : c'est lui qui pilote yt-dlp, pas le navigateur. Vous pouvez fermer l'onglet, les téléchargements continuent.

## Architecture

```
server/
  index.js     API Express (+ sert le build Astro en production)
  worker.js    Boucle de traitement : spawn yt-dlp, parsing progression, retries
  db.js        SQLite (better-sqlite3) : table downloads + settings
  sse.js       Diffusion Server-Sent Events vers le navigateur
  config.js    Constantes (port, dossier de sortie, Referer uqload, retries)
src/
  pages/index.astro      Page unique
  components/            Formulaire, téléchargement en cours, listes
  scripts/app.js         Logique client (fetch API + SSE + rendu)
downloads/     Dossier de sortie des vidéos
queue.db       Base SQLite (créée au premier lancement, ignorée par git)
```

### API

| Méthode | Route | Rôle |
|---|---|---|
| `GET` | `/api/queue` | État complet de la file |
| `POST` | `/api/queue` | Ajouter `{ items: [{ url, filename?, uqload? }] }` |
| `DELETE` | `/api/queue/:id` | Supprimer une entrée (refusé si en cours) |
| `POST` | `/api/queue/:id/retry` | Repasser une erreur en attente (retry_count remis à 0) |
| `POST` | `/api/control/pause` | Suspendre la file après l'élément en cours |
| `POST` | `/api/control/play` | Reprendre le traitement de la file |
| `GET` | `/api/logs` | Dernières lignes de sortie yt-dlp (console) |
| `GET` | `/api/events` | Flux SSE (`queue`, `progress`, `log`) |
| `POST` | `/api/open-folder` | Ouvrir le dossier de sortie dans l'explorateur |

## Notes

- Les relances internes de yt-dlp pendant un téléchargement HLS (segments manquants, timeouts réseau) sont normales et ne comptent **pas** comme des échecs : seul un exit code ≠ 0 du process déclenche le retry applicatif.
- Au démarrage, une entrée restée en `downloading` (arrêt brutal) repart automatiquement en `pending`.
