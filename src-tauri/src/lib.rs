mod diagnostics;
mod power;
mod preferences;
mod session;
use cap_core::discovery::DiscoveryEvent;
use cap_core::streaming::{LocalBufferPolicy, PreparedLiveStream};
use diagnostics::{DiagnosticState, SessionRecord};
use preferences::{AudioOptions, ReceiverPreferences, ReceiverPreferencesPatch};
use session::{SessionControl, SessionEvent};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cap_core::{
    browse_once,
    pairing::{pair_homepod, DeviceDescriptor, PairedSession},
    probe::{manual_device, parse_manual_endpoint},
    probe_airplay, Device, Discovery,
};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{Emitter, Manager, State, WindowEvent};
use tauri_plugin_store::StoreExt;
use tracing_subscriber::EnvFilter;

/// Nombre del archivo de la store en `%APPDATA%/<bundle-id>/`. Lo lleva
/// `tauri-plugin-store` por nosotros — sólo necesitamos una clave estable.
static RECEIVER_SETTINGS_LOCK: Mutex<()> = Mutex::new(());
const STORE_FILE: &str = "settings.json";
const KEY_LAST_DEVICE: &str = "last_device";
const KEY_VOLUME: &str = "volume";
const KEY_LATENCY: &str = "latency";
const KEY_MULTI_DEVICE: &str = "multi_device";
const KEY_LOCAL_BUFFER: &str = "experimental_local_buffer";
const AUDIO_DIAGNOSTICS_INTERVAL: Duration = Duration::from_secs(10);
// Cooldown disabled: latency changes apply immediately (each one still
// restarts the AirPlay session).
const LATENCY_CHANGE_COOLDOWN: Duration = Duration::ZERO;
/// Tope para emparejar/preparar una sesión. Tras una suspensión un socket
/// medio abierto puede no responder nunca; ningún estado de conexión debe
/// quedarse colgado esperándolo.
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);
/// Windows concede ~2 s entre el aviso de suspensión y el sueño real.
const SUSPEND_TEARDOWN_BUDGET: Duration = Duration::from_millis(1500);

async fn within_deadline<T>(
    work: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(CONNECT_DEADLINE, work)
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(?CONNECT_DEADLINE, "connection attempt exceeded its deadline");
            Err("connect_timeout".into())
        })
}

fn latency_cooldown_remaining(last_change: Option<Instant>, now: Instant) -> Duration {
    last_change
        .map(|at| LATENCY_CHANGE_COOLDOWN.saturating_sub(now.saturating_duration_since(at)))
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod latency_tests {
    use super::*;

    #[test]
    fn changes_are_never_locked() {
        let changed_at = Instant::now();
        assert_eq!(latency_cooldown_remaining(Some(changed_at), changed_at), Duration::ZERO);
    }

    #[test]
    fn older_latency_settings_survive_the_slider_upgrade() {
        assert_eq!(decode_latency(&serde_json::json!("music")), Some(3000));
        assert_eq!(decode_latency(&serde_json::json!("video")), Some(2000));
        assert_eq!(decode_latency(&serde_json::json!("gaming")), Some(1000));
        assert_eq!(decode_latency(&serde_json::json!(700)), Some(700));
        assert_eq!(decode_latency(&serde_json::json!(750)), Some(750));
        assert_eq!(decode_latency(&serde_json::json!(725)), None);
    }
}

#[cfg(windows)]
struct MmcssGuard(*mut std::ffi::c_void);

#[cfg(windows)]
impl Drop for MmcssGuard {
    fn drop(&mut self) {
        #[link(name = "avrt")]
        extern "system" {
            fn AvRevertMmThreadCharacteristics(handle: *mut std::ffi::c_void) -> i32;
        }
        unsafe {
            if AvRevertMmThreadCharacteristics(self.0) == 0 {
                tracing::warn!("airplay-pump: no se pudo liberar el registro MMCSS");
            }
        }
    }
}

#[cfg(windows)]
fn register_audio_pump_mmcss() -> Option<MmcssGuard> {
    const AVRT_PRIORITY_CRITICAL: i32 = 2;
    #[link(name = "avrt")]
    extern "system" {
        fn AvSetMmThreadCharacteristicsW(
            task: *const u16,
            task_index: *mut u32,
        ) -> *mut std::ffi::c_void;
        fn AvSetMmThreadPriority(handle: *mut std::ffi::c_void, priority: i32) -> i32;
        fn AvRevertMmThreadCharacteristics(handle: *mut std::ffi::c_void) -> i32;
    }

    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_index = 0;
    unsafe {
        let handle = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index);
        if handle.is_null() {
            tracing::warn!("airplay-pump: no se pudo registrar el hilo en MMCSS Pro Audio");
            return None;
        }
        if AvSetMmThreadPriority(handle, AVRT_PRIORITY_CRITICAL) == 0 {
            tracing::warn!("airplay-pump: no se pudo asignar prioridad MMCSS crítica");
            let _ = AvRevertMmThreadCharacteristics(handle);
            return None;
        }
        tracing::info!(
            task_index,
            "airplay-pump: hilo registrado en MMCSS Pro Audio"
        );
        Some(MmcssGuard(handle))
    }
}

#[cfg(not(windows))]
fn register_audio_pump_mmcss() {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct LastDevice {
    ip: String,
    port: u16,
    name: String,
}

/// Mantiene el `Discovery` activo entre llamadas para no recrear el daemon
/// (cada uno abre montones de sockets en 5353 — sin esto, cada hot-reload
/// del frontend filtraba un daemon nuevo).
///
/// `cache` guarda todo dispositivo visto (mDNS o manual). El frontend llama
/// `start_discovery_stream` cada vez que el usuario pulsa "Buscar"; sin
/// replay, los anuncios mDNS ya consumidos no se vuelven a emitir y la lista
/// queda vacía hasta el siguiente broadcast (puede tardar minutos).
#[derive(Default)]
struct DiscoveryState {
    inner: Mutex<Option<Discovery>>,
    cache: Mutex<HashMap<String, Device>>,
}

/// Mantiene la sesión RTSP autenticada con el HomePod. Por ahora, sólo una a la
/// vez (alcance MVP: un solo HomePod). El Connection no es Send/Sync trivial
/// porque tiene streams TCP/UDP; usamos un Mutex async para acceso seguro.
#[derive(Default)]
struct ConnectionState {
    control: SessionControl,
    inner: tokio::sync::Mutex<Option<PairedSession>>,
}

/// Each receiver owns its RTSP connection and feedback task. All receivers in
/// an active stream share one system-audio capture and one pump.
struct ActiveOutput {
    connection: std::sync::Arc<tokio::sync::Mutex<cap_core::streaming::Connection>>,
    _heartbeat: cap_core::streaming::HeartbeatGuard,
    ip: String,
    port: u16,
    name: String,
    volume: f32,
    metrics: std::sync::Arc<cap_core::stream_metrics::StreamMetrics>,
    volume_read_supported: Option<bool>,
}

impl ActiveOutput {
    fn info(&self, sample_rate: u32, channels: u8) -> StreamingInfo {
        StreamingInfo {
            ip: self.ip.clone(),
            port: self.port,
            name: self.name.clone(),
            sample_rate,
            channels,
            volume: self.volume,
        }
    }
}

struct ActiveStream {
    outputs: HashMap<String, ActiveOutput>,
    senders: std::sync::Arc<Mutex<HashMap<String, cap_core::streaming::LiveFrameSender>>>,
    _capture: Option<Box<dyn audio_capture::Capture>>,
    diagnostics: SessionRecord,
    policy: LocalBufferPolicy,
    pump: Option<std::thread::JoinHandle<()>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    latency_ms: u32,
    sample_rate: u32,
    channels: u8,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    group_mode: bool,
    target_order: Vec<String>,
}

impl ActiveStream {
    fn apply_mute(&self, app: &tauri::AppHandle) -> Result<(), String> {
        if get_audio_options(app.clone())?.mute_local {
            let path = app
                .path()
                .app_data_dir()
                .map_err(|e| e.to_string())?
                .join("local-mute-recovery.json");
            self._capture
                .as_ref()
                .ok_or("capture_interrupted")?
                .request_local_mute(path)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    async fn stop_outputs(&mut self) {
        for output in self.outputs.values_mut() {
            output._heartbeat.shutdown();
            let stopped = tokio::time::timeout(Duration::from_secs(3), async {
                output.connection.lock().await.disconnect().await
            })
            .await;
            if !matches!(stopped, Ok(Ok(()))) {
                tracing::warn!("AirPlay output did not disconnect within the shutdown deadline");
            }
        }
    }
    fn shutdown(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
        self.senders.lock().unwrap().clear();
        if let Some(capture) = self._capture.take() {
            capture.stop();
        }
        self.diagnostics.lock().unwrap().finish();
        // _heartbeat se aborta solo al dropearse.
    }
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct StreamingState {
    control: SessionControl,
    operation: tokio::sync::Mutex<()>,
    inner: tokio::sync::Mutex<Option<ActiveStream>>,
    last_latency_change: tokio::sync::Mutex<Option<Instant>>,
    phase: Mutex<(u64, &'static str)>,
}

fn output_key(ip: &str, port: u16) -> String {
    format!("{ip}:{port}")
}

#[tauri::command]
async fn discover_devices(timeout_ms: Option<u64>) -> Result<Vec<Device>, String> {
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(3_000));
    browse_once(timeout).await.map_err(|e| e.to_string())
}

#[tauri::command]
async fn start_discovery_stream(
    app: tauri::AppHandle,
    state: State<'_, DiscoveryState>,
) -> Result<(), String> {
    // Replay del cache: si el usuario pulsó "Buscar" otra vez, el listener de JS
    // se acaba de recrear con `known` vacío, así que reemitimos lo que ya
    // conocemos para rellenar la lista al instante.
    let cached: Vec<Device> = state
        .cache
        .lock()
        .map_err(|e| e.to_string())?
        .values()
        .cloned()
        .collect();
    for dev in &cached {
        let _ = app.emit("airplay://device", dev);
    }

    // Lock + spawn + drop del guard antes de devolver. Sin await durante el lock,
    // así el std::sync::Mutex es seguro aunque la fn sea async.
    let mut rx = {
        let mut slot = state.inner.lock().map_err(|e| e.to_string())?;
        if slot.is_some() {
            tracing::debug!("discovery ya en curso, replayé {} cacheados", cached.len());
            return Ok(());
        }
        let discovery = Discovery::new().map_err(|e| e.to_string())?;
        let rx = discovery.browse_events().map_err(|e| e.to_string())?;
        *slot = Some(discovery);
        rx
    };

    let app_pump = app.clone();
    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                DiscoveryEvent::Resolved(device) => {
                    if let Some(state) = app_pump.try_state::<DiscoveryState>() {
                        if let Ok(mut cache) = state.cache.lock() {
                            cache.insert(device.id.clone(), device.clone());
                        }
                    }
                    let _ = app_pump.emit("airplay://device", &device);
                }
                DiscoveryEvent::Removed(id) => {
                    if let Some(state) = app_pump.try_state::<DiscoveryState>() {
                        if let Ok(mut cache) = state.cache.lock() {
                            if let Some(device) = cache.get_mut(&id) {
                                device.available = false;
                            }
                        }
                    }
                    let _ = app_pump.emit("airplay://device-removed", &id);
                }
            }
        }
    });

    Ok(())
}

