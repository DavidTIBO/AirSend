//! Avisos de suspensión/reanudación de Windows. Al dormir el PC mueren en
//! silencio todos los sockets (RTSP, RTP, mDNS): hay que cerrar la sesión
//! antes de dormir y volver a descubrir receptores al despertar.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    Suspend,
    Resume,
}

/// Registra `handler` para toda la vida del proceso. Windows lo invoca desde
/// un hilo propio; en `Suspend` el sistema concede ~2 s antes de dormir.
#[cfg(windows)]
pub fn watch(handler: impl Fn(PowerEvent) + Send + Sync + 'static) -> bool {
    use std::ffi::c_void;
    const DEVICE_NOTIFY_CALLBACK: u32 = 2;
    const PBT_APMSUSPEND: u32 = 0x4;
    // Llega en toda reanudación (con o sin usuario); PBT_APMRESUMESUSPEND la
    // duplicaría.
    const PBT_APMRESUMEAUTOMATIC: u32 = 0x12;

    type Handler = Box<dyn Fn(PowerEvent) + Send + Sync>;
    type Callback = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> u32;
    #[repr(C)]
    struct SubscribeParameters {
        callback: Callback,
        context: *mut c_void,
    }
    #[link(name = "powrprof")]
    extern "system" {
        fn PowerRegisterSuspendResumeNotification(
            flags: u32,
            recipient: *mut c_void,
            registration: *mut *mut c_void,
        ) -> u32;
    }
    unsafe extern "system" fn callback(context: *mut c_void, kind: u32, _: *mut c_void) -> u32 {
        let handler = &*(context as *const Handler);
        match kind {
            PBT_APMSUSPEND => handler(PowerEvent::Suspend),
            PBT_APMRESUMEAUTOMATIC => handler(PowerEvent::Resume),
            _ => {}
        }
        0
    }

    // El registro nunca se anula: handler y parámetros deben vivir para siempre.
    let handler: &'static Handler = Box::leak(Box::new(Box::new(handler) as Handler));
    let parameters = Box::leak(Box::new(SubscribeParameters {
        callback,
        context: handler as *const Handler as *mut c_void,
    }));
    let mut registration = std::ptr::null_mut();
    let status = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            parameters as *mut SubscribeParameters as *mut c_void,
            &mut registration,
        )
    };
    if status != 0 {
        tracing::warn!(status, "no se pudo suscribir a suspensión/reanudación");
        return false;
    }
    tracing::info!("suscrito a avisos de suspensión/reanudación de Windows");
    true
}

#[cfg(not(windows))]
pub fn watch(_handler: impl Fn(PowerEvent) + Send + Sync + 'static) -> bool {
    false
}
