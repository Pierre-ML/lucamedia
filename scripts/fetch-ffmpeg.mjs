#!/usr/bin/env node
// Récupère les trois sidecars d'Ursa et les installe en src-tauri/binaries/<nom>-<triple>[.exe] :
//   - ffmpeg et ffprobe (même archive statique BtbN sous Windows/Linux) ;
//   - yt-dlp (exécutable autonome publié par yt-dlp sur GitHub).
// Node >= 20, aucune dépendance npm.
//
// Usage : node scripts/fetch-ffmpeg.mjs [--target <triple>] [--from-path] [--force]
//   --from-path : copie ffmpeg, ffprobe et yt-dlp trouvés dans le PATH au lieu de les télécharger.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const BTBN = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest";
const RIEDL = "https://ffmpeg.martin-riedl.de/redirect/latest/macos";
const YTDLP = "https://github.com/yt-dlp/yt-dlp/releases/latest/download";

// Pour chaque cible : où trouver ffmpeg/ffprobe (archive + nom du binaire dedans) et yt-dlp (fichier direct).
const SOURCES = {
  "x86_64-pc-windows-msvc": {
    ffmpeg: { url: `${BTBN}/ffmpeg-master-latest-win64-gpl.zip`, archive: "zip", bin: "ffmpeg.exe" },
    ffprobe: { url: `${BTBN}/ffmpeg-master-latest-win64-gpl.zip`, archive: "zip", bin: "ffprobe.exe" },
    ytdlp: { url: `${YTDLP}/yt-dlp.exe` },
  },
  "x86_64-unknown-linux-gnu": {
    ffmpeg: { url: `${BTBN}/ffmpeg-master-latest-linux64-gpl.tar.xz`, archive: "tar.xz", bin: "ffmpeg" },
    ffprobe: { url: `${BTBN}/ffmpeg-master-latest-linux64-gpl.tar.xz`, archive: "tar.xz", bin: "ffprobe" },
    ytdlp: { url: `${YTDLP}/yt-dlp_linux` },
  },
  "aarch64-apple-darwin": {
    ffmpeg: { url: `${RIEDL}/arm64/release/ffmpeg.zip`, archive: "zip", bin: "ffmpeg" },
    ffprobe: { url: `${RIEDL}/arm64/release/ffprobe.zip`, archive: "zip", bin: "ffprobe" },
    ytdlp: { url: `${YTDLP}/yt-dlp_macos` },
  },
  "x86_64-apple-darwin": {
    ffmpeg: { url: `${RIEDL}/amd64/release/ffmpeg.zip`, archive: "zip", bin: "ffmpeg" },
    ffprobe: { url: `${RIEDL}/amd64/release/ffprobe.zip`, archive: "zip", bin: "ffprobe" },
    ytdlp: { url: `${YTDLP}/yt-dlp_macos` },
  },
};

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(root, "src-tauri", "binaries");

function parseArgs(argv) {
  const opts = { target: null, fromPath: false, force: false };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--target") opts.target = argv[++i];
    else if (a.startsWith("--target=")) opts.target = a.slice(9);
    else if (a === "--from-path") opts.fromPath = true;
    else if (a === "--force") opts.force = true;
    else die(`Argument inconnu : ${a}`);
  }
  return opts;
}

function die(msg) {
  console.error(`Erreur : ${msg}`);
  process.exit(1);
}

function hostTriple() {
  try {
    const out = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
    const m = out.match(/^host:\s*(\S+)/m);
    if (m) return m[1];
  } catch {}
  die("impossible de déterminer la cible (rustc introuvable). Utilise --target <triple>.");
}

function findInPath(name) {
  const cmd = process.platform === "win32" ? "where" : "which";
  try {
    const out = execFileSync(cmd, [name], { encoding: "utf8" });
    const first = out.split(/\r?\n/).find(Boolean);
    if (first) return first.trim();
  } catch {}
  return null;
}

function findRecursive(dir, fileName) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, entry.name);
    if (entry.isDirectory()) {
      const r = findRecursive(p, fileName);
      if (r) return r;
    } else if (entry.name.toLowerCase() === fileName.toLowerCase()) {
      return p;
    }
  }
  return null;
}

