import { invoke } from "@tauri-apps/api/core";
import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { groupDevices, routesFor, type Device, type DeviceKind } from "./devices";
import { applyStaticTranslations, t } from "./i18n";
import type { PrefsAction, PrefsResult, PrefsState } from "./prefs-protocol";

// Compact window: owns the AirPlay session (discovery, playback, volumes) and
// applies the session-related settings requested by the Preferences window.

const KIND_LABEL: Record<DeviceKind, string> = {
  homepod: "HomePod",
  appletv: "Apple TV",
  airportexpress: "AirPort Express",
  otherairplay: "AirPlay",
};

interface ConnectionInfo { ip: string; port: number; name: string }
interface ReceiverPreferences { volume: number; latency_ms: number; reconnect: boolean }
interface PersistedDevice { ip: string; port: number; name: string }

const devicesList = document.getElementById("devices") as HTMLUListElement;
const emptyEl = document.getElementById("empty") as HTMLParagraphElement;
const toastEl = document.getElementById("toast") as HTMLDivElement;

// ── State ───────────────────────────────────────────────────────────────────

const known = new Map<string, Device>();
const discovered = new Map<string, Device>();
let unlisten: UnlistenFn | null = null;
let unlistenRemoved: UnlistenFn | null = null;
/// Primary receiver: the one per-receiver settings (latency) apply to.
let connectedId: string | null = null;
let connectingId: string | null = null;
let connectedRoute: Device | null = null;
const activeRoutes = new Map<string, Device>();
let actionBusy = false;
let stopping = false;
let latencyApplying = false;
let activeGeneration = 0;
let intentSequence = 0;
let volumeRevision = 0;
let playing = false;
let synchronizedPlaying = false;
let sessionRecovering = false;
/// Volume of the primary receiver (0-100).
let primaryVolume = 20;
let multiDevice = false;
let confirmedLatencyMs = 3000;
let showStereoPairs = false;
// Per-receiver volume (0-100) for the other rows: saved preferences, or the
// receiver itself while it plays.
const deviceVolumes = new Map<string, number>();
const loadingVolumes = new Set<string>();
// Re-rendering replaces the sliders, so it waits until a drag ends.
let draggingVolume = false;
let renderPending = false;

let toastTimer: number | null = null;
function showToast(msg: string, durationMs = 5000) {
  toastEl.textContent = msg;
  toastEl.hidden = false;
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => {
    toastEl.hidden = true;
    toastTimer = null;
  }, durationMs);
}

function ipOf(route: Device | undefined | null): string | undefined {
  return route?.addresses.find((a) => !a.includes(":")) ?? route?.addresses[0];
}

// ── Backend session events ──────────────────────────────────────────────────

function setupAsyncErrorListener() {
  // Asynchronous backend errors (dying pump, failing heartbeat).
  void listen<{ generation: number; code: string }>("airplay://error", (event) => {
    if (event.payload.generation !== activeGeneration) return;
    showToast(t(event.payload.code));
    playing = false;
    activeRoutes.clear();
    render();
  });
  void listen<{ generation: number; code: string }>("airplay://session", (event) => {
    if (event.payload.generation < activeGeneration) return;
    activeGeneration = event.payload.generation;
    sessionRecovering = event.payload.code === "recovering";
    if (event.payload.code === "recovering" || event.payload.code === "streaming") playing = true;
    if (event.payload.code === "disconnected" && !actionBusy) { playing = false; activeRoutes.clear(); }
    render();
  });
  // After sleep the backend already closed the session and mDNS; receivers
  // must be discovered again.
  void listen<"suspend" | "resume">("airplay://power", (event) => {
    resetToDisconnected();
    if (event.payload !== "resume") return;
    showToast(t("power_resumed"));
    void startScan();
    // The network can take a few seconds to come back after waking up.
    window.setTimeout(() => { if (known.size === 0) void startScan(); }, 10_000);
  });
}

