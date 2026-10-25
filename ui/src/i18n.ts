type Dict = Record<string, string>;

const EN: Dict = {
  audio_default: "Default output",
  refresh: "Refresh",
  mute_local: "Mute local output (experimental)",
  mute_off: "Local output unchanged",
  mute_waiting_signal: "Waiting for audio to test mute",
  mute_checking: "Checking capture while output is muted…",
  mute_muted: "Local output muted; capture has signal",
  mute_unsupported: "Mute does not preserve capture on this output. It has been restored.",
  mute_user_changed: "Your manual volume or mute change is respected",
  stereo_pair: "Stereo pair (experimental)",
  mute_unavailable: "Local mute is available on Windows",
  incomplete_pair: "Incomplete pair",
  feedback_failed: "The receiver stopped responding",
  transport_failed: "Audio transport failed",
  receiver_unavailable: "The receiver is no longer available",
  session_cancelled: "Operation cancelled",
  session_recovering: "Reconnecting…",
  connect_timeout: "The receiver did not respond in time",
  operation_timeout: "The operation took too long and was cancelled",
  power_resumed: "Resumed from sleep: searching for receivers…",
  rx_playing: "Playing",
  rx_recovering: "Reconnecting…",
  rx_connecting: "Connecting…",
  rx_disconnected: "Disconnected",
  rx_unavailable: "Unavailable",
  rx_play: "Play PC audio on {name}",
  rx_stop: "Stop {name}",
  rx_volume: "Volume of {name}",
  settings: "Settings",
  operation_busy: "Another operation is in progress, try again in a moment",
  stop_before_pairs_change: "Stop playback before changing how pairs are shown",
  quit: "Quit AirSend",
  searching_receivers: "Searching for AirPlay receivers…",
  stop_before_source_change: "Stop playback before changing the source",
  group_requires_two_receivers: "Select two different HomePods",
  stop_before_group_change: "Stop the group before changing its members",

  scan_searching: "scanning…",
  connecting: "Connecting…",
  stop: "⏸ Stop",
  local_buffer_error: "Could not change the local buffer: {err}",
  local_buffer_stop: "Stop playback before changing the local buffer.",
  diagnostics_ready: "Diagnostics prepared",
  diagnostics_error: "Could not export diagnostics: {err}",
  volume: "Volume",
  latency: "Requested buffer limit",
  latency_current: "Applied value",
  latency_pending: "Awaiting confirmation",
  latency_applying: "Applying…",
  latency_error: "Could not apply latency: {err}",
  capture_interrupted: "Audio capture stopped (device disconnected or driver failed)",
  manual_checking: "checking…",
  manual_ok: "OK: {name}",
  manual_need_ip: "enter an IP",
  cant_find: "can't find {name}: {err}",
  error_prefix: "error: {err}",
};

export function t(key: string, params?: Record<string, string | number>): string {
  let s = EN[key] ?? key;
  if (params) {
    for (const [k, v] of Object.entries(params)) {
      s = s.replaceAll(`{${k}}`, String(v));
    }
  }
  return s;
}

// Fills every node carrying data-i18n / data-i18n-attr.
// data-i18n="key"           -> replaces textContent
// data-i18n-attr="attr:key" -> replaces the attribute (e.g. "placeholder:manual_name_placeholder")
export function applyStaticTranslations(): void {
  document.querySelectorAll<HTMLElement>("[data-i18n]").forEach((el) => {
    const key = el.dataset.i18n;
    if (key) el.textContent = t(key);
  });
  document.querySelectorAll<HTMLElement>("[data-i18n-attr]").forEach((el) => {
    const spec = el.dataset.i18nAttr;
    if (!spec) return;
    for (const pair of spec.split(",")) {
      const [attr, key] = pair.split(":").map((s: string) => s.trim());
      if (attr && key) el.setAttribute(attr, t(key));
    }
  });
}