#[tauri::command]
async fn stop_discovery_stream(state: State<'_, DiscoveryState>) -> Result<(), String> {
    let mut slot = state.inner.lock().map_err(|e| e.to_string())?;
    if let Some(discovery) = slot.take() {
        discovery.shutdown();
    }
    Ok(())
}

/// Tira el daemon mDNS y su caché: tras dormir sus sockets ya no sirven
/// (os error 10065) y la caché repetiría receptores marcados como ausentes.
fn reset_discovery(app: &tauri::AppHandle) {
    let state = app.state::<DiscoveryState>();
    if let Some(discovery) = state.inner.lock().ok().and_then(|mut slot| slot.take()) {
        discovery.shutdown();
    }
    if let Ok(mut cache) = state.cache.lock() {
        cache.clear();
    };
}

/// Suspensión: cerrar la sesión limpiamente antes de que mueran los sockets.
/// Reanudación: repetir el cierre por si la suspensión no dio tiempo, y dejar
/// todo en "disconnected" para que la UI relance el escaneo.
fn handle_power_event(app: &tauri::AppHandle, event: power::PowerEvent) {
    tracing::info!(?event, "transición de energía de Windows");
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let task_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = stop_streaming(task_app.clone(), task_app.state::<StreamingState>()).await;
        let _ = disconnect_device(task_app.state::<ConnectionState>()).await;
        reset_discovery(&task_app);
        let code = match event {
            power::PowerEvent::Suspend => "suspend",
            power::PowerEvent::Resume => "resume",
        };
        let _ = task_app.emit("airplay://power", code);
        let _ = done_tx.send(());
    });
    if event == power::PowerEvent::Suspend && done_rx.recv_timeout(SUSPEND_TEARDOWN_BUDGET).is_err()
    {
        tracing::warn!("la sesión no terminó de cerrarse antes de la suspensión");
    }
}

#[derive(serde::Serialize, Clone)]
struct StreamingInfo {
    ip: String,
    port: u16,
    name: String,
    sample_rate: u32,
    channels: u8,
    volume: f32,
}

fn resolve_descriptor(
    app: &tauri::AppHandle,
    device_id: Option<&str>,
    ip: IpAddr,
    port: u16,
    name: &str,
) -> Result<DeviceDescriptor, String> {
    let state = app.state::<DiscoveryState>();
    let cache = state.cache.lock().map_err(|e| e.to_string())?;
    let route = if let Some(id) = device_id {
        cache
            .get(id)
            .filter(|d| d.port == port && d.addresses.contains(&ip))
    } else {
        cache
            .values()
            .find(|d| d.port == port && d.addresses.contains(&ip) && !d.manual)
            .or_else(|| {
                cache
                    .values()
                    .find(|d| d.port == port && d.addresses.contains(&ip))
            })
    };
    if device_id.is_some() && route.is_none() {
        return Err("receiver_unavailable".into());
    }
    match route {
        Some(device) => device
            .descriptor(ip, &cache.values().cloned().collect::<Vec<_>>())
            .map_err(|e| e.to_string()),
        None => Ok(DeviceDescriptor {
            ip,
            port,
            name: name.into(),
            mac: None,
            model: None,
            features: None,
            advertised: None,
        }),
    }
}

async fn prepare_output(
    ip: &str,
    port: u16,
    name: &str,
    volume: f32,
    latency_ms: u32,
    policy: LocalBufferPolicy,
    diagnostics: &SessionRecord,
    app: &tauri::AppHandle,
) -> Result<PreparedLiveStream, String> {
    let parsed: IpAddr = ip.parse().map_err(|e| format!("invalid IP: {e}"))?;
    let descriptor = resolve_descriptor(app, None, parsed, port, name)?;
    cap_core::streaming::prepare_live_stream(descriptor, Some(volume), Some(latency_ms), policy)
        .await
        .map_err(|e| {
            diagnostics.lock().unwrap().record_failure(e.stage_code());
            format!("stream: {e}")
        })
}

async fn start_output(
    prepared: PreparedLiveStream,
    ip: &str,
    port: u16,
    name: &str,
    volume: f32,
    diagnostics: &SessionRecord,
) -> Result<ActiveOutput, String> {
    let metrics = prepared.metrics();
    let handle = prepared.start().await.map_err(|e| {
        diagnostics.lock().unwrap().record_failure(e.stage_code());
        format!("stream: {e}")
    })?;
    let (_, connection, heartbeat, _, _) = handle.into_parts();
    Ok(ActiveOutput {
        connection,
        _heartbeat: heartbeat,
        ip: ip.to_string(),
        port,
        name: name.to_string(),
        volume,
        metrics,
        volume_read_supported: None,
    })
}

async fn attach_output(
    active: &mut ActiveStream,
    ip: &str,
    port: u16,
    name: &str,
    volume: f32,
    app: &tauri::AppHandle,
) -> Result<(), String> {
    let prepared = prepare_output(
        ip,
        port,
        name,
        volume,
        active.latency_ms,
        active.policy,
        &active.diagnostics,
        app,
    )
    .await?;
    if (prepared.sample_rate(), prepared.channels()) != (active.sample_rate, active.channels) {
        return Err("receivers do not share an audio format".into());
    }
    let key = output_key(ip, port);
    let sender = prepared.sender();
    active
        .diagnostics
        .lock()
        .unwrap()
        .add_output(sender.clone(), prepared.metrics());
    active
        .senders
        .lock()
        .map_err(|e| e.to_string())?
        .insert(key.clone(), sender);
    // The existing pump feeds pre-roll while this receiver starts.
    match start_output(prepared, ip, port, name, volume, &active.diagnostics).await {
        Ok(output) => {
            active.outputs.insert(key, output);
            Ok(())
        }
        Err(error) => {
            active
                .senders
                .lock()
                .map_err(|e| e.to_string())?
                .remove(&key);
            Err(error)
        }
    }
}