// Drops any pending operation: late results are ignored thanks to `intentSequence`.
function resetToDisconnected() {
  ++intentSequence;
  actionBusy = false; stopping = false; latencyApplying = false;
  connectingId = null; connectedId = null; connectedRoute = null;
  playing = false; synchronizedPlaying = false; activeRoutes.clear();
  render();
}

// Last resort: no operation may leave the UI stuck. The backend cuts every
// connection attempt at 30 s; the margin covers the teardown.
const OPERATION_DEADLINE_MS = 45_000;
let busySince: number | null = null;
window.setInterval(() => {
  if (!actionBusy && !latencyApplying) { busySince = null; return; }
  busySince ??= Date.now();
  if (Date.now() - busySince < OPERATION_DEADLINE_MS) return;
  busySince = null;
  // Cancelling in the backend releases any task still waiting on the network.
  void invoke("stop_streaming").catch(() => {});
  void invoke("disconnect_device").catch(() => {});
  resetToDisconnected();
  showToast(t("operation_timeout"));
}, 1000);

// ── Playback ────────────────────────────────────────────────────────────────

async function streamTo(device: Device, command: "start_streaming" | "add_streaming", preferred?: Device | null): Promise<{ route: Device; ip: string }> {
  if (device.member_ids) {
    if (command === "add_streaming") throw new Error(t("stop_before_group_change"));
    if (!device.pair_complete) throw new Error(t("incomplete_pair"));
    await invoke("start_group_streaming", { deviceIds: device.member_ids, stereoPair: true, latencyMs: confirmedLatencyMs });
    activeGeneration = (await invoke<{ generation: number }>("get_session_status")).generation;
    synchronizedPlaying = true;
    return { route: device, ip: device.addresses[0] };
  }
  const preferences = command === "add_streaming" || (playing && connectedId !== device.id)
    ? await invoke<ReceiverPreferences>("get_receiver_preferences", { receiverId: device.id }) : null;
  let lastError: unknown = new Error("device has no IP address");
  for (const route of routesFor(discovered, device, preferred ?? null)) {
    const ip = ipOf(route);
    if (!ip) continue;
    try {
      await invoke(command, {
        deviceId: route.id,
        ip,
        port: route.port,
        name: device.name,
        volume: preferences?.volume ?? primaryVolume / 100,
        latencyMs: command === "add_streaming" ? confirmedLatencyMs : preferences?.latency_ms ?? confirmedLatencyMs,
      });
      activeGeneration = (await invoke<{ generation: number }>("get_session_status")).generation;
      if (command === "start_streaming") synchronizedPlaying = false;
      if (preferences && command === "start_streaming") {
        confirmedLatencyMs = preferences.latency_ms;
        primaryVolume = Math.round(preferences.volume * 100);
      }
      return { route, ip };
    } catch (err) {
      lastError = String(err) === "capture_interrupted" ? t("capture_interrupted") : err;
      if (!String(err).startsWith("stream:")) break;
    }
  }
  throw lastError;
}

async function startPlay() {
  if (actionBusy || latencyApplying) return;
  const dev = connectedId ? known.get(connectedId) : null;
  if (!dev) return;
  const intent = ++intentSequence;
  actionBusy = true;
  render();
  try {
    const { route, ip } = await streamTo(dev, "start_streaming", connectedRoute);
    if (intent !== intentSequence) return;
    connectedRoute = route;
    activeRoutes.clear();
    activeRoutes.set(dev.id, route);
    playing = true;
    void invoke("save_last_device", { ip, port: route.port, name: dev.name });
    void invoke("save_volume", { volume: primaryVolume / 100 });
  } catch (err) {
    showToast(t("error_prefix", { err: String(err) }));
  } finally {
    actionBusy = false;
    render();
  }
}

