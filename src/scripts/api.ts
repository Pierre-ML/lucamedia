import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export type Phase = 'resolving' | 'downloading' | 'verifying' | 'repairing' | 'assembling';
export type DownloadStatus = 'pending' | 'running' | 'done' | 'warning' | 'error' | 'cancelled';
export type TrackKind = 'video' | 'audio';
export type Gap = { stream: TrackKind; at: number; missing: number; segmentIndex: number | null };
export type BadSegment = { track: TrackKind; index: number; reason: string };
export type GapReport = {
  ok: boolean;
  expectedDuration: number | null;
  actualDuration: number;
  gaps: Gap[];
  badSegments: BadSegment[];
  avDrift: number | null;
  summary: string;
};
export type Download = {
  id: number;
  url: string;
  filename: string;
  referer: string | null;
  status: DownloadStatus;
  phase: Phase | null;
  progressPercent: number;
  downloadedBytes: number;
  totalBytes: number | null;
  speedBps: number | null;
  etaSeconds: number | null;
  segmentsDone: number | null;
  segmentsTotal: number | null;
  retryCount: number;
  error: string | null;
  outputPath: string | null;
  resolvedVia: 'direct' | 'ytdlp' | 'browser' | null;
  mediaKind: 'hls' | 'dash' | 'file' | 'ytdlp' | null;
  quality: string | null;
  report: GapReport | null;
  createdAt: number;
  finishedAt: number | null;
};
export type QueueState = { paused: boolean; items: Download[] };
export type NewItem = { url: string; filename?: string; referer?: string };
export type Settings = {
  destDir: string;
  parallelSegments: number;
  maxRetries: number;
  browser: 'auto' | 'chrome' | 'edge';
  showBrowser: boolean;
  verifyAfterDownload: boolean;
  autoRepair: boolean;
};
export type ToolStatus = { available: boolean; path: string | null; version: string | null };
export type ToolsStatus = {
  ffmpeg: ToolStatus;
  ffprobe: ToolStatus;
  ytdlp: ToolStatus;
  browser: { available: boolean; name: string | null; path: string | null };
};
export type ProgressEvent = {
  id: number;
  phase: Phase | null;
  progressPercent: number;
  downloadedBytes: number;
  totalBytes: number | null;
  speedBps: number | null;
  etaSeconds: number | null;
  segmentsDone: number | null;
  segmentsTotal: number | null;
};
export type LogLine = { ts: number; level: 'info' | 'warn' | 'error'; downloadId: number | null; message: string };

export const inTauri = (): boolean =>
  typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;

/** Message d'erreur lisible depuis une valeur rejetée par invoke. */
export function errMsg(e: unknown): string {
  if (typeof e === 'string') return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

export const getSettings = () => invoke<Settings>('get_settings');
export const saveSettings = (settings: Settings) => invoke<Settings>('save_settings', { settings });
export const queueState = () => invoke<QueueState>('queue_state');
export const queueAdd = (items: NewItem[]) => invoke<QueueState>('queue_add', { items });
export const queueRemove = (id: number) => invoke<QueueState>('queue_remove', { id });
export const queueRetry = (id: number) => invoke<QueueState>('queue_retry', { id });
export const queueCancel = (id: number) => invoke<QueueState>('queue_cancel', { id });
export const queueClearFinished = () => invoke<QueueState>('queue_clear_finished');
export const queueSetPaused = (paused: boolean) => invoke<QueueState>('queue_set_paused', { paused });
export const verifyAgain = (id: number) => invoke<QueueState>('verify_again', { id });
export const revealDownload = (id: number) => invoke<null>('reveal_download', { id });
export const openDestDir = () => invoke<null>('open_dest_dir');
export const toolsStatus = () => invoke<ToolsStatus>('tools_status');
export const logsRecent = () => invoke<LogLine[]>('logs_recent');

export const onQueueUpdate = (cb: (q: QueueState) => void): Promise<UnlistenFn> =>
  listen<QueueState>('queue-update', (e) => cb(e.payload));
export const onDownloadProgress = (cb: (p: ProgressEvent) => void): Promise<UnlistenFn> =>
  listen<ProgressEvent>('download-progress', (e) => cb(e.payload));
export const onLog = (cb: (l: LogLine) => void): Promise<UnlistenFn> =>
  listen<LogLine>('log', (e) => cb(e.payload));

export type UpdateInfo = {
  current: string;
  latest: string | null;
  available: boolean;
  notes: string | null;
  url: string | null;
};
export type UpdateProgress = { downloaded: number; total: number | null };

export const appCheckUpdate = () => invoke<UpdateInfo>('app_check_update');
export const appInstallUpdate = () => invoke<null>('app_install_update');
export const appUninstall = () => invoke<null>('app_uninstall');
export const onUpdateProgress = (cb: (p: UpdateProgress) => void): Promise<UnlistenFn> =>
  listen<UpdateProgress>('app-update-progress', (e) => cb(e.payload));
