// Shared TS types. Mirror the Rust structs by hand; keep in sync.
// Pairs:
//   Config / NotificationPrefs  <-> src-tauri/src/config/mod.rs
//   StackStatus / StackStatusPayload <-> src-tauri/src/lifecycle/mod.rs

export interface NotificationPrefs {
  task_complete: boolean;
  approval_needed: boolean;
  task_failed: boolean;
  stack_degraded: boolean;
}

export interface Config {
  hotkey: string;
  use_copilot_key: boolean;
  default_context_folder: string | null;
  ollama_base_url: string;
  setup_completed: boolean;
  theme: string;
  notifications: NotificationPrefs;
  remember_overlay_position: boolean;
}

// Serde `rename_all = "snake_case"` on the Rust enum.
export type StackStatus =
  | 'starting'
  | 'ok'
  | 'ollama_down'
  | 'goosed_down'
  | 'no_model'
  | 'provider_unreachable'
  | 'conflict_goose_desktop';

export interface StackStatusPayload {
  status: StackStatus;
  detail: string | null;
}