async function download(url, dest) {
  console.log(`Téléchargement : ${url}`);
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) throw new Error(`HTTP ${res.status} ${res.statusText}`);
  const total = Number(res.headers.get("content-length")) || 0;
  const chunks = [];
  let got = 0;
  let lastPct = -10;
  for await (const chunk of res.body) {
    chunks.push(chunk);
    got += chunk.length;
    if (total) {
      const pct = Math.floor((got / total) * 100);
      if (pct >= lastPct + 10) {
        lastPct = pct;
        console.log(`  ${pct}% (${(got / 1048576).toFixed(0)} Mo)`);
      }
    }
  }
  writeFileSync(dest, Buffer.concat(chunks));
  console.log(`  ${(got / 1048576).toFixed(1)} Mo reçus.`);
}

// Extrait une archive. Windows : tar.exe système (bsdtar, gère .zip) plutôt que le tar GNU de Git Bash.
// Chemins relatifs + cwd pour éviter que "C:" soit lu comme un hôte distant.
function extract(tmp, archiveName, extractDirName) {
  mkdirSync(join(tmp, extractDirName), { recursive: true });
  const sysTar = process.platform === "win32" ? join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe") : "tar";
  execFileSync(existsSync(sysTar) ? sysTar : "tar", ["-xf", archiveName, "-C", extractDirName], { stdio: "inherit", cwd: tmp });
}

function report(name, dest) {
  console.log(`${name} installé : ${dest} (${(statSync(dest).size / 1048576).toFixed(1)} Mo)`);
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const target = opts.target || hostTriple();
  const isWin = target.includes("windows");
  const ext = isWin ? ".exe" : "";
  const destOf = (name) => join(outDir, `${name}-${target}${ext}`);
  const names = ["ffmpeg", "ffprobe", "yt-dlp"];
  const key = { ffmpeg: "ffmpeg", ffprobe: "ffprobe", "yt-dlp": "ytdlp" };

  const todo = names.filter((n) => {
    if (existsSync(destOf(n)) && !opts.force) {
      console.log(`${n} déjà présent : ${destOf(n)} (utilise --force pour le remplacer).`);
      return false;
    }
    return true;
  });
  if (!todo.length) return;
  mkdirSync(outDir, { recursive: true });

  if (opts.fromPath) {
    for (const n of todo) {
      const found = findInPath(n);
      if (!found) die(`${n} est introuvable dans le PATH.`);
      console.log(`Copie de ${found}`);
      copyFileSync(found, destOf(n));
    }
  } else {
    const srcs = SOURCES[target];
    if (!srcs) die(`cible non prise en charge : ${target}. Cibles : ${Object.keys(SOURCES).join(", ")}.`);
    const tmp = mkdtempSync(join(tmpdir(), "ursa-fetch-"));
    try {
      // Une archive n'est téléchargée et extraite qu'une fois, même si elle fournit ffmpeg ET ffprobe.
      const extracted = new Map(); // url -> dossier d'extraction
      let n = 0;
      for (const name of todo) {
        const src = srcs[key[name]];
        if (name === "yt-dlp") {
          await download(src.url, destOf(name));
          continue;
        }
        let dir = extracted.get(src.url);
        if (!dir) {
          const id = `a${n++}`;
          const archiveName = `${id}.${src.archive}`;
          await download(src.url, join(tmp, archiveName));
          console.log("Extraction…");
          extract(tmp, archiveName, `${id}-x`);
          dir = join(tmp, `${id}-x`);
          extracted.set(src.url, dir);
        }
        const bin = findRecursive(dir, src.bin);
        if (!bin) die(`${src.bin} introuvable dans l'archive.`);
        copyFileSync(bin, destOf(name));
      }
    } finally {
      rmSync(tmp, { recursive: true, force: true });
    }
  }

  for (const n of todo) {
    if (!isWin) chmodSync(destOf(n), 0o755);
    report(n, destOf(n));
  }
}

main().catch((e) => die(e.message || String(e)));
