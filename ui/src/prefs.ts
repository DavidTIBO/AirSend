import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import type { Device } from "./devices";
import { t } from "./i18n";
import type { PrefsAction, PrefsResult, PrefsState } from "./prefs-protocol";

// Preferences window. Settings that affect the running session are sent to
// the compact window (which owns it); the others are saved directly.

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const send = (action: PrefsAction) => void emit("prefs://action", action);

let state: PrefsState | null = null;

// ── Navigation ──────────────────────────────────────────────────────────────

const navButtons = Array.from(document.querySelectorAll<HTMLButtonElement>(".sidebar button"));
const pages = Array.from(document.querySelectorAll<HTMLElement>(".pages section"));
for (const button of navButtons) {
  button.addEventListener("click", () => {
    for (const other of navButtons) other.classList.toggle("active", other === button);
    for (const page of pages) page.hidden = page.dataset.page !== button.dataset.page;
  });
}

// ── AirPlay streaming ───────────────────────────────────────────────────────

const latency = $<HTMLInputElement>("latency");
const latencyOut = $<HTMLOutputElement>("latency-out");
const latencyConfirm = $<HTMLButtonElement>("latency-confirm");
const latencyStatus = $<HTMLSpanElement>("latency-status");
let latencyTouched = false;
let latencyError = "";

function refreshLatency() {
  if (!state) return;
  if (!latencyTouched) latency.value = String(state.latencyMs);
  latencyOut.textContent = `${latency.value} ms`;
  const pending = Number(latency.value) !== state.latencyMs;
  latencyConfirm.disabled = !pending || state.latencyApplying || state.busy;
  latencyStatus.textContent = state.latencyApplying
    ? t("latency_applying")
    : latencyError || t(pending ? "latency_pending" : "latency_current");
}

latency.addEventListener("input", () => {
  latencyTouched = true;
  latencyError = "";
  refreshLatency();
});
latencyConfirm.addEventListener("click", () => {
  latencyConfirm.disabled = true;
  latencyStatus.textContent = t("latency_applying");
  send({ type: "latency", ms: Number(latency.value) });
});

const localBuffer = $<HTMLInputElement>("local-buffer");
const localBufferStatus = $<HTMLSpanElement>("local-buffer-status");
let savedLocalBuffer = false;
void invoke<boolean>("get_experimental_local_buffer").then((enabled) => {
  savedLocalBuffer = enabled;
  localBuffer.checked = enabled;
}).catch(() => {});
localBuffer.addEventListener("change", async () => {
  const enabled = localBuffer.checked;
  localBuffer.disabled = true;
  try {
    await invoke("save_experimental_local_buffer", { enabled });
    savedLocalBuffer = enabled;
    localBufferStatus.textContent = "";
  } catch (err) {
    localBuffer.checked = savedLocalBuffer;
    localBufferStatus.textContent = String(err) === "stop_before_buffer_change"
      ? t("local_buffer_stop")
      : t("local_buffer_error", { err: String(err) });
  } finally {
    applyState();
  }
});

const reconnect = $<HTMLInputElement>("reconnect");
const reconnectLabel = $<HTMLSpanElement>("reconnect-label");
reconnect.addEventListener("change", () => send({ type: "reconnect", enabled: reconnect.checked }));

// ── Audio capture ───────────────────────────────────────────────────────────

interface AudioOptions { device_id: string | null; mute_local: boolean }
const audioSource = $<HTMLSelectElement>("audio-source");
const muteLocal = $<HTMLInputElement>("mute-local");
const audioStatus = $<HTMLSpanElement>("audio-status");
let savedAudio: AudioOptions = { device_id: null, mute_local: false };
let windowsAudioAvailable = false;

async function refreshAudioOutputs() {
  const devices = await invoke<{ id: string; name: string; is_default: boolean }[]>("list_audio_outputs");
  windowsAudioAvailable = devices.length > 0;
  audioSource.replaceChildren(new Option(t("audio_default"), ""));
  for (const device of devices) {
    audioSource.add(new Option(device.name + (device.is_default ? ` (${t("audio_default")})` : ""), device.id));
  }
  if (savedAudio.device_id && !devices.some((d) => d.id === savedAudio.device_id)) {
    audioSource.add(new Option(t("receiver_unavailable"), savedAudio.device_id));
  }
  audioSource.value = savedAudio.device_id ?? "";
  applyState();
}

async function saveAudioOptions() {
  const options = { device_id: audioSource.value || null, mute_local: muteLocal.checked };
  try {
    await invoke("save_audio_options", { options });
    savedAudio = options;
  } catch (error) {
    audioSource.value = savedAudio.device_id ?? "";
    muteLocal.checked = savedAudio.mute_local;
    audioStatus.textContent = t(String(error));
  }
}

void invoke<AudioOptions>("get_audio_options").then((options) => {
  savedAudio = options;
  muteLocal.checked = options.mute_local;
  return refreshAudioOutputs();
}).catch((e) => { audioStatus.textContent = String(e); });
audioSource.addEventListener("change", () => void saveAudioOptions());
muteLocal.addEventListener("change", () => void saveAudioOptions());
$<HTMLButtonElement>("audio-refresh").addEventListener("click", () => {
  void refreshAudioOutputs().catch((e) => { audioStatus.textContent = String(e); });
});

// Mute status of the running session.
window.setInterval(async () => {
  try {
    const status = await invoke<{ mute_status: string }>("get_session_status");
    audioStatus.textContent = t(`mute_${status.mute_status}`);
  } catch {
    // Retried on the next tick.
  }
}, 3000);