async function stopPlay() {
  if (stopping) return;
  ++intentSequence;
  stopping = true;
  actionBusy = true;
  render();
  try {
    await invoke("stop_streaming");
    await invoke("disconnect_device");
    playing = false;
    synchronizedPlaying = false;
    activeGeneration = (await invoke<{ generation: number }>("get_session_status")).generation;
    activeRoutes.clear();
  } catch (error) {
    showToast(t("error_prefix", { err: String(error) }));
  } finally {
    stopping = false;
    actionBusy = false;
    render();
  }
}

/// While playing: switch to `device`, or add it with "Play on multiple devices".
async function switchOrAdd(device: Device) {
  if (actionBusy || latencyApplying) return;
  const intent = ++intentSequence;
  actionBusy = true;
  connectingId = device.id;
  render();
  try {
    const { route, ip } = await streamTo(device, multiDevice ? "add_streaming" : "start_streaming");
    if (intent !== intentSequence) return;
    if (!multiDevice) {
      activeRoutes.clear();
      connectedId = device.id;
      connectedRoute = route;
      void invoke("save_last_device", { ip, port: route.port, name: device.name });
    }
    activeRoutes.set(device.id, route);
  } catch (err) {
    showToast(t("error_prefix", { err: String(err) }));
  } finally {
    actionBusy = false;
    connectingId = null;
    render();
  }
}

/// Removes one receiver from a multi-device session.
async function removeReceiver(device: Device) {
  if (actionBusy || latencyApplying) return;
  actionBusy = true;
  render();
  try {
    const route = activeRoutes.get(device.id);
    const ip = ipOf(route);
    if (!ip || !route) throw new Error("device has no IP address");
    await invoke("remove_streaming", { ip, port: route.port });
    activeRoutes.delete(device.id);
    if (connectedId === device.id) {
      const next = activeRoutes.entries().next().value as [string, Device] | undefined;
      if (next) {
        connectedId = next[0];
        connectedRoute = next[1];
        primaryVolume = deviceVolumes.get(next[0]) ?? primaryVolume;
        const nextIp = ipOf(next[1]);
        if (nextIp) void invoke("save_last_device", { ip: nextIp, port: next[1].port, name: known.get(next[0])?.name ?? next[1].name });
      }
    }
  } catch (err) {
    showToast(t("error_prefix", { err: String(err) }));
  } finally {
    actionBusy = false;
    render();
  }
}

function isPlayingOn(d: Device) {
  return playing && activeRoutes.has(d.id);
}

/// ▶ on a row: start on that receiver, switch to it, or add it.
async function playOn(d: Device) {
  if (actionBusy || latencyApplying || stopping) return;
  if (playing) { await switchOrAdd(d); return; }
  connectingId = d.id;
  connectedId = d.id;
  connectedRoute = null;
  render();
  try {
    await loadReceiverPreferences(d.id);
    await startPlay();
  } finally {
    connectingId = null;
    render();
  }
}

/// ■ on a row: remove that receiver, or stop everything if it is the last one.
async function stopOn(d: Device) {
  if (activeRoutes.size > 1 && !synchronizedPlaying) await removeReceiver(d);
  else await stopPlay();
}

// ── Volumes ─────────────────────────────────────────────────────────────────

function rowVolume(d: Device): number {
  if (d.id === connectedId) return primaryVolume;
  const saved = deviceVolumes.get(d.id);
  if (saved !== undefined) return saved;
  if (!loadingVolumes.has(d.id)) {
    loadingVolumes.add(d.id);
    void invoke<ReceiverPreferences>("get_receiver_preferences", { receiverId: d.id })
      .then((prefs) => { deviceVolumes.set(d.id, Math.round(prefs.volume * 100)); render(); })
      .catch(() => {})
      .finally(() => loadingVolumes.delete(d.id));
  }
  return primaryVolume;
}