/// Prepare the network before capturing. Feed live PCM before waiting for pre-roll.
/// The previous stream remains owned by the caller until this returns successfully.
async fn prepare_stream(
    app: tauri::AppHandle,
    targets: &[(String, u16, String, f32)],
    latency_ms: u32,
    generation: u64,
) -> Result<ActiveStream, String> {
    use audio_capture::{CaptureFormat, CapturePolicy};
    use std::sync::{atomic::AtomicBool, Arc};
    cap_core::streaming::validate_latency_ms(latency_ms).map_err(|e| e.to_string())?;
    let (first_ip, first_port, first_name, first_volume) =
        targets.first().ok_or("no receivers")?;
    let policy = if get_experimental_local_buffer(app.clone())? {
        LocalBufferPolicy::LowLatency
    } else {
        LocalBufferPolicy::Stable
    };
    let diagnostics = app.state::<DiagnosticState>().begin(latency_ms, policy);
    let _preparation = diagnostics::PreparationGuard::new(diagnostics.clone());
    let prepared = match prepare_output(
        first_ip,
        *first_port,
        first_name,
        *first_volume,
        latency_ms,
        policy,
        &diagnostics,
        &app,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            diagnostics.lock().unwrap().failed();
            return Err(error);
        }
    };
    let sample_rate = prepared.sample_rate();
    let channels = prepared.channels();
    let began = Instant::now();
    let capture_policy = if policy == LocalBufferPolicy::LowLatency {
        CapturePolicy::LowLatency
    } else {
        CapturePolicy::Stable
    };
    let audio_options = get_audio_options(app.clone())?;
    let (capture, rx) = match tokio::task::spawn_blocking(move || {
        audio_capture::start_loopback_with_device(
            CaptureFormat::AIRPLAY_DEFAULT,
            capture_policy,
            audio_options.device_id,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    {
        Ok(value) => value,
        Err(error) => {
            diagnostics.lock().unwrap().record_failure("capture_start");
            diagnostics.lock().unwrap().failed();
            return Err(format!("captura: {error}"));
        }
    };
    diagnostics.lock().unwrap().capture_started(began.elapsed());
    let key = output_key(first_ip, *first_port);
    let sender = prepared.sender();
    diagnostics
        .lock()
        .unwrap()
        .add_output(sender.clone(), prepared.metrics());
    let senders = Arc::new(Mutex::new(HashMap::from([(key.clone(), sender)])));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let pump_senders = senders.clone();
    let pump_diagnostics = diagnostics.clone();
    let pump_app = app.clone();
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(generation));
    let pump_generation = generation.clone();
    let pump = std::thread::Builder::new()
        .name("airplay-pump".into())
        .spawn(move || {
            pump_loop(
                pump_app,
                rx,
                pump_senders,
                sample_rate,
                channels,
                stop_thread,
                pump_diagnostics,
                pump_generation,
            )
        })
        .map_err(|e| {
            diagnostics.lock().unwrap().failed();
            format!("pump thread: {e}")
        })?;
    let mut active = ActiveStream {
        outputs: HashMap::new(),
        senders,
        _capture: Some(capture),
        diagnostics,
        policy,
        pump: Some(pump),
        stop,
        latency_ms,
        sample_rate,
        channels,
        generation,
        group_mode: false,
        target_order: targets
            .iter()
            .map(|(ip, port, _, _)| output_key(ip, *port))
            .collect(),
    };
    let first = start_output(
        prepared,
        first_ip,
        *first_port,
        first_name,
        *first_volume,
        &active.diagnostics,
    )
    .await?;
    if active.stop.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("capture_interrupted".into());
    }
    active.outputs.insert(key, first);
    for (ip, port, name, volume) in targets.iter().skip(1) {
        attach_output(&mut active, ip, *port, name, *volume, &app).await?;
    }
    if active.stop.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("capture_interrupted".into());
    }
    active.diagnostics.lock().unwrap().streaming();
    Ok(active)
}

async fn prepare_targets(
    app: tauri::AppHandle,
    targets: &[(String, u16, String, f32)],
    latency_ms: u32,
    generation: u64,
    group_mode: bool,
) -> Result<ActiveStream, String> {
    if group_mode {
        prepare_synchronized_stream(app, targets, latency_ms, generation).await
    } else {
        prepare_stream(app, targets, latency_ms, generation).await
    }
}