// ── AirPlay receivers ───────────────────────────────────────────────────────

const multi = $<HTMLInputElement>("multi-device");
const multiStatus = $<HTMLSpanElement>("multi-status");
multi.addEventListener("change", () => {
  multiStatus.textContent = "";
  send({ type: "multi", enabled: multi.checked });
});

const stereoPairs = $<HTMLInputElement>("stereo-pairs");
const groupFirst = $<HTMLSelectElement>("group-first");
const groupSecond = $<HTMLSelectElement>("group-second");
const groupPlay = $<HTMLButtonElement>("group-play");
const groupStatus = $<HTMLSpanElement>("group-status");
stereoPairs.addEventListener("change", () => {
  groupStatus.textContent = "";
  send({ type: "stereoPairs", enabled: stereoPairs.checked });
});
groupPlay.addEventListener("click", () => {
  groupStatus.textContent = t("connecting");
  send({ type: "groupPlay", first: groupFirst.value, second: groupSecond.value });
});

function refreshGroupChoices(candidates: PrefsState["groupCandidates"]) {
  const first = groupFirst.value;
  const second = groupSecond.value;
  const key = JSON.stringify(candidates);
  if (groupFirst.dataset.key === key) return;
  groupFirst.dataset.key = key;
  groupFirst.replaceChildren();
  groupSecond.replaceChildren();
  for (const { id, name } of candidates) {
    groupFirst.add(new Option(name, id));
    groupSecond.add(new Option(name, id));
  }
  if (candidates.some((c) => c.id === first)) groupFirst.value = first;
  if (second !== groupFirst.value && candidates.some((c) => c.id === second)) groupSecond.value = second;
  else if (candidates.length > 1) groupSecond.selectedIndex = candidates.findIndex((c) => c.id !== groupFirst.value);
}

const manualIp = $<HTMLInputElement>("manual-ip");
const manualName = $<HTMLInputElement>("manual-name");
const manualAdd = $<HTMLButtonElement>("manual-add");
const manualStatus = $<HTMLSpanElement>("manual-status");
async function addManual() {
  const ip = manualIp.value.trim();
  if (!ip) {
    manualStatus.textContent = t("manual_need_ip");
    return;
  }
  manualAdd.disabled = true;
  manualStatus.textContent = t("manual_checking");
  try {
    const device = await invoke<Device>("add_manual_device", { ip, port: null, name: manualName.value.trim() || null });
    manualStatus.textContent = t("manual_ok", { name: device.name });
    manualIp.value = "";
    manualName.value = "";
  } catch (err) {
    manualStatus.textContent = String(err);
  } finally {
    manualAdd.disabled = false;
  }
}
manualAdd.addEventListener("click", () => void addManual());
manualIp.addEventListener("keydown", (e) => { if (e.key === "Enter") void addManual(); });

const rescanStatus = $<HTMLSpanElement>("rescan-status");
$<HTMLButtonElement>("rescan").addEventListener("click", () => {
  rescanStatus.textContent = t("scan_searching");
  send({ type: "rescan" });
});

// ── Diagnostics ─────────────────────────────────────────────────────────────

const diagnosticsExport = $<HTMLButtonElement>("diagnostics-export");
const diagnosticsStatus = $<HTMLSpanElement>("diagnostics-status");
diagnosticsExport.addEventListener("click", async () => {
  diagnosticsExport.disabled = true;
  try {
    const report = await invoke<string>("export_audio_diagnostics");
    const url = URL.createObjectURL(new Blob([report], { type: "application/json" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = `AirSend-diagnostics-${new Date().toISOString().replaceAll(":", "-")}.json`;
    document.body.append(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
    diagnosticsStatus.textContent = t("diagnostics_ready");
  } catch (err) {
    diagnosticsStatus.textContent = t("diagnostics_error", { err: String(err) });
  } finally {
    diagnosticsExport.disabled = false;
  }
});

// ── State from the compact window ───────────────────────────────────────────

function applyState() {
  if (!state) return;
  const locked = state.playing || state.busy;
  refreshLatency();
  localBuffer.disabled = locked;
  reconnect.checked = state.reconnect;
  reconnect.disabled = !state.receiverName;
  reconnectLabel.textContent = state.receiverName
    ? `Reconnect ${state.receiverName} after a failure (up to 3 attempts)`
    : "Reconnect the current receiver after a failure (up to 3 attempts)";
  audioSource.disabled = locked;
  muteLocal.disabled = !windowsAudioAvailable || locked;
  multi.checked = state.multiDevice;
  multi.disabled = state.synchronized || state.busy;
  stereoPairs.checked = state.stereoPairs;
  stereoPairs.disabled = locked;
  refreshGroupChoices(state.groupCandidates);
  groupFirst.disabled = groupSecond.disabled = groupPlay.disabled = locked;
}

void listen<PrefsState>("ui://state", (event) => {
  state = event.payload;
  applyState();
});

void listen<PrefsResult>("prefs://result", (event) => {
  const { action, ok, message } = event.payload;
  const error = ok ? "" : message ?? "error";
  switch (action) {
    case "latency":
      latencyError = error;
      if (ok) latencyTouched = false;
      refreshLatency();
      break;
    case "multi":
      multiStatus.textContent = error;
      break;
    case "stereoPairs":
    case "groupPlay":
      groupStatus.textContent = error;
      break;
    case "rescan":
      rescanStatus.textContent = error;
      break;
  }
});

void emit("prefs://hello");
