type Dict = Record<string, string>;

const EN: Dict = {
  audio_source: "Windows audio source",
  audio_default: "Default output",
  refresh: "Refresh",
  audio_source_hint: "Captures apps already playing on this output; it does not redirect them. Stop playback before changing it.",
  mute_local: "Mute local output (experimental)",
  mute_hint: "Waits for audio and checks that capture keeps its signal. Restores the output if capture stops, and when playback stops or fails.",
  mute_off: "Local output unchanged",
  mute_waiting_signal: "Waiting for audio to test mute",
  mute_checking: "Checking capture while output is muted…",
  mute_muted: "Local output muted; capture has signal",
  mute_unsupported: "Mute does not preserve capture on this output. It has been restored.",
  mute_user_changed: "Your manual volume or mute change is respected",
  group_first: "First HomePod",
  stereo_pair: "Stereo pair (experimental)",
  group_second: "Second HomePod",
  mute_unavailable: "Local mute is available on Windows",
  group_summary: "Pairs and synchronization (experimental)",
  stereo_pairs: "Show pairs configured in Apple Home",
  group_hint: "Try simultaneous playback on two HomePods. Losing a member stops the whole group. Validation with real devices is pending.",
  group_play: "Test synchronized playback",
  incomplete_pair: "Incomplete pair",
  auto_reconnect: "Reconnect this receiver after failure (up to 3 attempts)",
  check_updates: "Check for updates",
  volume_unreadable: "Receiver reading unavailable; requested setting",
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

  subtitle: "Send your Windows audio to your HomePod",
  scan: "Scan devices",
  scan_searching: "scanning…",
  devices_count_one: "1 device",
  devices_count_other: "{n} devices",
  connect: "Connect",
  connecting: "Connecting…",
  disconnect: "Disconnect",
  play_generic: "▶ Play PC audio",
  play_to: "▶ Send PC audio to {name}",
  stop: "⏸ Stop",
  player_starting: "starting…",
  player_playing: "playing",
  multi_device: "Play on multiple devices",
  multi_device_hint: "Experimental: connecting another receiver during playback adds it to the current audio. Receivers may not be perfectly synchronized.",
  switch_title: "Switch audio device",
  switch_message: "Audio is playing on {from}. With multiple devices off, connecting {to} will switch playback and disconnect {from}.",
  switch_confirm: "Switch to {name}",
  multi_off_title: "Turn off multiple devices",
  multi_off_message: "Playback will stop on the other devices and continue on {name}.",
  multi_off_confirm: "Keep only {name}",
  cancel: "Cancel",
  local_buffer: "Reduced local buffer (experimental)",
  local_buffer_hint: "May reduce delay, but may also cause dropouts. Stop playback before changing it. The HomePod can add its own delay.",
  local_buffer_error: "Could not change the local buffer: {err}",
  local_buffer_stop: "Stop playback before changing the local buffer.",
  diagnostics_summary: "Audio diagnostics",
  diagnostics_hint: "Download recent session metrics to compare delay and dropouts. Device names, IP addresses and credentials are excluded. Local measurements do not represent audible HomePod delay.",
  diagnostics_export: "Download diagnostics",
  diagnostics_ready: "Diagnostics prepared",
  diagnostics_error: "Could not export diagnostics: {err}",
  volume: "Volume",
  latency: "Requested buffer limit",
  latency_lower: "Less delay",
  latency_safer: "More stable",
  latency_hint:
    "Confirm to apply a change. If audio is playing, the connection briefly restarts. 0 ms requests no extra buffer, but is experimental: it may fail or stutter. The receiver may add more delay.",
  latency_confirm: "Confirm",
  latency_current: "Applied value",
  latency_pending: "Awaiting confirmation",
  latency_applying: "Applying…",
  latency_cooldown: "Available in {seconds} s",
  latency_error: "Could not apply latency: {err}",
  capture_interrupted: "Audio capture stopped (device disconnected or driver failed)",
  manual_summary: "Can't see your HomePod? Add its IP and port manually",
  manual_hint:
    "Enter an IP or IP:port, for example 192.168.1.50:7453. Use [address]:port for IPv6.",
  manual_endpoint_placeholder: "192.168.1.50[:7000]",
  manual_name_placeholder: "Name (optional)",
  manual_add: "Add",
  manual_checking: "checking…",
  manual_ok: "OK: {name}",
  manual_need_ip: "enter an IP",
  reconnecting: "reconnecting to {name}…",
  cant_find: "can't find {name}: {err}",
  error_prefix: "error: {err}",
  vol_error_prefix: "vol err: {err}",
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