async fn prepare_synchronized_stream(
    app: tauri::AppHandle,
    targets: &[(String, u16, String, f32)],
    latency_ms: u32,
    generation: u64,
) -> Result<ActiveStream, String> {
    if targets.len() != 2 {
        return Err("group_requires_two_receivers".into());
    }
    let policy = if get_experimental_local_buffer(app.clone())? {
        LocalBufferPolicy::LowLatency
    } else {
        LocalBufferPolicy::Stable
    };
    let diagnostics = app.state::<DiagnosticState>().begin(latency_ms, policy);
    let _preparation = diagnostics::PreparationGuard::new(diagnostics.clone());
    let descriptors = targets
        .iter()
        .map(|(ip, port, name, _)| {
            resolve_descriptor(
                &app,
                None,
                ip.parse().map_err(|_| "invalid_ip")?,
                *port,
                name,
            )
        })
        .collect::<Result<Vec<_>, String>>()?;
    let volumes = targets.iter().map(|t| t.3).collect::<Vec<_>>();
    let prepared =
        cap_core::streaming::prepare_group_live_stream(descriptors, &volumes, latency_ms, policy)
            .await
            .map_err(|e| {
                diagnostics.lock().unwrap().record_failure(e.stage_code());
                format!("stream: {e}")
            })?;
    let options = get_audio_options(app.clone())?;
    let capture_policy = if policy == LocalBufferPolicy::LowLatency {
        audio_capture::CapturePolicy::LowLatency
    } else {
        audio_capture::CapturePolicy::Stable
    };
    let began = Instant::now();
    let (capture, rx) = tokio::task::spawn_blocking(move || {
        audio_capture::start_loopback_with_device(
            audio_capture::CaptureFormat::AIRPLAY_DEFAULT,
            capture_policy,
            options.device_id,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| {
        diagnostics.lock().unwrap().record_failure("capture_start");
        e.to_string()
    })?;
    diagnostics.lock().unwrap().capture_started(began.elapsed());
    let sender = prepared.sender.clone();
    for metrics in &prepared.metrics {
        diagnostics
            .lock()
            .unwrap()
            .add_group_output(sender.clone(), metrics.clone());
    }
    let senders = std::sync::Arc::new(Mutex::new(HashMap::from([(
        output_key(&targets[0].0, targets[0].1),
        sender,
    )])));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (pump_senders, pump_stop, pump_diagnostics, pump_app) = (
        senders.clone(),
        stop.clone(),
        diagnostics.clone(),
        app.clone(),
    );
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(generation));
    let pump_generation = generation.clone();
    let pump = std::thread::Builder::new()
        .name("airplay-group-pump".into())
        .spawn(move || {
            pump_loop(
                pump_app,
                rx,
                pump_senders,
                44100,
                2,
                pump_stop,
                pump_diagnostics,
                pump_generation,
            )
        })
        .map_err(|e| e.to_string())?;
    let mut active = ActiveStream {
        outputs: HashMap::new(),
        senders,
        _capture: Some(capture),
        diagnostics,
        policy,
        pump: Some(pump),
        stop,
        latency_ms,
        sample_rate: 44100,
        channels: 2,
        generation,
        group_mode: true,
        target_order: targets
            .iter()
            .map(|(ip, port, _, _)| output_key(ip, *port))
            .collect(),
    };
    let handle = prepared.start().await.map_err(|e| {
        active
            .diagnostics
            .lock()
            .unwrap()
            .record_failure(e.stage_code());
        format!("stream: {e}")
    })?;
    for ((connection, heartbeat, metrics), (ip, port, name, volume)) in
        handle.outputs.into_iter().zip(targets)
    {
        active.outputs.insert(
            output_key(ip, *port),
            ActiveOutput {
                connection,
                _heartbeat: heartbeat,
                ip: ip.clone(),
                port: *port,
                name: name.clone(),
                volume: *volume,
                metrics,
                volume_read_supported: None,
            },
        );
    }
    if active.stop.load(std::sync::atomic::Ordering::SeqCst) {
        return Err("capture_interrupted".into());
    }
    active.diagnostics.lock().unwrap().streaming();
    Ok(active)
}

#[tauri::command]
async fn start_group_streaming(
    app: tauri::AppHandle,
    device_ids: Vec<String>,
    stereo_pair: bool,
    latency_ms: u32,
    state: State<'_, StreamingState>,
) -> Result<Vec<StreamingInfo>, String> {
    if device_ids.len() != 2 || device_ids[0] == device_ids[1] {
        return Err("group_requires_two_receivers".into());
    }
    let targets = {
        let discovery = app.state::<DiscoveryState>();
        let cache = discovery.cache.lock().map_err(|e| e.to_string())?;
        let devices = device_ids
            .iter()
            .map(|id| {
                cache
                    .get(id)
                    .filter(|d| d.available)
                    .cloned()
                    .ok_or_else(|| "receiver_unavailable".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if devices[0].receiver_id() == devices[1].receiver_id() {
            return Err("group_requires_two_receivers".into());
        }
        if stereo_pair
            && (devices[0].tight_sync_id.is_none()
                || devices[0].tight_sync_id != devices[1].tight_sync_id)
        {
            return Err("incomplete_stereo_pair".into());
        }
        devices
            .into_iter()
            .map(|d| {
                let ip = d
                    .addresses
                    .iter()
                    .find(|ip| ip.is_ipv4())
                    .or(d.addresses.first())
                    .ok_or_else(|| "receiver_unavailable".to_string())?;
                let preferences = get_receiver_preferences(app.clone(), d.receiver_id())?;
                Ok((ip.to_string(), d.port, d.name, preferences.volume))
            })
            .collect::<Result<Vec<_>, String>>()?
    };
    let mut token = state.control.begin();
    let generation = token.generation;
    let _operation = state.operation.lock().await;
    emit_session(&app, generation, "preparing");
    let prepared = tokio::select! { biased; _ = token.cancelled() => return Err("session_cancelled".into()), result = within_deadline(prepare_synchronized_stream(app.clone(), &targets, latency_ms, generation)) => result };
    let mut active = match prepared {
        Ok(value) => value,
        Err(error) => {
            monitor_active(&app, &state, generation).await;
            return Err(error);
        }
    };
    let info = active.outputs.values().map(|o| o.info(44100, 2)).collect();
    let previous = {
        let mut slot = state.inner.lock().await;
        if state.control.current() != generation {
            active.shutdown();
            return Err("session_cancelled".into());
        }
        slot.replace(active)
    };
    if let Some(mut previous) = previous {
        previous.shutdown();
        previous.stop_outputs().await;
    }
    if let Some(active) = state.inner.lock().await.as_ref() {
        let _ = active.apply_mute(&app);
    }
    start_session_monitor(app.clone(), generation);
    emit_session(&app, generation, "streaming");
    Ok(info)
}

#[tauri::command]
async fn start_streaming(
    app: tauri::AppHandle,
    device_id: Option<String>,
    ip: String,
    port: Option<u16>,
    name: Option<String>,
    volume: Option<f32>,
    latency_ms: Option<u32>,
    state: State<'_, StreamingState>,
) -> Result<StreamingInfo, String> {
    let parsed: IpAddr = ip.parse().map_err(|e| format!("invalid IP: {e}"))?;
    let port = port.unwrap_or(7000);
    let name = name.unwrap_or_else(|| format!("HomePod {parsed}"));
    let volume = volume
        .unwrap_or(cap_core::streaming::DEFAULT_INITIAL_VOLUME)
        .clamp(0.0, 1.0);
    let latency_ms = latency_ms.unwrap_or(cap_core::streaming::DEFAULT_LATENCY_MS);
    if !volume.is_finite() {
        return Err("invalid_volume".into());
    }
    resolve_descriptor(&app, device_id.as_deref(), parsed, port, &name)?;
    let mut token = state.control.begin();
    let _operation = state.operation.lock().await;
    if token.generation != state.control.current() {
        return Err("session_cancelled".into());
    }
    let generation = token.generation;
    emit_session(&app, generation, "preparing");
    let targets = [(parsed.to_string(), port, name, volume)];
    let prepared = tokio::select! {
        biased;
        _ = token.cancelled() => return Err("session_cancelled".into()),
        result = within_deadline(prepare_stream(app.clone(), &targets, latency_ms, generation)) => result,
    };
    let mut active = match prepared {
        Ok(value) => value,
        Err(error) => {
            monitor_active(&app, &state, generation).await;
            return Err(error);
        }
    };
    let info = active
        .outputs
        .values()
        .next()
        .unwrap()
        .info(active.sample_rate, active.channels);
    let previous = {
        let mut slot = state.inner.lock().await;
        if generation != state.control.current() {
            active.shutdown();
            return Err("session_cancelled".into());
        }
        slot.replace(active)
    };
    if let Some(mut previous) = previous {
        previous.shutdown();
        previous.stop_outputs().await;
    }
    if let Some(active) = state.inner.lock().await.as_ref() {
        let _ = active.apply_mute(&app);
    }
    start_session_monitor(app.clone(), generation);
    emit_session(&app, generation, "streaming");
    Ok(info)
}

/// Add a receiver to the existing capture without interrupting other outputs.
#[tauri::command]
async fn add_streaming(
    app: tauri::AppHandle,
    device_id: Option<String>,
    ip: String,
    port: Option<u16>,
    name: Option<String>,
    volume: Option<f32>,
    latency_ms: Option<u32>,
    state: State<'_, StreamingState>,
) -> Result<StreamingInfo, String> {
    let parsed: IpAddr = ip.parse().map_err(|e| format!("invalid IP: {e}"))?;
    let port = port.unwrap_or(7000);
    let name = name.unwrap_or_else(|| format!("HomePod {parsed}"));
    let volume = volume
        .unwrap_or(cap_core::streaming::DEFAULT_INITIAL_VOLUME)
        .clamp(0.0, 1.0);
    if !volume.is_finite() {
        return Err("invalid_volume".into());
    }
    resolve_descriptor(&app, device_id.as_deref(), parsed, port, &name)?;
    let mut token = state.control.begin();
    let _operation = state.operation.lock().await;
    if token.generation != state.control.current() {
        return Err("session_cancelled".into());
    }
    let mut slot = state.inner.lock().await;
    let Some(active) = slot.as_mut() else {
        let latency = latency_ms.unwrap_or(cap_core::streaming::DEFAULT_LATENCY_MS);
        let generation = token.generation;
        let targets = [(parsed.to_string(), port, name, volume)];
        drop(slot);
        let stream = tokio::select! { biased; _ = token.cancelled() => return Err("session_cancelled".into()), result = within_deadline(prepare_stream(
            app.clone(),
            &targets,
            latency,
            generation,
        )) => result? };
        let info = stream
            .outputs
            .values()
            .next()
            .unwrap()
            .info(stream.sample_rate, stream.channels);
        let mut slot = state.inner.lock().await;
        if state.control.current() != token.generation {
            return Err("session_cancelled".into());
        }
        *slot = Some(stream);
        drop(slot);
        monitor_active(&app, &state, token.generation).await;
        return Ok(info);
    };
    if active.group_mode {
        drop(slot);
        monitor_active(&app, &state, token.generation).await;
        return Err("stop_before_group_change".into());
    }
    if let Some(requested) = latency_ms {
        if requested != active.latency_ms {
            drop(slot);
            monitor_active(&app, &state, token.generation).await;
            return Err("latency does not match the current playback".to_string());
        }
    }
    let key = output_key(&parsed.to_string(), port);
    if let Some(output) = active.outputs.get(&key) {
        let info = output.info(active.sample_rate, active.channels);
        drop(slot);
        monitor_active(&app, &state, token.generation).await;
        return Ok(info);
    }
    let address = parsed.to_string();
    let attached = tokio::select! {
        biased;
        _ = token.cancelled() => Err("session_cancelled".into()),
        result = within_deadline(attach_output(active, &address, port, &name, volume, &app)) => result,
    };
    if let Err(error) = attached {
        active.senders.lock().unwrap().remove(&key);
        drop(slot);
        monitor_active(&app, &state, token.generation).await;
        return Err(error);
    }

    active.target_order.push(key.clone());
    let info = active
        .outputs
        .get(&key)
        .unwrap()
        .info(active.sample_rate, active.channels);
    drop(slot);
    monitor_active(&app, &state, token.generation).await;
    Ok(info)
}

fn pump_loop(
    _app: tauri::AppHandle,
    rx: audio_capture::CaptureReceiver,
    senders: std::sync::Arc<Mutex<HashMap<String, cap_core::streaming::LiveFrameSender>>>,
    sample_rate: u32,
    channels: u8,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    diagnostics: SessionRecord,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
) {
    use cap_core::streaming::LivePcmFrame;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let _mmcss = register_audio_pump_mmcss();
    let mut unexpected_exit = false;
    let mut frames_forwarded = 0u64;
    let mut frames_dropped = 0u64;
    let mut last_frame = Instant::now();
    let mut last_report = last_frame;
    let mut max_capture_gap = Duration::ZERO;
    while !stop.load(Ordering::SeqCst) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(frame) => {
                let now = Instant::now();
                max_capture_gap = max_capture_gap.max(now.saturating_duration_since(last_frame));
                last_frame = now;
                let Ok(targets) = senders.lock() else {
                    unexpected_exit = true;
                    break;
                };
                let mut samples = Some(frame.samples);
                let count = targets.len();
                for (index, sender) in targets.values().enumerate() {
                    let payload = if index + 1 == count {
                        samples.take().unwrap()
                    } else {
                        samples.as_ref().unwrap().clone()
                    };
                    if !sender.try_send_at(
                        LivePcmFrame {
                            samples: payload,
                            channels,
                            sample_rate,
                        },
                        frame.captured_at,
                    ) {
                        frames_dropped += 1;
                    } else {
                        frames_forwarded += 1;
                    }
                }

                if last_report.elapsed() >= AUDIO_DIAGNOSTICS_INTERVAL {
                    tracing::info!(
                        frames_forwarded,
                        frames_dropped,
                        max_capture_gap_ms = max_capture_gap.as_millis(),
                        "airplay-pump diagnostics"
                    );
                    diagnostics.lock().unwrap().update(
                        rx.diagnostics(),
                        frames_forwarded,
                        frames_dropped,
                        max_capture_gap,
                    );
                    last_report = Instant::now();
                    max_capture_gap = Duration::ZERO;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => {
                // El canal de captura se cerró sin que nosotros pidamos shutdown:
                // device removed, driver crash, parec mató al subproceso, etc.
                unexpected_exit = !stop.load(Ordering::SeqCst);
                break;
            }
        }
    }
    diagnostics.lock().unwrap().update(
        rx.diagnostics(),
        frames_forwarded,
        frames_dropped,
        max_capture_gap,
    );
    if unexpected_exit {
        stop.store(true, Ordering::SeqCst);
        diagnostics.lock().unwrap().record_failure("capture");
        diagnostics.lock().unwrap().failed();
        tracing::warn!(
            generation = generation.load(Ordering::SeqCst),
            "capture stopped unexpectedly; session monitor will recover or report the failure"
        );
    } else {
        tracing::info!("airplay-pump thread exit");
    }
}

#[tauri::command]
async fn stop_streaming(
    app: tauri::AppHandle,
    state: State<'_, StreamingState>,
) -> Result<(), String> {
    state.control.cancel();
    emit_session(&app, state.control.current(), "disconnected");
    let active = state.inner.lock().await.take();
    if let Some(mut active) = active {
        active.shutdown();
        active.stop_outputs().await;
    }
    Ok(())
}

#[tauri::command]
async fn remove_streaming(
    ip: String,
    port: u16,
    state: State<'_, StreamingState>,
) -> Result<(), String> {
    let mut slot = state.inner.lock().await;
    let Some(active) = slot.as_mut() else {
        return Ok(());
    };
    if active.group_mode {
        let mut group = slot.take().unwrap();
        drop(slot);
        state.control.cancel();
        group.shutdown();
        group.stop_outputs().await;
        return Ok(());
    }
    let key = output_key(&ip, port);
    active
        .senders
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&key);
    active.target_order.retain(|target| target != &key);
    if let Some(mut output) = active.outputs.remove(&key) {
        output._heartbeat.shutdown();
        let _ = tokio::time::timeout(Duration::from_secs(3), async {
            output.connection.lock().await.disconnect().await
        })
        .await;
    }
    if active.outputs.is_empty() {
        if let Some(mut empty) = slot.take() {
            empty.shutdown();
        }
    }
    Ok(())
}

#[tauri::command]
async fn set_stream_volume(
    app: tauri::AppHandle,
    volume: f32,
    state: State<'_, StreamingState>,
) -> Result<f32, String> {
    let mut slot = state.inner.lock().await;
    let active = slot
        .as_mut()
        .ok_or_else(|| "no active stream".to_string())?;
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err("invalid_volume".into());
    }
    let v = volume;
    let mut errors = Vec::new();
    for output in active.outputs.values_mut() {
        let mut conn = output.connection.lock().await;
        match tokio::time::timeout(Duration::from_secs(3), conn.set_volume(v)).await {
            Ok(Ok(())) => {
                output.volume = v;
                let receiver_id = {
                    let discovery = app.state::<DiscoveryState>();
                    let cache = discovery.cache.lock().unwrap();
                    cache
                        .values()
                        .find(|d| {
                            d.port == output.port
                                && d.addresses.iter().any(|a| a.to_string() == output.ip)
                        })
                        .map(|d| d.receiver_id())
                        .unwrap_or_else(|| format!("manual://{}:{}", output.ip, output.port))
                };
                if let Err(error) = patch_receiver_preferences(
                    app.clone(),
                    receiver_id,
                    ReceiverPreferencesPatch {
                        volume: Some(v),
                        latency_ms: None,
                        reconnect: None,
                    },
                ) {
                    errors.push(error);
                }
            }
            Ok(Err(error)) => errors.push(format!("{}: {error}", output.name)),
            Err(_) => errors.push(format!("{}: volume_timeout", output.name)),
        }
    }
    if errors.is_empty() {
        Ok(v)
    } else {
        Err(errors.join("; "))
    }
}

#[tauri::command]
async fn is_streaming(state: State<'_, StreamingState>) -> Result<Option<StreamingInfo>, String> {
    let slot = state.inner.lock().await;
    Ok(slot.as_ref().and_then(|a| {
        a.outputs
            .values()
            .next()
            .map(|o| o.info(a.sample_rate, a.channels))
    }))
}

#[tauri::command]
async fn list_streaming(state: State<'_, StreamingState>) -> Result<Vec<StreamingInfo>, String> {
    let slot = state.inner.lock().await;
    Ok(slot
        .as_ref()
        .map(|a| {
            a.outputs
                .values()
                .map(|o| o.info(a.sample_rate, a.channels))
                .collect()
        })
        .unwrap_or_default())
}

#[derive(serde::Serialize)]
struct ConnectionInfo {
    ip: String,
    port: u16,
    name: String,
}

/// Hace pair-setup transient + pair-verify contra el HomePod indicado y
/// mantiene la sesión RTSP abierta hasta que se llame a `disconnect_device`.
#[tauri::command]
async fn connect_device(
    app: tauri::AppHandle,
    device_id: Option<String>,
    ip: String,
    port: Option<u16>,
    name: Option<String>,
    state: State<'_, ConnectionState>,
) -> Result<ConnectionInfo, String> {
    let parsed: IpAddr = ip.parse().map_err(|e| format!("invalid IP '{ip}': {e}"))?;
    let port = port.unwrap_or(7000);
    let display_name = name.clone().unwrap_or_else(|| format!("HomePod {parsed}"));

    let descriptor = resolve_descriptor(&app, device_id.as_deref(), parsed, port, &display_name)?;
    // Keep the previous pairing until the new target succeeds.
    let mut token = state.control.begin();
    let mut slot = state.inner.lock().await;
    let session = tokio::select! {
        biased;
        _ = token.cancelled() => return Err("session_cancelled".into()),
        result = within_deadline(async {
            pair_homepod(descriptor).await.map_err(|e| format!("pairing failed: {e}"))
        }) => result?,
    };
    if let Some(mut previous) = slot.replace(session) {
        let _ =
            tokio::time::timeout(Duration::from_secs(3), previous.connection.disconnect()).await;
    }

    Ok(ConnectionInfo {
        ip: parsed.to_string(),
        port,
        name: display_name,
    })
}

/// Suelta la sesión RTSP actual (si hay).
#[tauri::command]
async fn disconnect_device(state: State<'_, ConnectionState>) -> Result<(), String> {
    state.control.cancel();
    let mut slot = state.inner.lock().await;
    if let Some(mut session) = slot.take() {
        let _ = tokio::time::timeout(Duration::from_secs(3), session.connection.disconnect()).await;
    }
    Ok(())
}

#[tauri::command]
async fn is_connected(state: State<'_, ConnectionState>) -> Result<bool, String> {
    let slot = state.inner.lock().await;
    Ok(slot.is_some())
}

/// Añade un dispositivo introducido manualmente por IP. Útil en redes donde
/// mDNS no se propaga (típicamente router Movistar HGU sin reflexión multicast
/// entre 2.4 y 5 GHz). Verifica que hay un AirPlay escuchando antes de emitir.
#[tauri::command]
async fn add_manual_device(
    app: tauri::AppHandle,
    ip: String,
    port: Option<u16>,
    name: Option<String>,
    state: State<'_, DiscoveryState>,
) -> Result<Device, String> {
    let (parsed, port) =
        parse_manual_endpoint(&ip, port).map_err(|e| format!("invalid endpoint '{ip}': {e}"))?;

    let probe = probe_airplay(parsed, port)
        .await
        .map_err(|e| format!("no parece un AirPlay en {parsed}:{port} — {e}"))?;

    let mut device = manual_device(parsed, Some(port), name);
    device.server_header = probe.server_header;

    if let Ok(mut cache) = state.cache.lock() {
        cache.insert(device.id.clone(), device.clone());
    }

    app.emit("airplay://device", &device)
        .map_err(|e| e.to_string())?;

    Ok(device)
}

#[tauri::command]
async fn list_audio_outputs() -> Result<Vec<audio_capture::AudioEndpoint>, String> {
    tokio::task::spawn_blocking(audio_capture::list_audio_outputs)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn get_audio_options(app: tauri::AppHandle) -> Result<AudioOptions, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store
        .get("audio_options")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| e.to_string())
        .map(|v| v.unwrap_or_default())
}
#[tauri::command]
async fn save_audio_options(
    app: tauri::AppHandle,
    options: AudioOptions,
    state: State<'_, StreamingState>,
) -> Result<(), String> {
    let _operation = state.operation.lock().await;
    if state.inner.lock().await.is_some() {
        return Err("stop_before_source_change".into());
    }
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(
        "audio_options",
        serde_json::to_value(options).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())
}
#[tauri::command]
async fn get_session_status(state: State<'_, StreamingState>) -> Result<serde_json::Value, String> {
    let stream = state.inner.lock().await;
    Ok(match stream.as_ref() {
        Some(active) => {
            serde_json::json!({ "generation": active.generation.load(std::sync::atomic::Ordering::SeqCst), "status": "streaming", "mute_status": active._capture.as_ref().map_or("off", |c| c.mute_status()), "group_mode": active.group_mode })
        }
        None => {
            let phase = state.phase.lock().unwrap();
            let status = if phase.1.is_empty() {
                "disconnected"
            } else {
                phase.1
            };
            serde_json::json!({ "generation": state.control.current(), "status": status, "mute_status": "off" })
        }
    })
}
#[tauri::command]
fn get_receiver_preferences(
    app: tauri::AppHandle,
    receiver_id: String,
) -> Result<ReceiverPreferences, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    if let Some(value) = store.get(format!("receiver:{receiver_id}")) {
        return serde_json::from_value(value).map_err(|e| e.to_string());
    }
    // Lazy migration: retain the user's existing global settings for first use.
    Ok(ReceiverPreferences {
        volume: get_volume(app.clone())?.unwrap_or(0.2),
        latency_ms: get_latency(app)?.unwrap_or(3000),
        reconnect: false,
    })
}
#[tauri::command]
fn save_receiver_preferences(
    app: tauri::AppHandle,
    receiver_id: String,
    preferences: ReceiverPreferences,
) -> Result<(), String> {
    preferences.validate()?;
    if receiver_id.len() > 512 {
        return Err("invalid_receiver_id".into());
    }
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(
        format!("receiver:{receiver_id}"),
        serde_json::to_value(preferences).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())
}
#[tauri::command]
fn patch_receiver_preferences(
    app: tauri::AppHandle,
    receiver_id: String,
    patch: ReceiverPreferencesPatch,
) -> Result<(), String> {
    let _guard = RECEIVER_SETTINGS_LOCK.lock().map_err(|e| e.to_string())?;
    let mut preferences = get_receiver_preferences(app.clone(), receiver_id.clone())?;
    patch.apply(&mut preferences);
    save_receiver_preferences(app, receiver_id, preferences)
}

