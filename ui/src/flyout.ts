import { invoke } from "@tauri-apps/api/core";
import { emit, listen } from "@tauri-apps/api/event";
import { getLang, setLang, t, type Lang } from "./i18n";

// Compact tray panel. The main window stays the single source of truth: it
// publishes its state on `flyout://state` and performs the actions requested
// here (`flyout://toggle`, `flyout://volume`).
export interface FlyoutState {
  name: string | null;
  status: "playing" | "busy" | "connected" | "idle";
  volume: number;
  lang: Lang;
}

const PLAY_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>';
const STOP_ICON = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7 7h10v10H7z"/></svg>';

const nameEl = document.getElementById("name")!;
const stateEl = document.getElementById("state")!;
const volume = document.getElementById("volume") as HTMLInputElement;
const volOut = document.getElementById("vol-out") as HTMLOutputElement;
const toggle = document.getElementById("toggle") as HTMLButtonElement;
const openMain = document.getElementById("open-main") as HTMLButtonElement;

// Ignore echoed volume updates while the user is moving the slider.
let lastLocalVolumeChange = 0;

function apply(state: FlyoutState) {
  if (state.lang !== getLang()) setLang(state.lang);
  nameEl.textContent = state.name ?? t("flyout_no_receiver");
  stateEl.textContent = t(`flyout_${state.status}`);
  stateEl.dataset.status = state.status;
  if (Date.now() - lastLocalVolumeChange > 1000) {
    volume.value = String(state.volume);
    volOut.textContent = `${state.volume}%`;
  }
  const playing = state.status === "playing";
  toggle.disabled = state.status === "busy" || !state.name;
  toggle.classList.toggle("playing", playing);
  toggle.innerHTML = playing ? STOP_ICON : PLAY_ICON;
  toggle.title = t(playing ? "flyout_stop" : "flyout_play");
  openMain.title = t("flyout_open");
}

void listen<FlyoutState>("flyout://state", (event) => apply(event.payload));

volume.addEventListener("input", () => {
  lastLocalVolumeChange = Date.now();
  volOut.textContent = `${volume.value}%`;
  void emit("flyout://volume", Number(volume.value));
});
toggle.addEventListener("click", () => void emit("flyout://toggle"));
openMain.addEventListener("click", () => void invoke("show_main_window"));
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") void invoke("hide_flyout");
});

// Ask the main window for a fresh state every time the panel is shown.
window.addEventListener("focus", () => void emit("flyout://hello"));
void emit("flyout://hello");
