// Proxy HLS local : certains hébergeurs empoisonnent leurs playlists avec des segments
// vidéo déguisés en images (faux en-tête PNG/JPEG de quelques octets devant le flux
// MPEG-TS, que leur lecteur web retire en JavaScript). Ce module récupère playlists et
// segments avec les bons en-têtes, réécrit les URLs vers le proxy local et retire les
// faux en-têtes, pour livrer à ffmpeg un flux TS propre.

import { request as httpsRequest } from 'node:https';
import { request as httpRequest } from 'node:http';

const TS_SYNC = 0x47; // octet de synchronisation MPEG-TS, répété tous les 188 octets

// Constructeurs d'URL vers les routes proxy locales, partagés par le serveur et le worker.
// Le chemin se termine par .m3u8 / .ts car ffmpeg refuse d'ouvrir un segment HLS dont
// l'URL n'a pas une extension connue (contrôle « allowed_segment_extensions »).
export function makeProxyUrls(base) {
  const build = (path, url, ref) =>
    `${base}/hls/${path}?url=${encodeURIComponent(url)}${ref ? `&ref=${encodeURIComponent(ref)}` : ''}`;
  return {
    playlistProxyUrl: (url, ref) => build('list.m3u8', url, ref),
    segmentProxyUrl: (url, ref) => build('seg.ts', url, ref),
  };
}

// Télécharge une URL en mémoire (certificats invalides tolérés, redirections suivies)
export function fetchBuffer(url, headers = {}, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    const target = new URL(url);
    const requestFn = target.protocol === 'http:' ? httpRequest : httpsRequest;
    const req = requestFn(
      target,
      { headers, rejectUnauthorized: false, timeout: 30_000 },
      (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location && maxRedirects > 0) {
          res.resume();
          resolve(fetchBuffer(new URL(res.headers.location, target).href, headers, maxRedirects - 1));
          return;
        }
        if (res.statusCode !== 200) {
          res.resume();
          reject(new Error(`HTTP ${res.statusCode} sur ${target.host}`));
          return;
        }
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () => resolve(Buffer.concat(chunks)));
        res.on('error', reject);
      }
    );
    req.on('timeout', () => req.destroy(new Error(`délai dépassé sur ${target.host}`)));
    req.on('error', reject);
    req.end();
  });
}

// Retire un éventuel faux en-tête devant le flux TS : on cherche le premier octet 0x47
// suivi de deux autres à 188 octets d'intervalle (période des paquets MPEG-TS)
export function stripFakeHeader(buf) {
  const limit = Math.min(buf.length - 376, 65_536);
  for (let i = 0; i < limit; i++) {
    if (buf[i] === TS_SYNC && buf[i + 188] === TS_SYNC && buf[i + 376] === TS_SYNC) {
      return i === 0 ? buf : buf.subarray(i);
    }
  }
  return buf; // pas un flux TS (fMP4, clé de chiffrement…) : inchangé
}

// Réécrit toutes les URLs d'une playlist HLS (segments, variantes, URI d'attributs)
// vers le proxy local. `variantUrl` et `segmentUrl` transforment une URL absolue.
export function rewritePlaylist(text, baseUrl, { variantUrl, segmentUrl }) {
  const toAbsolute = (uri) => new URL(uri, baseUrl).href;
  const out = [];
  let nextIsVariant = false;
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) {
      out.push(raw);
      continue;
    }
    if (line.startsWith('#')) {
      if (line.startsWith('#EXT-X-STREAM-INF')) nextIsVariant = true;
      // #EXT-X-KEY, #EXT-X-MAP, #EXT-X-MEDIA… portent leur URL dans un attribut URI="…"
      out.push(line.replace(/URI="([^"]+)"/g, (_m, uri) => `URI="${segmentUrl(toAbsolute(uri))}"`));
      continue;
    }
    out.push(nextIsVariant ? variantUrl(toAbsolute(line)) : segmentUrl(toAbsolute(line)));
    nextIsVariant = false;
  }
  return out.join('\n');
}