// ── Persistencia (C1) ────────────────────────────────────────────────────────
//
// Usamos `tauri-plugin-store` para volcar a `%APPDATA%/<bundle-id>/settings.json`
// las preferencias del usuario que sobreviven a reinicios de la app:
// - Último HomePod conectado: para reconectar en frío y para el menú del tray.
// - Volumen: para que la sesión nueva arranque al nivel que dejaste.
//
// El plugin maneja serialización JSON, escritura atómica y carga lazy.

#[tauri::command]
fn save_last_device(
    app: tauri::AppHandle,
    ip: String,
    port: u16,
    name: String,
) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    let dev = LastDevice { ip, port, name };
    store.set(
        KEY_LAST_DEVICE,
        serde_json::to_value(&dev).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_last_device(app: tauri::AppHandle) -> Result<Option<LastDevice>, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    let value = store.get(KEY_LAST_DEVICE);
    match value {
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

#[tauri::command]
fn clear_last_device(app: tauri::AppHandle) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.delete(KEY_LAST_DEVICE);
    store.save().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn save_volume(app: tauri::AppHandle, volume: f32) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    let v = volume.clamp(0.0, 1.0);
    store.set(
        KEY_VOLUME,
        serde_json::to_value(v).map_err(|e| e.to_string())?,
    );
    store.save().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_volume(app: tauri::AppHandle) -> Result<Option<f32>, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    let value = store.get(KEY_VOLUME);
    match value {
        Some(v) => serde_json::from_value(v)
            .map(Some)
            .map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

#[tauri::command]
fn save_multi_device(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(KEY_MULTI_DEVICE, serde_json::json!(enabled));
    store.save().map_err(|e| e.to_string())
}

const KEY_GENERAL: &str = "general";

/// Preferences > General.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug)]
#[serde(default)]
struct GeneralSettings {
    /// Reconnect to the last receiver and start playing when AirSend starts.
    auto_play: bool,
    /// Start hidden in the notification area instead of opening the window.
    start_minimized: bool,
    /// Reconnect automatically (up to 3 attempts) when a session fails.
    reconnect_on_error: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            auto_play: true,
            start_minimized: false,
            reconnect_on_error: false,
        }
    }
}

fn load_general_settings(app: &tauri::AppHandle) -> GeneralSettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(KEY_GENERAL))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