let rowVolumeTimer: number | undefined;
function setRowVolume(d: Device, pct: number) {
  ++volumeRevision;
  deviceVolumes.set(d.id, pct);
  const volume = pct / 100;
  if (d.id === connectedId) primaryVolume = pct;
  clearTimeout(rowVolumeTimer);
  rowVolumeTimer = window.setTimeout(() => {
    const route = isPlayingOn(d) ? activeRoutes.get(d.id) : undefined;
    const ip = ipOf(route);
    if (d.id === connectedId) void invoke("save_volume", { volume });
    if (route && ip && activeRoutes.size > 1 && !synchronizedPlaying) {
      // Several receivers: only this one changes (the command also saves it).
      void invoke("set_receiver_volume", { ip, port: route.port, volume }).catch((e) => showToast(String(e)));
    } else if (route) {
      void invoke("set_stream_volume", { volume })
        .then(() => patchReceiverPreferences(d.id, { volume }))
        .catch((e) => showToast(String(e)));
    } else {
      void patchReceiverPreferences(d.id, { volume }).catch((e) => showToast(String(e)));
    }
  }, 120);
}

window.addEventListener("pointerup", () => {
  if (!draggingVolume) return;
  draggingVolume = false;
  if (renderPending) render();
});

async function patchReceiverPreferences(id: string, patch: Partial<ReceiverPreferences>) {
  await invoke("patch_receiver_preferences", { receiverId: id, patch });
}

async function loadReceiverPreferences(id: string) {
  const preferences = await invoke<ReceiverPreferences>("get_receiver_preferences", { receiverId: id });
  if (connectedId !== id) return;
  primaryVolume = Math.round(preferences.volume * 100);
  deviceVolumes.set(id, primaryVolume);
  if (!playing) confirmedLatencyMs = preferences.latency_ms;
  publishState();
}

// ── Receiver list ───────────────────────────────────────────────────────────

const PLAY_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>';
const STOP_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 7h10v10H7z"/></svg>';
const SPEAKER_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><rect x="6" y="3" width="12" height="18" rx="5"/><circle cx="12" cy="14" r="3"/><circle cx="12" cy="7.5" r="1"/></svg>';

function rowStatus(d: Device): string {
  if (isPlayingOn(d)) return sessionRecovering ? "recovering" : "playing";
  if (connectingId === d.id || (actionBusy && !playing && connectedId === d.id)) return "connecting";
  if (d.available === false) return "unavailable";
  return "disconnected";
}

function render() {
  if (draggingVolume) { renderPending = true; return; }
  renderPending = false;
  devicesList.replaceChildren();
  const sorted = [...known.values()].sort((a, b) => {
    if (a.kind === "homepod" && b.kind !== "homepod") return -1;
    if (b.kind === "homepod" && a.kind !== "homepod") return 1;
    return a.name.localeCompare(b.name);
  });
  const busy = actionBusy || latencyApplying || stopping;
  for (const d of sorted) {
    const status = rowStatus(d);
    const active = isPlayingOn(d);
    const li = document.createElement("li");
    li.className = `receiver ${status}`;
    li.title = d.member_ids
      ? `${t("stereo_pair")} · ${d.member_names?.join(" + ") ?? ""}`
      : `${KIND_LABEL[d.kind]} · ${ipOf(d) ?? d.host}:${d.port}${d.supports_airplay2 ? " · AirPlay 2" : ""}`;

    const icon = document.createElement("span");
    icon.className = "kind-icon";
    icon.innerHTML = SPEAKER_ICON;

    const info = document.createElement("div");
    info.className = "info";
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = d.name;
    const state = document.createElement("span");
    state.className = "state";
    state.textContent = d.member_ids && !d.pair_complete ? t("incomplete_pair") : t(`rx_${status}`);
    info.append(name, state);

    const slider = document.createElement("input");
    slider.type = "range";
    slider.min = "0";
    slider.max = "100";
    slider.className = "row-volume";
    slider.value = String(rowVolume(d));
    slider.title = `${slider.value}%`;
    slider.setAttribute("aria-label", t("rx_volume", { name: d.name }));
    slider.addEventListener("pointerdown", () => { draggingVolume = true; });
    slider.addEventListener("input", () => { slider.title = `${slider.value}%`; setRowVolume(d, Number(slider.value)); });

    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = `row-toggle${active ? " playing" : ""}`;
    toggle.innerHTML = active ? STOP_ICON : PLAY_ICON;
    toggle.title = t(active ? "rx_stop" : "rx_play", { name: d.name });
    toggle.setAttribute("aria-label", toggle.title);
    toggle.disabled = busy || (!active && (d.available === false || (Boolean(d.member_ids) && !d.pair_complete)));
    toggle.addEventListener("click", () => void (active ? stopOn(d) : playOn(d)));

    li.append(icon, info, slider, toggle);
    devicesList.appendChild(li);
  }
  emptyEl.hidden = known.size > 0;
  publishState();
}

