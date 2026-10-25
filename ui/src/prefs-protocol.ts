// Messages between the compact window (main.ts, which owns the AirPlay
// session) and the Preferences window (prefs.ts).

/// Published by the main window on `ui://state`.
export interface PrefsState {
  playing: boolean;
  busy: boolean;
  /// Primary receiver (the one the per-receiver settings apply to).
  receiverName: string | null;
  multiDevice: boolean;
  activeCount: number;
  latencyMs: number;
  latencyApplying: boolean;
  reconnect: boolean;
  stereoPairs: boolean;
  synchronized: boolean;
  groupCandidates: { id: string; name: string }[];
}

/// Sent by the Preferences window on `prefs://action`.
export type PrefsAction =
  | { type: "multi"; enabled: boolean }
  | { type: "latency"; ms: number }
  | { type: "reconnect"; enabled: boolean }
  | { type: "stereoPairs"; enabled: boolean }
  | { type: "groupPlay"; first: string; second: string }
  | { type: "rescan" };

/// Answer to an action, on `prefs://result`.
export interface PrefsResult {
  action: PrefsAction["type"];
  ok: boolean;
  message?: string;
}