#[tauri::command]
fn get_general_settings(app: tauri::AppHandle) -> GeneralSettings {
    load_general_settings(&app)
}

#[tauri::command]
fn save_general_settings(app: tauri::AppHandle, settings: GeneralSettings) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(KEY_GENERAL, serde_json::to_value(settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())
}

/// "Start AirSend when Windows starts": an entry in the user's Run key,
/// managed by tauri-plugin-autostart.
#[tauri::command]
fn get_start_at_login(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_start_at_login(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let launcher = app.autolaunch();
    if enabled { launcher.enable() } else { launcher.disable() }.map_err(|e| e.to_string())
}

#[tauri::command]
fn get_multi_device(app: tauri::AppHandle) -> Result<bool, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    Ok(store
        .get(KEY_MULTI_DEVICE)
        .as_ref()
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

#[tauri::command]
fn get_experimental_local_buffer(app: tauri::AppHandle) -> Result<bool, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    Ok(store
        .get(KEY_LOCAL_BUFFER)
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}

#[tauri::command]
async fn save_experimental_local_buffer(
    app: tauri::AppHandle,
    enabled: bool,
    state: State<'_, StreamingState>,
) -> Result<(), String> {
    let stream = state.inner.lock().await;
    if stream.is_some() {
        return Err("stop_before_buffer_change".into());
    }
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(KEY_LOCAL_BUFFER, serde_json::json!(enabled));
    store.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn export_audio_diagnostics(state: State<'_, DiagnosticState>) -> Result<String, String> {
    serde_json::to_string_pretty(&state.report()).map_err(|e| e.to_string())
}

fn save_latency(app: &tauri::AppHandle, latency_ms: u32) -> Result<(), String> {
    cap_core::streaming::validate_latency_ms(latency_ms).map_err(|e| e.to_string())?;
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(KEY_LATENCY, serde_json::json!(latency_ms));
    store.save().map_err(|e| e.to_string())?;
    Ok(())
}

fn decode_latency(value: &serde_json::Value) -> Option<u32> {
    let latency_ms = match value.as_str() {
        Some("music") => Some(3000),
        Some("video") => Some(2000),
        Some("gaming") => Some(1000),
        Some(_) => None,
        None => value.as_u64().and_then(|n| u32::try_from(n).ok()),
    };
    latency_ms.filter(|ms| cap_core::streaming::validate_latency_ms(*ms).is_ok())
}

#[tauri::command]
fn get_latency(app: tauri::AppHandle) -> Result<Option<u32>, String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    Ok(store.get(KEY_LATENCY).as_ref().and_then(decode_latency))
}

#[tauri::command]
async fn get_latency_cooldown_ms(state: State<'_, StreamingState>) -> Result<u64, String> {
    let changed = state.last_latency_change.lock().await;
    Ok(latency_cooldown_remaining(*changed, Instant::now()).as_millis() as u64)
}

/// Confirm one negotiated buffer change at most every ten seconds. A live
/// stream needs a fresh AirPlay SETUP, so restart it only after confirmation.
#[tauri::command]
async fn confirm_latency(
    app: tauri::AppHandle,
    latency_ms: u32,
    state: State<'_, StreamingState>,
) -> Result<bool, String> {
    cap_core::streaming::validate_latency_ms(latency_ms).map_err(|e| e.to_string())?;
    let mut token = state.control.begin();
    let generation = token.generation;
    let _operation = state.operation.lock().await;
    if generation != state.control.current() {
        return Err("session_cancelled".into());
    }
    let mut changed = state.last_latency_change.lock().await;
    let remaining = latency_cooldown_remaining(*changed, Instant::now());
    if !remaining.is_zero() {
        monitor_active(&app, &state, generation).await;
        return Err(format!("latency_cooldown:{}", remaining.as_millis()));
    }

    let previous = state
        .inner
        .lock()
        .await
        .as_ref()
        .map(|s| s.latency_ms)
        .unwrap_or(get_latency(app.clone())?.unwrap_or(cap_core::streaming::DEFAULT_LATENCY_MS));
    if previous == latency_ms {
        monitor_active(&app, &state, generation).await;
        return Ok(false);
    }

    let mut slot = state.inner.lock().await;
    let restarted = if let Some(mut stream) = slot.take() {
        let targets = stream
            .target_order
            .iter()
            .filter_map(|key| stream.outputs.get(key))
            .map(|output| {
                (
                    output.ip.clone(),
                    output.port,
                    output.name.clone(),
                    output.volume,
                )
            })
            .collect::<Vec<_>>();
        let old_latency_ms = stream.latency_ms;
        let group_mode = stream.group_mode;
        if let Err(error) = save_latency(&app, latency_ms) {
            *slot = Some(stream);
            return Err(error);
        }
        // A receiver may reject a second RTSP session to itself. Close the old
        // sessions before reopening them, then restore the old setup on error.
        stream.shutdown();
        stream.stop_outputs().await;
        drop(stream);
        match tokio::select! { biased; _ = token.cancelled() => Err("session_cancelled".into()), result = within_deadline(prepare_targets(app.clone(), &targets, latency_ms, generation, group_mode)) => result }
        {
            Ok(replacement) => *slot = Some(replacement),
            Err(error) => {
                let _ = save_latency(&app, previous);
                if token.generation != state.control.current() {
                    return Err(error);
                }
                let restored = tokio::select! { biased; _ = token.cancelled() => return Err("session_cancelled".into()), result = within_deadline(prepare_targets(
                    app.clone(),
                    &targets,
                    old_latency_ms,
                    generation,
                    group_mode,
                )) => result };
                if let Ok(restored) = restored {
                    *slot = Some(restored);
                }
                drop(slot);
                monitor_active(&app, &state, generation).await;
                return Err(error);
            }
        }
        true
    } else {
        save_latency(&app, latency_ms)?;
        false
    };

    drop(slot);
    monitor_active(&app, &state, generation).await;
    *changed = Some(Instant::now());
    Ok(restarted)
}

// ── System tray (C3) ─────────────────────────────────────────────────────────
//
// La app vive en bandeja del sistema. Cerrar la ventana la oculta pero el
// proceso sigue (streaming continúa). Sólo "Salir" del menú o un kill mata el
// daemon. Esto es lo esperado para una app de tipo "siempre disponible".

fn setup_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let show_item = MenuItemBuilder::with_id("show", "Show / hide window").build(app)?;
    let quit_item = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .item(&show_item)
        .separator()
        .item(&quit_item)
        .build()?;

    let mut builder = TrayIconBuilder::with_id("main")
        .tooltip("AirSend")
        .menu(&menu)
        // Left click is reserved for the compact window; the menu is right-click only.
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => toggle_main_window(app, tray_icon_rect(app)),
            "quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn(quit_app(app));
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left click opens the compact window just above the icon, like
            // TuneBlade.
            if let TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                toggle_main_window(tray.app_handle(), Some(rect));
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;

    Ok(())
}

const MAIN_LABEL: &str = "main";
const MAIN_WIDTH: f64 = 380.0;
const MAIN_MIN_HEIGHT: f64 = 96.0;

/// Where the compact window is anchored, in physical pixels: horizontally
/// centred on the tray icon and glued to the taskbar edge of the work area.
#[derive(Clone, Copy)]
struct MainAnchor {
    center_x: f64,
    from_bottom: bool,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
    scale: f64,
}

static MAIN_ANCHOR: std::sync::Mutex<Option<MainAnchor>> = std::sync::Mutex::new(None);
/// Content height requested by the UI (logical pixels).
static MAIN_HEIGHT: std::sync::Mutex<f64> = std::sync::Mutex::new(160.0);
/// When the window was last hidden by losing focus. Clicking the tray icon
/// while the window is open first steals its focus (hiding it), then delivers
/// the click: that click must not reopen it.
static MAIN_HIDDEN_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
/// When the window was last shown: focus changes right after showing it are
/// part of the activation and must not hide it again.
static MAIN_SHOWN_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
/// The startup window is shown once the UI has measured its content.
static MAIN_STARTUP_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tray_icon_rect(app: &tauri::AppHandle) -> Option<tauri::Rect> {
    app.tray_by_id("main").and_then(|tray| tray.rect().ok().flatten())
}

fn main_anchor(app: &tauri::AppHandle, icon: Option<tauri::Rect>) -> Option<MainAnchor> {
    let (monitor, icon_point) = match icon {
        Some(icon) => {
            let pos = icon.position.to_physical::<f64>(1.0);
            let size = icon.size.to_physical::<f64>(1.0);
            let point = (pos.x + size.width / 2.0, pos.y);
            (app.monitor_from_point(point.0, point.1).ok().flatten(), Some(point))
        }
        None => (app.primary_monitor().ok().flatten(), None),
    };
    let monitor = monitor?;
    let area = monitor.work_area();
    let (left, top) = (area.position.x as f64, area.position.y as f64);
    let (right, bottom) = (left + area.size.width as f64, top + area.size.height as f64);
    let scale = monitor.scale_factor();
    // Without an icon position, fall back to the usual tray corner.
    let (center_x, icon_y) = icon_point.unwrap_or((right - MAIN_WIDTH * scale / 2.0, bottom));
    Some(MainAnchor {
        center_x,
        from_bottom: icon_y >= (top + bottom) / 2.0,
        left,
        top,
        right,
        bottom,
        scale,
    })
}

/// Sizes the window to the requested content height and keeps it glued to
/// the taskbar edge, never larger than the work area.
fn apply_main_geometry(win: &tauri::WebviewWindow, anchor: MainAnchor) {
    let margin = 8.0 * anchor.scale;
    let width = MAIN_WIDTH * anchor.scale;
    let wanted = MAIN_HEIGHT.lock().unwrap().max(MAIN_MIN_HEIGHT) * anchor.scale;
    let height = wanted.min(anchor.bottom - anchor.top - 2.0 * margin);
    let x = (anchor.center_x - width / 2.0)
        .min(anchor.right - width - margin)
        .max(anchor.left + margin);
    let y = if anchor.from_bottom {
        anchor.bottom - height - margin
    } else {
        anchor.top + margin
    };
    let _ = win.set_size(tauri::PhysicalSize::new(width, height));
    let _ = win.set_position(tauri::PhysicalPosition::new(x, y));
    tracing::info!(
        requested_height = wanted,
        applied_height = height,
        actual = ?win.inner_size().ok(),
        scale = anchor.scale,
        "compact window geometry"
    );
}

fn show_main_window(app: &tauri::AppHandle, icon: Option<tauri::Rect>) {
    let Some(win) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    let anchor = main_anchor(app, icon);
    *MAIN_ANCHOR.lock().unwrap() = anchor;
    if let Some(anchor) = anchor {
        apply_main_geometry(&win, anchor);
    }
    *MAIN_SHOWN_AT.lock().unwrap() = Some(Instant::now());
    let _ = win.show();
    let _ = win.set_focus();
}

fn hide_main(app: &tauri::AppHandle) {
    if MAIN_SHOWN_AT
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < Duration::from_millis(500))
    {
        return;
    }
    if let Some(win) = app.get_webview_window(MAIN_LABEL) {
        if win.is_visible().unwrap_or(false) {
            let _ = win.hide();
            *MAIN_HIDDEN_AT.lock().unwrap() = Some(Instant::now());
        }
    }
}