function refreshKnownDevices() {
  const previous = [...known.values()];
  known.clear();
  for (const [key, device] of groupDevices(discovered, showStereoPairs)) known.set(key, device);
  for (const device of previous) {
    if (!known.has(device.id) && (connectedId === device.id || activeRoutes.has(device.id))) known.set(device.id, { ...device, available: false });
  }
}

async function startScan() {
  discovered.clear();
  refreshKnownDevices();
  render();
  unlisten?.();
  unlisten = null;
  unlistenRemoved?.();
  unlistenRemoved = await listen<string>("airplay://device-removed", (event) => { discovered.delete(event.payload); refreshKnownDevices(); render(); });
  unlisten = await listen<Device>("airplay://device", (event) => {
    discovered.set(event.payload.id, event.payload);
    refreshKnownDevices();
    render();
  });
  try {
    await invoke("start_discovery_stream");
  } catch (err) {
    showToast(t("error_prefix", { err: String(err) }));
  }
}

// ── Preferences window ──────────────────────────────────────────────────────

function groupCandidates(): PrefsState["groupCandidates"] {
  const list: PrefsState["groupCandidates"] = [];
  for (const device of groupDevices(discovered).values()) {
    if (device.kind !== "homepod") continue;
    const route = routesFor(discovered, device)[0];
    if (route) list.push({ id: route.id, name: device.name });
  }
  return list;
}

function publishState() {
  const dev = connectedId ? known.get(connectedId) : null;
  const state: PrefsState = {
    playing,
    busy: actionBusy || stopping || latencyApplying,
    receiverName: dev?.name ?? null,
    multiDevice,
    activeCount: activeRoutes.size,
    latencyMs: confirmedLatencyMs,
    latencyApplying,
    stereoPairs: showStereoPairs,
    synchronized: synchronizedPlaying,
    groupCandidates: groupCandidates(),
  };
  void emit("ui://state", state).catch(() => {});
}

async function setMultiDevice(enabled: boolean) {
  if (actionBusy || latencyApplying) throw new Error(t("operation_busy"));
  if (!enabled && playing && activeRoutes.size > 1) {
    // Keep the primary receiver, drop the others.
    const keepId = connectedId && activeRoutes.has(connectedId) ? connectedId : activeRoutes.keys().next().value as string;
    actionBusy = true;
    render();
    try {
      for (const [id, route] of [...activeRoutes]) {
        if (id === keepId) continue;
        const ip = ipOf(route);
        if (!ip) throw new Error("device has no IP address");
        await invoke("remove_streaming", { ip, port: route.port });
        activeRoutes.delete(id);
      }
    } finally {
      actionBusy = false;
      render();
    }
  }
  await invoke("save_multi_device", { enabled });
  multiDevice = enabled;
}

