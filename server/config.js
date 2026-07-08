import { mkdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');

export const config = {
  port: 3000,
  outputDir: join(root, 'downloads'),
  ytdlpPath: 'yt-dlp', // doit être dans le PATH
  maxRetries: 10,
  // Referer envoyé quand la case "uqload" est cochée — à ajuster si le domaine change
  uqloadReferer: 'https://uqload.cx/',
};

mkdirSync(config.outputDir, { recursive: true });
