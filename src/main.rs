#![windows_subsystem = "windows"]

use std::sync::{atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering}, Arc, RwLock};
use std::thread;
use std::fs::OpenOptions;
use std::io::Write;

static VERBOSE_LOGGING: AtomicBool = AtomicBool::new(false);

fn log(msg: &str) {
    if !VERBOSE_LOGGING.load(Ordering::Relaxed) {
        return;
    }
    let path = match std::env::current_exe() {
        Ok(mut p) => {
            p.set_file_name("VolumeProxy.log");
            p
        }
        Err(_) => std::env::temp_dir().join("VolumeProxy.log"),
    };
        
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        let _ = writeln!(file, "[{}] {}", now, msg);
    }
}
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        Media::Audio::*,
        System::Com::*,
        System::Threading::*,
        UI::Accessibility::*,
        UI::WindowsAndMessaging::*,
    },
};

const OWN_CTX: GUID = GUID {
    data1: 0x7A3B_5C1D,
    data2: 0xE2F4,
    data3: 0x4A6B,
    data4:[0x8C, 0x9D, 0xAE, 0xBF, 0xC0, 0xD1, 0xE2, 0xF3],
};

const SESSION_GUID: GUID = GUID {
    data1: 0xA1B2_C3D4,
    data2: 0xE5F6,
    data3: 0x7890,
    data4:[0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89],
};

struct ForegroundHookContext {
    own_vol: ISimpleAudioVolume,
    device: IMMDevice,
    prev_vol: Arc<AtomicU32>,
}

unsafe impl Send for ForegroundHookContext {}
unsafe impl Sync for ForegroundHookContext {}

static FOREGROUND_HOOK_CTX: RwLock<Option<Arc<ForegroundHookContext>>> = RwLock::new(None);

struct SessionEventTracker {
    session: IAudioSessionControl,
    events: IAudioSessionEvents,
}

unsafe impl Send for SessionEventTracker {}
unsafe impl Sync for SessionEventTracker {}

impl Drop for SessionEventTracker {
    fn drop(&mut self) {
        unsafe {
            let _ = self.session.UnregisterAudioSessionNotification(&self.events);
        }
    }
}

static ALL_SESSIONS: RwLock<Option<Vec<SessionEventTracker>>> = RwLock::new(None);

struct SessionManagerNotificationTracker {
    mgr: IAudioSessionManager2,
    notification: IAudioSessionNotification,
}

impl Drop for SessionManagerNotificationTracker {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mgr.UnregisterSessionNotification(&self.notification);
        }
    }
}

#[implement(IAudioSessionNotification)]
struct SessionManagerEvents;