async function applyLatency(latencyMs: number) {
  if (actionBusy || latencyApplying) throw new Error(t("operation_busy"));
  if (latencyMs === confirmedLatencyMs) return;
  latencyApplying = true;
  render();
  try {
    const restarted = await invoke<boolean>("confirm_latency", { latencyMs });
    activeGeneration = (await invoke<{ generation: number }>("get_session_status")).generation;
    confirmedLatencyMs = latencyMs;
    if (connectedId) await patchReceiverPreferences(connectedId, { latency_ms: latencyMs });
    if (restarted && !stopping) playing = true;
  } catch (err) {
    try {
      playing = (await invoke<ConnectionInfo | null>("is_streaming")) !== null;
    } catch {
      // Keep the previous state if the status query itself fails.
    }
    throw new Error(t("latency_error", { err: String(err) }));
  } finally {
    latencyApplying = false;
    render();
  }
}

async function groupPlay(firstId: string, secondId: string) {
  if (playing || actionBusy || latencyApplying) throw new Error(t("operation_busy"));
  if (!firstId || firstId === secondId) throw new Error(t("group_requires_two_receivers"));
  const selected = [firstId, secondId].map((id) => discovered.get(id));
  if (selected.some((d) => !d)) throw new Error(t("receiver_unavailable"));
  const routes = selected as Device[];
  actionBusy = true;
  const intent = ++intentSequence;
  render();
  try {
    await invoke("start_group_streaming", { deviceIds: routes.map((d) => d.id), stereoPair: false, latencyMs: confirmedLatencyMs });
    if (intent !== intentSequence) return;
    activeGeneration = (await invoke<{ generation: number }>("get_session_status")).generation;
    activeRoutes.clear();
    for (const route of routes) {
      activeRoutes.set([...groupDevices(discovered)].find(([, d]) => d.hardware_id === route.hardware_id)?.[0] ?? route.id, route);
    }
    connectedId = activeRoutes.keys().next().value ?? null;
    connectedRoute = connectedId ? activeRoutes.get(connectedId)! : null;
    playing = true;
    synchronizedPlaying = true;
    if (connectedId) await loadReceiverPreferences(connectedId);
  } finally {
    actionBusy = false;
    render();
  }
}

async function handlePrefsAction(action: PrefsAction) {
  try {
    switch (action.type) {
      case "multi":
        await setMultiDevice(action.enabled);
        break;
      case "latency":
        await applyLatency(action.ms);
        break;
      case "stereoPairs":
        if (playing) throw new Error(t("stop_before_pairs_change"));
        showStereoPairs = action.enabled;
        refreshKnownDevices();
        render();
        break;
      case "groupPlay":
        await groupPlay(action.first, action.second);
        break;
      case "rescan":
        await startScan();
        break;
    }
    reply(action, true);
  } catch (err) {
    reply(action, false, err instanceof Error ? err.message : String(err));
  }
  publishState();
}

function reply(action: PrefsAction, ok: boolean, message?: string) {
  const result: PrefsResult = { action: action.type, ok, message };
  void emit("prefs://result", result).catch(() => {});
}

void listen("prefs://hello", () => publishState());
void listen<PrefsAction>("prefs://action", (event) => void handlePrefsAction(event.payload));

// ── Startup ─────────────────────────────────────────────────────────────────

/// If a HomePod was saved, reconnect to it and start playback. Waits up to
/// RECONNECT_TIMEOUT_MS for mDNS, then probes the saved IP directly (routers
/// that do not propagate mDNS).
const RECONNECT_TIMEOUT_MS = 6000;

async function reconnectLast() {
  if (actionBusy || latencyApplying || playing) return;
  let last: PersistedDevice | null = null;
  try {
    last = await invoke<PersistedDevice | null>("get_last_device");
  } catch {
    last = null;
  }
  if (!last) return;

  const matchByIp = (): Device | null => {
    for (const d of known.values()) {
      if (d.addresses.some((a) => a === last!.ip)) return d;
    }
    return null;
  };
  const deadline = Date.now() + RECONNECT_TIMEOUT_MS;
  let found = matchByIp();
  while (!found && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 250));
    found = matchByIp();
  }
  if (!found) {
    try {
      const dev = await invoke<Device>("add_manual_device", { ip: last.ip, port: last.port, name: last.name });
      discovered.set(dev.id, dev);
      refreshKnownDevices();
      found = known.get(dev.id) ?? dev;
      render();
    } catch (err) {
      showToast(t("cant_find", { name: last.name, err: String(err) }), 6000);
      return;
    }
  }
  // Go straight to streaming (no separate pairing step).
  connectedId = found.id;
  connectedRoute = routesFor(discovered, found).find((route) =>
    route.port === last!.port && route.addresses.includes(last!.ip)
  ) ?? null;
  await loadReceiverPreferences(found.id);
  render();
  await startPlay();
}

