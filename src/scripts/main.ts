import { inTauri } from './api';
import { state } from './state';
import { initNav } from './nav';
import { initDownloads } from './downloads';
import { initLogs } from './logs';
import { initSettings } from './settings';
import { initUpdate } from './update';
import { initResize } from './resize';

async function boot(): Promise<void> {
  state.tauri = inTauri();
  document.getElementById('browser-banner')!.hidden = state.tauri;
  initResize();
  initNav();
  initUpdate();
  await Promise.allSettled([initDownloads(), initLogs(), initSettings()]);
}

void boot();