impl IAudioSessionNotification_Impl for SessionManagerEvents_Impl {
    fn OnSessionCreated(&self, new_session: Option<&IAudioSessionControl>) -> Result<()> {
        let Some(session) = new_session else { return Ok(()) };
        let Ok(ctrl2) = session.cast::<IAudioSessionControl2>() else { return Ok(()) };

        let events: IAudioSessionEvents = AppSessionEvents { session: ctrl2 }.into();

        if unsafe { session.RegisterAudioSessionNotification(&events) }.is_ok() {
            if let Ok(mut lock) = ALL_SESSIONS.write() {
                if let Some(trackers) = lock.as_mut() {
                    trackers.retain(|t| {
                        unsafe { t.session.GetState().unwrap_or(AudioSessionStateExpired) != AudioSessionStateExpired }
                    });
                    trackers.push(SessionEventTracker {
                        session: session.clone(),
                        events,
                    });
                }
            }
        }
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct AppSessionEvents {
    session: IAudioSessionControl2,
}

impl IAudioSessionEvents_Impl for AppSessionEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> { Ok(()) }

    fn OnSimpleVolumeChanged(
        &self,
        new_volume: f32,
        _new_mute: BOOL,
        event_context: *const GUID,
    ) -> Result<()> {
        if !event_context.is_null() && unsafe { *event_context } == OWN_CTX {
            return Ok(());
        }

        let pid = unsafe { self.session.GetProcessId().unwrap_or(0) };
        if pid == 0 || pid != foreground_pid() {
            return Ok(());
        }

        let ctx = {
            let guard = match FOREGROUND_HOOK_CTX.read() {
                Ok(g) => g,
                Err(_) => return Ok(()),
            };
            match guard.as_ref() {
                Some(c) => c.clone(),
                None => return Ok(()),
            }
        };

        let prev = f32::from_bits(ctx.prev_vol.load(Ordering::Relaxed));
        if (new_volume - prev).abs() >= 0.001 {
            if unsafe { ctx.own_vol.SetMasterVolume(new_volume, &OWN_CTX) }.is_ok() {
                ctx.prev_vol.store(new_volume.to_bits(), Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> { Ok(()) }
    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> Result<()> { Ok(()) }
}

static REINIT_NEEDED: AtomicBool = AtomicBool::new(false);
static WAKE_HANDLE_PTR: AtomicUsize = AtomicUsize::new(0);

// Aggiunto per evitare Leak di risorse di sistema chiudendo correttamente WAKE_HANDLE
struct EventHandleGuard(HANDLE);
impl Drop for EventHandleGuard {
    fn drop(&mut self) {
        WAKE_HANDLE_PTR.store(0, Ordering::Release);
        unsafe {
            if !self.0.0.is_null() {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[implement(IAudioSessionEvents)]
struct Proxy {
    own_vol: ISimpleAudioVolume,
    device: IMMDevice,
    prev_vol: Arc<AtomicU32>,
}

impl IAudioSessionEvents_Impl for Proxy_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> { Ok(()) }

    fn OnSimpleVolumeChanged(
        &self,
        new_volume: f32,
        _new_mute: BOOL,
        event_context: *const GUID,
    ) -> Result<()> {
        if !event_context.is_null() && unsafe { *event_context } == OWN_CTX {
            return Ok(());
        }

        let prev = f32::from_bits(self.prev_vol.load(Ordering::Relaxed));
        let delta = new_volume - prev;
        if delta.abs() < 0.001 {
            return Ok(());
        }

        let actual = apply_delta_to_foreground(&self.device, delta).unwrap_or(new_volume);
        let _ = unsafe { self.own_vol.SetMasterVolume(actual, &OWN_CTX) };
        self.prev_vol.store(actual.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> { Ok(()) }
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> { Ok(()) }

    fn OnSessionDisconnected(&self, _reason: AudioSessionDisconnectReason) -> Result<()> {
        log(&format!("Proxy OnSessionDisconnected triggered! Reason: {:?}", _reason.0));
        REINIT_NEEDED.store(true, Ordering::Release);
        let handle_val = WAKE_HANDLE_PTR.load(Ordering::Acquire);
        if handle_val != 0 {
            unsafe {
                let _ = SetEvent(HANDLE(handle_val as *mut _));
            }
        }
        Ok(())
    }
}

fn get_volume_for_pid(device: &IMMDevice, pid: u32) -> Option<f32> {
    let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None).ok()? };
    let list = unsafe { mgr.GetSessionEnumerator().ok()? };
    let n = unsafe { list.GetCount().ok()? };
    for i in 0..n {
        let Ok(ctrl) = (unsafe { list.GetSession(i) }) else { continue };
        let Ok(ctrl2) = ctrl.cast::<IAudioSessionControl2>() else { continue };
        if unsafe { ctrl2.GetProcessId().unwrap_or(0) } != pid { continue; }
        let Ok(vol) = ctrl.cast::<ISimpleAudioVolume>() else { continue };
        if let Ok(cur) = unsafe { vol.GetMasterVolume() } { return Some(cur); }
    }
    None
}

fn get_foreground_volume(device: &IMMDevice) -> Option<f32> {
    let pid = foreground_pid();
    if pid == 0 { return None; }
    get_volume_for_pid(device, pid)
}

fn foreground_pid() -> u32 {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() { return 0; }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK, event: u32, hwnd: HWND,
    _idobject: i32, _idchild: i32, _dw_event_thread: u32, _dwms_event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND || hwnd.0.is_null() { return; }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 { return; }
    log(&format!("Foreground window changed. PID: {}", pid));

    let ctx = {
        let guard = match FOREGROUND_HOOK_CTX.read() {
            Ok(g) => g,
            Err(_) => return,
        };
        match guard.as_ref() {
            Some(c) => c.clone(),
            None => return,
        }
    };

    if let Some(volume) = get_volume_for_pid(&ctx.device, pid) {
        let prev = f32::from_bits(ctx.prev_vol.load(Ordering::Relaxed));
        if (volume - prev).abs() >= 0.001 {
            if unsafe { ctx.own_vol.SetMasterVolume(volume, &OWN_CTX) }.is_ok() {
                ctx.prev_vol.store(volume.to_bits(), Ordering::Relaxed);
            }
        }
    }
}

fn apply_delta_to_foreground(device: &IMMDevice, delta: f32) -> Option<f32> {
    let pid = foreground_pid();
    if pid == 0 { return None; }
    let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None).ok()? };
    let list = unsafe { mgr.GetSessionEnumerator().ok()? };
    let n = unsafe { list.GetCount().ok()? };
    let mut result = None;
    for i in 0..n {
        let Ok(ctrl) = (unsafe { list.GetSession(i) }) else { continue };
        let Ok(ctrl2) = ctrl.cast::<IAudioSessionControl2>() else { continue };
        if unsafe { ctrl2.GetProcessId().unwrap_or(0) } != pid { continue; }
        let Ok(vol) = ctrl.cast::<ISimpleAudioVolume>() else { continue };
        let Ok(cur) = (unsafe { vol.GetMasterVolume() }) else { continue };
        let next = (cur + delta).clamp(0.0, 1.0);
        if unsafe { vol.SetMasterVolume(next, std::ptr::null()) }.is_ok() {
            log(&format!("Applied volume change (delta: {}) to PID {}", delta, pid));
            result = Some(next);
        }
    }
    result
}

// Rimosso il passaggio di `denum` tramite argomenti per permetterne il riavvio autonomo
fn run_session() -> Result<()> {
    log("--- run_session started ---");
    REINIT_NEEDED.store(false, Ordering::Release);
    if let Ok(mut lock) = ALL_SESSIONS.write() {
        *lock = None;
    }
    if let Ok(mut lock) = FOREGROUND_HOOK_CTX.write() {
        *lock = None;
    }

    // [FIX 1] Spostato CoCreateInstance ALL'INTERNO della funzione. 
    // Se fallisce all'avvio del sistema, il programma catturerà l'errore e riproverà.
    let denum: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice = unsafe { denum.GetDefaultAudioEndpoint(eRender, eConsole)? };
    log("Got default audio endpoint.");
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

    let fmt = unsafe { client.GetMixFormat()? };
    let block_align = unsafe { (*fmt).nBlockAlign as usize };
    let mut period = 0i64;
    unsafe { client.GetDevicePeriod(Some(&mut period), None)? };

    loop {
        let result = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period, 0, fmt, Some(&SESSION_GUID),
            )
        };
        match result {
            Ok(()) => break,
            Err(e) if e.code() == AUDCLNT_E_DEVICE_IN_USE => {
                log("AUDCLNT_E_DEVICE_IN_USE, waiting...");
                thread::sleep(std::time::Duration::from_secs(2));
                if REINIT_NEEDED.load(Ordering::Acquire) { return Ok(()); }
            }
            Err(e) => {
                log(&format!("client.Initialize failed: {:?}", e));
                return Err(e);
            }
        }
    }
    unsafe { CoTaskMemFree(Some(fmt as *const core::ffi::c_void)) };

    let wake = unsafe { CreateEventW(None, false, false, None)? };
    WAKE_HANDLE_PTR.store(wake.0 as usize, Ordering::Release);
    
    // [FIX 3] Aggiunto EventHandleGuard per non perdere WAKE_HANDLE ad ogni riconnessione.
    let _wake_guard = EventHandleGuard(wake);

    let vol: ISimpleAudioVolume = unsafe { client.GetService()? };
    let ctrl: IAudioSessionControl = unsafe { client.GetService()? };
    unsafe { client.SetEventHandle(wake)? };

    let initial = get_foreground_volume(&device).unwrap_or(0.5);
    log(&format!("Initial proxy volume set to {}", initial));
    unsafe { vol.SetMasterVolume(initial, &OWN_CTX)? };

    let prev_vol = Arc::new(AtomicU32::new(initial.to_bits()));

    let hook_ctx = Arc::new(ForegroundHookContext {
        own_vol: vol.clone(),
        device: device.clone(),
        prev_vol: prev_vol.clone(),
    });

    if let Ok(mut lock) = FOREGROUND_HOOK_CTX.write() {
        *lock = Some(hook_ctx);
    }

    let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let session_notification: IAudioSessionNotification = SessionManagerEvents.into();
    unsafe { mgr.RegisterSessionNotification(&session_notification)? };

    let _mgr_tracker = SessionManagerNotificationTracker {
        mgr: mgr.clone(),
        notification: session_notification.clone(),
    };

    let mut trackers = Vec::new();
    if let Ok(list) = unsafe { mgr.GetSessionEnumerator() } {
        if let Ok(n) = unsafe { list.GetCount() } {
            for i in 0..n {
                if let Ok(session) = unsafe { list.GetSession(i) } {
                    if let Ok(ctrl2) = session.cast::<IAudioSessionControl2>() {
                        let events: IAudioSessionEvents = AppSessionEvents { session: ctrl2 }.into();
                        if unsafe { session.RegisterAudioSessionNotification(&events) }.is_ok() {
                            trackers.push(SessionEventTracker { session: session.clone(), events });
                        }
                    }
                }
            }
        }
    }
    if let Ok(mut lock) = ALL_SESSIONS.write() {
        *lock = Some(trackers);
    }

    let events: IAudioSessionEvents = Proxy {
        own_vol: vol.clone(),
        device: device.clone(),
        prev_vol: prev_vol.clone(),
    }.into();
    unsafe { ctrl.RegisterAudioSessionNotification(&events)? };

    struct ProxyEventTracker {
        ctrl: IAudioSessionControl,
        events: IAudioSessionEvents,
    }
    impl Drop for ProxyEventTracker {
        fn drop(&mut self) {
            unsafe {
                let _ = self.ctrl.UnregisterAudioSessionNotification(&self.events);
            }
        }
    }
    let _proxy_tracker = ProxyEventTracker {
        ctrl: ctrl.clone(),
        events: events.clone(),
    };

    unsafe { client.Start()? };
    log("Audio client successfully started and proxy registered.");

    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let buf_frames = unsafe { client.GetBufferSize()? };
    unsafe {
        let buf = render.GetBuffer(buf_frames)?;
        std::ptr::write_bytes(buf, 0, buf_frames as usize * block_align);
        render.ReleaseBuffer(buf_frames, 0)?;
    }

    loop {
        let result = unsafe { MsgWaitForMultipleObjects(Some(&[wake]), false, INFINITE, QS_ALLINPUT) };

        if result == WAIT_FAILED {
            log("MsgWaitForMultipleObjects returned WAIT_FAILED!");
            return Err(windows::core::Error::from_win32());
        }

        if REINIT_NEEDED.load(Ordering::Acquire) {
            log("REINIT_NEEDED is true, exiting run_session loop cleanly.");
            if let Ok(mut lock) = FOREGROUND_HOOK_CTX.write() {
                *lock = None;
            }
            if let Ok(mut lock) = ALL_SESSIONS.write() {
                *lock = None;
            }
            // Non azzeriamo WAKE_HANDLE manualmente qui perché lo fa il _wake_guard in automatico 
            return Ok(());
        }

        if result.0 == WAIT_OBJECT_0.0 {
            match unsafe { client.GetCurrentPadding() } {
                Ok(padding) => {
                    let avail = buf_frames - padding;
                    if avail > 0 {
                        match unsafe { render.GetBuffer(avail) } {
                            Ok(buf) => unsafe {
                                std::ptr::write_bytes(buf, 0, avail as usize * block_align);
                                if render.ReleaseBuffer(avail, 0).is_err() {
                                    thread::sleep(std::time::Duration::from_millis(500));
                                }
                            },
                            Err(_) => { thread::sleep(std::time::Duration::from_millis(500)); }
                        }
                    }
                }
                Err(_) => { thread::sleep(std::time::Duration::from_millis(500)); }
            }
        } else if result.0 == WAIT_OBJECT_0.0 + 1 {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { let _ = TranslateMessage(&msg); }
                unsafe { DispatchMessageW(&msg); }
            }
        }
    }
}

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg == "--verbose") {
        VERBOSE_LOGGING.store(true, Ordering::Relaxed);
    }
    log("=== VolumeProxy Executable Started ===");
    unsafe {
        let process = GetCurrentProcess();
        let _ = SetPriorityClass(process, IDLE_PRIORITY_CLASS);
        let mut throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            process,
            ProcessPowerThrottling,
            &mut throttling as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
        log("Applied IDLE_PRIORITY_CLASS and PowerThrottling.");
    }

    // Evita crash se per qualche motivo il thread principale risulta già inizializzato
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    thread::spawn(|| {
        log("Hook thread starting...");
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        
        // Aspetta che il desktop/shell sia pronto prima di agganciare gli eventi
        let mut wait_logged = false;
        while unsafe { GetShellWindow() }.0.is_null() {
            if !wait_logged {
                log("Waiting for Shell Window to be ready...");
                wait_logged = true;
            }
            thread::sleep(std::time::Duration::from_secs(1));
        }
        log("Shell Window is ready.");

        let mut hook = HWINEVENTHOOK::default();
        
        // Loop protettivo: se lanciato da startup l'ambiente desktop potrebbe non essere pronto per generare l'hook
        while hook == HWINEVENTHOOK::default() {
            hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
                    None, Some(foreground_event_proc),
                    0, 0,
                    WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
                )
            };
            if hook == HWINEVENTHOOK::default() {
                log("SetWinEventHook failed, retrying in 2s...");
                thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        log("SetWinEventHook registered successfully!");

        let mut msg = MSG::default();
        loop {
            let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
            if ret.0 == 0 {
                break; // WM_QUIT (Normale spegnimento)
            } else if ret.0 == -1 {
                // [FIX 2] Gestione errore critica! Evita che il processo esca prematuramente se riceve 
                // un errore dal gestore finestre per via di un caricamento ritardato all'avvio.
                thread::sleep(std::time::Duration::from_millis(500));
                continue;
            }
            unsafe { let _ = TranslateMessage(&msg); }
            unsafe { DispatchMessageW(&msg); }
        }
    });

    loop {
        match run_session() {
            Ok(()) => {
                log("run_session exited cleanly. Sleeping 500ms and restarting.");
                thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                log(&format!("run_session ERROR: {:?}. Retrying in 5 seconds.", e));
                // Se c'è un errore (es. servizio audio o device audio scollegato in fase di boot), 
                // ora il loop lo cattura e riprova in sicurezza dopo 5 secondi.
                thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }
}