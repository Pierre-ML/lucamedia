import type { QueueState } from './api';

export const state: {
  tauri: boolean;
  queue: QueueState;
} = { tauri: false, queue: { paused: false, items: [] } };