fn toggle_main_window(app: &tauri::AppHandle, icon: Option<tauri::Rect>) {
    let Some(win) = app.get_webview_window(MAIN_LABEL) else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        hide_main(app);
        return;
    }
    if MAIN_HIDDEN_AT
        .lock()
        .unwrap()
        .is_some_and(|at| at.elapsed() < Duration::from_millis(300))
    {
        return;
    }
    show_main_window(app, icon);
}

/// Called by the UI whenever its content height changes.
#[tauri::command]
fn fit_main_window(app: tauri::AppHandle, height: f64) {
    if !height.is_finite() {
        return;
    }
    *MAIN_HEIGHT.lock().unwrap() = height;
    if MAIN_STARTUP_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) {
        show_main_window(&app, tray_icon_rect(&app));
        return;
    }
    let anchor = *MAIN_ANCHOR.lock().unwrap();
    if let (Some(win), Some(anchor)) = (app.get_webview_window(MAIN_LABEL), anchor) {
        apply_main_geometry(&win, anchor);
    }
}

#[tauri::command]
fn hide_main_window(app: tauri::AppHandle) {
    hide_main(&app);
}

/// Opens the Preferences window (created on first use, focused afterwards).
/// Async on purpose: creating a window from a synchronous command can
/// deadlock on Windows.
#[tauri::command]
async fn open_preferences(app: tauri::AppHandle) -> Result<(), String> {
    hide_main(&app);
    if let Some(win) = app.get_webview_window("prefs") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(&app, "prefs", tauri::WebviewUrl::App("prefs.html".into()))
        .title("AirSend Preferences")
        .inner_size(640.0, 460.0)
        .min_inner_size(520.0, 360.0)
        .center()
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Stops streaming, releases the receivers and exits (tray "Quit" and the
/// power button of the compact window).
#[tauri::command]
async fn quit_app(app: tauri::AppHandle) {
    let _ = stop_streaming(app.clone(), app.state::<StreamingState>()).await;
    let _ = disconnect_device(app.state::<ConnectionState>()).await;
    app.exit(0);
}

// ─────────────────────────────────────────────────────────────────────────────

/// Localización de los logs por plataforma. Windows usa %APPDATA% (igual que
/// hace Tauri por defecto para `app_log_dir` con el bundle id de la app).
/// Si no se puede resolver, devolvemos None y los logs quedan solo en stdout.
fn resolve_log_dir() -> Option<std::path::PathBuf> {
    const APP_DIRNAME: &str = "ConexionAirPlay";
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")?;
        Some(
            std::path::PathBuf::from(base)
                .join(APP_DIRNAME)
                .join("logs"),
        )
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            std::path::PathBuf::from(home)
                .join("Library")
                .join("Logs")
                .join(APP_DIRNAME),
        )
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
            return Some(std::path::PathBuf::from(xdg).join(APP_DIRNAME).join("logs"));
        }
        let home = std::env::var_os("HOME")?;
        Some(
            std::path::PathBuf::from(home)
                .join(".local")
                .join("state")
                .join(APP_DIRNAME)
                .join("logs"),
        )
    }
}

