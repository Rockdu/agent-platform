// task22 / AC-8.1 / AC-8.4 — wire types + invoke wrappers for the
// host notification surface. Mirrors src-tauri/src/notification.rs.

import { invoke } from "@tauri-apps/api/core";

export type PermissionStateDto = "unknown" | "granted" | "denied";

export interface TrayEntryDto {
  id: string;
  pluginId: string;
  terminalId: string;
  kindName: string;
  severity: string;
  summary: string;
  firedAtUnixMs: number;
  suppressedCount: number;
  /**
   * task23 / AC-3.5: true when this event originated from the
   * orchestrator's claude PTY. The tray UI renders a 🤖 / "claude"
   * badge on these rows so orchestrator events are visually distinct
   * from regular tabs.
   */
  isOrchestrator: boolean;
}

export async function notificationGetPermissionState(): Promise<PermissionStateDto> {
  return await invoke<PermissionStateDto>("notification_get_permission_state");
}

export async function notificationListRecentTrayEntries(): Promise<TrayEntryDto[]> {
  return await invoke<TrayEntryDto[]>("notification_list_recent_tray_entries");
}

export async function notificationClearTrayEntries(): Promise<void> {
  await invoke<void>("notification_clear_tray_entries");
}

/**
 * Daily-digest paper card. Stored in a ring buffer separate from the
 * terminal-event-shaped TrayEntryDto so the tray UI can render a
 * dedicated papers section without conflating the two data shapes.
 */
export interface PapersTrayEntryDto {
  arxivId: string;
  title: string;
  abstractSnippet: string;
  absUrl: string;
  fetchedAt: string;
}

export async function notificationListRecentPapersTrayEntries(): Promise<PapersTrayEntryDto[]> {
  return await invoke<PapersTrayEntryDto[]>("notification_list_recent_papers_tray_entries");
}

export async function notificationClearPapersTrayEntries(): Promise<void> {
  await invoke<void>("notification_clear_papers_tray_entries");
}
