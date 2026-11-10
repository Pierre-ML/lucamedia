import type { QueueState, Settings } from './api';

export const state: {
  tauri: boolean;
  queue: QueueState;
  settings: Settings | null;
} = { tauri: false, queue: { paused: false, items: [] }, settings: null };