/// Configura `tracing`: stdout siempre + archivo rotado por día si el directorio
/// de logs es resoluble y escribible. Devuelve el `WorkerGuard` que mantiene
/// vivo el writer non-blocking (se debe retener todo el lifetime del programa;
/// si se dropea, los logs pendientes se pierden).
fn init_tracing() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_ansi(true);

    let (file_layer, guard) = match resolve_log_dir() {
        Some(dir) => match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                let appender = tracing_appender::rolling::daily(&dir, "app.log");
                let (writer, guard) = tracing_appender::non_blocking(appender);
                let layer = tracing_subscriber::fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .with_target(true);
                eprintln!("→ logs a {}", dir.display());
                (Some(layer), Some(guard))
            }
            Err(e) => {
                eprintln!("→ no pude crear dir de logs {}: {e}", dir.display());
                (None, None)
            }
        },
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(stdout_layer)
        .with(file_layer.map(|l| l.boxed()))
        .init();

    guard
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // `_log_guard` debe vivir todo el programa para que el writer non-blocking
    // siga drenando logs al archivo. Lo "olvidamos" con un binding mut a static.
    let _log_guard = init_tracing();

    tauri::Builder::default()
        // Must be the first plugin: a second launch exits immediately and the
        // running instance shows its window instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app, tray_icon_rect(app));
        }))
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(DiscoveryState::default())
        .manage(ConnectionState::default())
        .manage(StreamingState::default())
        .manage(DiagnosticState::default())
        .invoke_handler(tauri::generate_handler![
            fit_main_window,
            hide_main_window,
            open_preferences,
            quit_app,
            get_general_settings,
            save_general_settings,
            get_start_at_login,
            set_start_at_login,
            discover_devices,
            start_discovery_stream,
            stop_discovery_stream,
            add_manual_device,
            connect_device,
            disconnect_device,
            is_connected,
            start_streaming,
            start_group_streaming,
            get_receiver_volumes,
            set_receiver_volume,
            add_streaming,
            remove_streaming,
            stop_streaming,
            set_stream_volume,
            is_streaming,
            list_streaming,
            save_last_device,
            get_last_device,
            clear_last_device,
            save_volume,
            get_volume,
            save_multi_device,
            get_multi_device,
            get_latency,
            get_latency_cooldown_ms,
            confirm_latency,
            get_experimental_local_buffer,
            save_experimental_local_buffer,
            export_audio_diagnostics,
            list_audio_outputs,
            get_audio_options,
            save_audio_options,
            get_session_status,
            get_receiver_preferences,
            save_receiver_preferences,
            patch_receiver_preferences,
        ])
        .setup(|app| {
            let journal = app.path().app_data_dir()?.join("local-mute-recovery.json");
            // Recovery runs before the window can start another mute session.
            match std::thread::spawn(move || audio_capture::recover_local_mute(&journal)).join() {
                Ok(Ok(())) => {}
                result => tracing::warn!(?result, "local mute recovery remains pending"),
            }
            setup_tray(app.handle())?;

            let power_app = app.handle().clone();
            power::watch(move |event| handle_power_event(&power_app, event));

            // The compact window behaves like a tray popup: it hides when it
            // loses focus or is closed (Alt+F4); the process stays in the tray
            // until "Quit".
            if let Some(win) = app.get_webview_window(MAIN_LABEL) {
                let app_handle = app.handle().clone();
                win.on_window_event(move |event| match event {
                    WindowEvent::CloseRequested { api, .. } => {
                        api.prevent_close();
                        hide_main(&app_handle);
                    }
                    WindowEvent::Focused(false) => hide_main(&app_handle),
                    _ => {}
                });
            }
            // Open next to the tray icon on startup, like TuneBlade, as soon as
            // the UI has measured its content (fit_main_window); show it anyway
            // after a few seconds if the UI never reports. "Start minimized"
            // keeps it in the notification area.
            if !load_general_settings(app.handle()).start_minimized {
                MAIN_STARTUP_PENDING.store(true, std::sync::atomic::Ordering::SeqCst);
                let startup_app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    if MAIN_STARTUP_PENDING.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        show_main_window(&startup_app, tray_icon_rect(&startup_app));
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");

    // `_log_guard` se dropea aquí al salir, drenando los últimos logs.
    drop(_log_guard);
}

fn emit_session(app: &tauri::AppHandle, generation: u64, code: &'static str) {
    let state = app.state::<StreamingState>();
    let mut phase = state.phase.lock().unwrap();
    if generation < phase.0 {
        return;
    }
    *phase = (generation, code);
    let _ = app.emit(
        "airplay://session",
        SessionEvent {
            generation,
            code: code.into(),
        },
    );
}

async fn monitor_active(app: &tauri::AppHandle, state: &StreamingState, generation: u64) {
    let slot = state.inner.lock().await;
    if state.control.current() != generation {
        return;
    }
    if let Some(active) = slot.as_ref() {
        active
            .generation
            .store(generation, std::sync::atomic::Ordering::SeqCst);
        if active
            ._capture
            .as_ref()
            .is_some_and(|c| c.mute_status() == "off")
        {
            if let Err(error) = active.apply_mute(app) {
                tracing::warn!(%error, "local mute unavailable");
            }
        }
        start_session_monitor(app.clone(), generation);
        emit_session(app, generation, "streaming");
    } else {
        emit_session(app, generation, "disconnected");
    }
}

fn start_session_monitor(app: tauri::AppHandle, generation: u64) {
    tauri::async_runtime::spawn(async move {
        let mut last_send_errors = 0;
        let mut failed_sends = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let state = app.state::<StreamingState>();
            let reason = {
                let slot = state.inner.lock().await;
                let Some(active) = slot.as_ref().filter(|s| {
                    s.generation.load(std::sync::atomic::Ordering::SeqCst) == generation
                }) else {
                    return;
                };
                let sends = active.diagnostics.lock().unwrap().send_errors();
                if sends > last_send_errors {
                    failed_sends += 1;
                } else {
                    failed_sends = 0;
                }
                last_send_errors = sends;
                if active.stop.load(std::sync::atomic::Ordering::SeqCst) {
                    Some("capture_interrupted")
                } else if active.outputs.values().any(|o| o.metrics.unhealthy()) {
                    Some("feedback_failed")
                } else if failed_sends >= 3 {
                    Some("transport_failed")
                } else {
                    None
                }
            };
            let Some(reason) = reason else {
                continue;
            };
            let mut failed = {
                let mut slot = state.inner.lock().await;
                if slot.as_ref().is_none_or(|s| {
                    s.generation.load(std::sync::atomic::Ordering::SeqCst) != generation
                }) {
                    return;
                }
                slot.take().unwrap()
            };
            let targets = failed
                .target_order
                .iter()
                .filter_map(|key| failed.outputs.get(key))
                .map(|o| (o.ip.clone(), o.port, o.name.clone(), o.volume))
                .collect::<Vec<_>>();
            let latency_ms = failed.latency_ms;
            let group_mode = failed.group_mode;
            failed.diagnostics.lock().unwrap().record_failure(reason);
            failed.shutdown();
            failed.stop_outputs().await;
            drop(failed);
            // A user-requested replacement/Stop always takes priority over automatic recovery.
            if state.control.current() != generation {
                return;
            }
            // Global setting (Preferences > General), or the legacy per-receiver flag.
            let reconnect = load_general_settings(&app).reconnect_on_error
                || targets.iter().any(|(ip, port, _, _)| {
                let discovery = app.state::<DiscoveryState>();
                let cache = discovery.cache.lock().unwrap();
                let id = cache
                    .values()
                    .find(|d| d.port == *port && d.addresses.iter().any(|a| a.to_string() == *ip))
                    .map(|d| d.receiver_id())
                    .unwrap_or_else(|| format!("manual://{ip}:{port}"));
                let pair_reconnect = group_mode
                    && cache
                        .values()
                        .find(|d| {
                            d.port == *port && d.addresses.iter().any(|a| a.to_string() == *ip)
                        })
                        .and_then(|d| d.tight_sync_id.as_ref())
                        .is_some_and(|id| {
                            get_receiver_preferences(app.clone(), format!("stereo:{id}"))
                                .is_ok_and(|p| p.reconnect)
                        });
                pair_reconnect
                    || get_receiver_preferences(app.clone(), id).is_ok_and(|p| p.reconnect)
            });
            if reconnect {
                let mut token = state.control.subscribe(generation);
                emit_session(&app, generation, "recovering");
                for delay in [1, 2, 4] {
                    let restored = tokio::select! {
                        biased;
                        _ = token.cancelled() => return,
                        result = async {
                            tokio::time::sleep(Duration::from_secs(delay)).await;
                            within_deadline(prepare_targets(app.clone(), &targets, latency_ms, generation, group_mode)).await
                        } => result,
                    };
                    match restored {
                        Ok(mut stream) => {
                            let mut slot = state.inner.lock().await;
                            if state.control.current() != generation {
                                stream.shutdown();
                                return;
                            }
                            let _ = stream.apply_mute(&app);
                            *slot = Some(stream);
                            emit_session(&app, generation, "streaming");
                            last_send_errors = 0;
                            failed_sends = 0;
                            break;
                        }
                        Err(error) => {
                            // Authentication/configuration failures require user action.
                            if !session::recoverable_failure(&error) {
                                break;
                            }
                        }
                    }
                }
                if state.inner.lock().await.is_some() {
                    continue;
                }
            }
            let _ = app.emit(
                "airplay://error",
                SessionEvent {
                    generation,
                    code: reason.into(),
                },
            );
            // Sin esto la fase se quedaba en "streaming" con la sesión ya destruida.
            emit_session(&app, generation, "disconnected");
            return;
        }
    });
}

#[derive(serde::Serialize)]
struct ReceiverVolume {
    ip: String,
    port: u16,
    volume: f32,
    readable: bool,
}
#[tauri::command]
async fn get_receiver_volumes(
    state: State<'_, StreamingState>,
) -> Result<Vec<ReceiverVolume>, String> {
    let mut slot = state.inner.lock().await;
    let Some(active) = slot.as_mut() else {
        return Ok(Vec::new());
    };
    let mut values = Vec::new();
    for output in active.outputs.values_mut() {
        let Ok(mut connection) = output.connection.try_lock() else {
            values.push(ReceiverVolume {
                ip: output.ip.clone(),
                port: output.port,
                volume: output.volume,
                readable: output.volume_read_supported.unwrap_or(false),
            });
            continue;
        };
        let volume = if output.volume_read_supported == Some(false) {
            None
        } else {
            match tokio::time::timeout(Duration::from_secs(2), connection.read_volume()).await {
                Ok(Ok(Some(v))) => {
                    output.volume_read_supported = Some(true);
                    Some(v)
                }
                _ => {
                    output.volume_read_supported = Some(false);
                    None
                }
            }
        };
        if let Some(volume) = volume {
            output.volume = volume;
        }
        values.push(ReceiverVolume {
            ip: output.ip.clone(),
            port: output.port,
            volume: output.volume,
            readable: volume.is_some(),
        });
    }
    Ok(values)
}
#[tauri::command]
async fn set_receiver_volume(
    app: tauri::AppHandle,
    ip: String,
    port: u16,
    volume: f32,
    state: State<'_, StreamingState>,
) -> Result<f32, String> {
    if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
        return Err("invalid_volume".into());
    }
    let mut slot = state.inner.lock().await;
    let output = slot
        .as_mut()
        .and_then(|s| s.outputs.get_mut(&output_key(&ip, port)))
        .ok_or("receiver_not_streaming")?;
    tokio::time::timeout(
        Duration::from_secs(3),
        output.connection.lock().await.set_volume(volume),
    )
    .await
    .map_err(|_| "volume_timeout")?
    .map_err(|e| e.to_string())?;
    output.volume = volume;
    let receiver_id = {
        let discovery = app.state::<DiscoveryState>();
        let cache = discovery.cache.lock().unwrap();
        cache
            .values()
            .find(|d| d.port == port && d.addresses.iter().any(|a| a.to_string() == ip))
            .map(|d| d.receiver_id())
            .unwrap_or_else(|| format!("manual://{ip}:{port}"))
    };
    patch_receiver_preferences(
        app,
        receiver_id,
        ReceiverPreferencesPatch {
            volume: Some(volume),
            latency_ms: None,
            reconnect: None,
        },
    )?;
    Ok(volume)
}
