// Usage : node scripts/bump-version.mjs <x.y.z | patch | minor | major>
// Met à jour la version dans package.json, src-tauri/tauri.conf.json,
// package-lock.json (racine + packages[""]), src-tauri/Cargo.toml ([package]) et src-tauri/Cargo.lock.
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const p = (f) => join(root, f);
const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;

const arg = process.argv[2];
if (!arg) {
  console.error("Usage : node scripts/bump-version.mjs <x.y.z | patch | minor | major>");
  process.exit(1);
}

const pkg = JSON.parse(readFileSync(p("package.json"), "utf8"));
const old = pkg.version;
let next = arg;
if (["patch", "minor", "major"].includes(arg)) {
  const [ma, mi, pa] = old.split("-")[0].split(".").map(Number);
  next = arg === "major" ? `${ma + 1}.0.0` : arg === "minor" ? `${ma}.${mi + 1}.0` : `${ma}.${mi}.${pa + 1}`;
}
if (!SEMVER.test(next)) {
  console.error(`Version invalide : "${next}" (attendu x.y.z)`);
  process.exit(1);
}

const jsonVersion = (file) => {
  const s = readFileSync(p(file), "utf8");
  const out = s.replace(/("version"\s*:\s*")[^"]*(")/, `$1${next}$2`);
  if (out === s && !s.includes(`"version": "${next}"`)) throw new Error(`version introuvable dans ${file}`);
  writeFileSync(p(file), out);
};
jsonVersion("package.json");
jsonVersion("src-tauri/tauri.conf.json");

// package-lock.json : "version" racine, puis packages[""].version (les deux doivent concorder)
if (existsSync(p("package-lock.json"))) {
  let lock = readFileSync(p("package-lock.json"), "utf8");
  const reRoot = /^(\s*"version"\s*:\s*")[^"]*(")/m;
  const rePkg = /("packages"\s*:\s*\{\s*""\s*:\s*\{[^{}]*?"version"\s*:\s*")[^"]*(")/;
  if (!reRoot.test(lock) || !rePkg.test(lock)) throw new Error("version introuvable dans package-lock.json");
  lock = lock.replace(reRoot, `$1${next}$2`).replace(rePkg, `$1${next}$2`);
  writeFileSync(p("package-lock.json"), lock);
}

// Cargo.toml : uniquement la section [package]
let cargo = readFileSync(p("src-tauri/Cargo.toml"), "utf8");
const pkgName = cargo.match(/^\[package\][^[]*?^name\s*=\s*"([^"]+)"/m)?.[1];
cargo = cargo.replace(/(\[package\][^[]*?^version\s*=\s*")[^"]*(")/m, `$1${next}$2`);
writeFileSync(p("src-tauri/Cargo.toml"), cargo);

// Cargo.lock : entrée du package
const lockPath = p("src-tauri/Cargo.lock");
if (existsSync(lockPath) && pkgName) {
  const lock = readFileSync(lockPath, "utf8");
  const re = new RegExp(String.raw`(\[\[package\]\]\r?\nname = "${pkgName}"\r?\nversion = ")[^"]*(")`);
  writeFileSync(lockPath, lock.replace(re, `$1${next}$2`));
}

console.log(`${old} -> ${next}`);