async function initialize() {
  await Promise.all([
    invoke<number | null>("get_volume").then((v) => {
      if (v !== null && v !== undefined) primaryVolume = Math.round(Math.max(0, Math.min(1, v)) * 100);
    }).catch(() => {}),
    invoke<number | null>("get_latency").then((saved) => {
      if (saved !== null && saved >= 0 && saved <= 3000) confirmedLatencyMs = saved;
    }).catch(() => {}),
    invoke<boolean>("get_multi_device").then((enabled) => { multiDevice = enabled; }).catch(() => {}),
  ]);
  void startScan();
  // Preferences > General > "Reconnect to the last receiver and start playing".
  const general = await invoke<{ auto_play: boolean }>("get_general_settings").catch(() => ({ auto_play: true }));
  if (general.auto_play) await reconnectLast();
}

// Keeps the receivers' real volumes and the group state up to date.
let polling = false;
window.setInterval(async () => {
  if (polling) return;
  polling = true;
  try {
    const status = await invoke<{ generation: number; group_mode?: boolean }>("get_session_status");
    synchronizedPlaying = status.group_mode ?? synchronizedPlaying;
    if (!playing || actionBusy || latencyApplying) return;
    const revision = volumeRevision;
    const values = await invoke<{ ip: string; port: number; volume: number; readable: boolean }[]>("get_receiver_volumes");
    if (revision !== volumeRevision || !playing || status.generation !== activeGeneration || draggingVolume) return;
    let changed = false;
    for (const value of values) {
      if (!value.readable) continue;
      const id = [...activeRoutes].find(([, route]) => route.port === value.port && route.addresses.includes(value.ip))?.[0];
      if (!id) continue;
      const pct = Math.round(value.volume * 100);
      if (id === connectedId && primaryVolume !== pct) { primaryVolume = pct; changed = true; }
      if (deviceVolumes.get(id) !== pct) { deviceVolumes.set(id, pct); changed = true; }
    }
    if (changed) render();
  } catch {
    // The next poll retries; session errors are reported by the backend.
  } finally {
    polling = false;
  }
}, 3000);

// ── Compact window ──────────────────────────────────────────────────────────

document.getElementById("settings-toggle")!.addEventListener("click", () => {
  void invoke("open_preferences").catch((e) => showToast(String(e)));
});
document.getElementById("quit")!.addEventListener("click", () => void invoke("quit_app"));
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") void invoke("hide_main_window");
});
// The native window follows the content height (capped to the screen by the backend).
const appEl = document.getElementById("app") as HTMLDivElement;
let fittedHeight = 0;
// Extra pixels to request when the native window ends up smaller than asked
// (window borders, DPI rounding).
let heightCorrection = 0;
function fitWindow() {
  const height = Math.ceil(appEl.getBoundingClientRect().height) + heightCorrection;
  if (height === fittedHeight) return;
  fittedHeight = height;
  void invoke("fit_main_window", { height });
}
new ResizeObserver(fitWindow).observe(appEl);
window.addEventListener("resize", () => {
  const content = Math.ceil(appEl.getBoundingClientRect().height);
  const missing = content - window.innerHeight;
  // Small gap only: a large one means the window is capped to the screen.
  if (missing > 0 && missing <= 48) {
    heightCorrection += missing;
    fitWindow();
  }
});

setupAsyncErrorListener();
applyStaticTranslations();
void initialize();
