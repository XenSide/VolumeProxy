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
        UI::Shell::PropertiesSystem::PROPERTYKEY,
    },
};

const OWN_CTX: GUID = GUID {
    data1: 0x7A3B_5C1D,
    data2: 0xE2F4,
    data3: 0x4A6B,
    data4:[0x8C, 0x9D, 0xAE, 0xBF, 0xC0, 0xD1, 0xE2, 0xF3],
};

fn own_write(event_context: *const GUID) -> bool {
    !event_context.is_null() && unsafe { *event_context } == OWN_CTX
}

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
    client: IAudioClient,
    stream_active: Arc<AtomicBool>,
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

        let events: IAudioSessionEvents = AppSessionEvents { session: ctrl2.clone() }.into();

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
        
        let pid = unsafe { ctrl2.GetProcessId().unwrap_or(0) };
        if pid != 0 && pid == foreground_pid() {
            let wait_val = WAKE_UP_EVENT_PTR.load(Ordering::Acquire);
            if wait_val != 0 {
                unsafe { let _ = SetEvent(HANDLE(wait_val as *mut _)); }
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
        if own_write(event_context) {
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
static WAKE_UP_EVENT_PTR: AtomicUsize = AtomicUsize::new(0);
static TEARDOWN_REQUEST_TIME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
        if own_write(event_context) {
            return Ok(());
        }

        let prev = f32::from_bits(self.prev_vol.load(Ordering::Relaxed));
        if (new_volume - prev).abs() < 0.001 {
            return Ok(());
        }

        // Absolute, not a delta: anyone's write (Stream Deck, mixer, AudioDuck) lands on the app as the same value,
        // so the two sides can't drift or ratchet. OWN_CTX keeps the app's change event from echoing back here.
        set_foreground_volume(&self.device, new_volume);
        self.prev_vol.store(new_volume.to_bits(), Ordering::Relaxed);
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

#[implement(IMMNotificationClient)]
struct DefaultDeviceTracker;

impl IMMNotificationClient_Impl for DefaultDeviceTracker_Impl {
    fn OnDeviceStateChanged(&self, _pwstrdeviceid: &PCWSTR, _dwnewstate: DEVICE_STATE) -> Result<()> { Ok(()) }
    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> { Ok(()) }
    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> { Ok(()) }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _pwstrdefaultdeviceid: &PCWSTR) -> Result<()> {
        if flow == eRender && role == eConsole {
            log("Default audio device changed! Requesting session teardown.");
            REINIT_NEEDED.store(true, Ordering::Release);
            let handle_val = WAKE_HANDLE_PTR.load(Ordering::Acquire);
            if handle_val != 0 {
                unsafe { let _ = SetEvent(HANDLE(handle_val as *mut _)); }
            }
            let wait_val = WAKE_UP_EVENT_PTR.load(Ordering::Acquire);
            if wait_val != 0 {
                unsafe { let _ = SetEvent(HANDLE(wait_val as *mut _)); }
            }
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _pwstrdeviceid: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> { Ok(()) }
}

struct DeviceNotificationTracker {
    denum: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}
impl Drop for DeviceNotificationTracker {
    fn drop(&mut self) {
        unsafe {
            let _ = self.denum.UnregisterEndpointNotificationCallback(&self.client);
        }
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

fn get_process_name(pid: u32) -> String {
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; MAX_PATH as usize];
            let mut size = buf.len() as u32;
            let success = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut size);
            let _ = CloseHandle(h);
            if success.is_ok() {
                let name = String::from_utf16_lossy(&buf[..size as usize]);
                if let Some(idx) = name.rfind('\\') {
                    return format!("{} (PID: {})", &name[idx + 1..], pid);
                }
                return format!("{} (PID: {})", name, pid);
            }
        }
    }
    format!("PID: {}", pid)
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK, event: u32, hwnd: HWND,
    _idobject: i32, _idchild: i32, _dw_event_thread: u32, _dwms_event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND || hwnd.0.is_null() { return; }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 { return; }
    log(&format!("Foreground window changed. {}", get_process_name(pid)));

    let wait_val = WAKE_UP_EVENT_PTR.load(Ordering::Acquire);
    if wait_val != 0 {
        unsafe { let _ = SetEvent(HANDLE(wait_val as *mut _)); }
    }

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

    let mut has_audio = false;
    if let Some(volume) = get_volume_for_pid(&ctx.device, pid) {
        has_audio = true;
        let prev = f32::from_bits(ctx.prev_vol.load(Ordering::Relaxed));
        if (volume - prev).abs() >= 0.001 {
            if unsafe { ctx.own_vol.SetMasterVolume(volume, &OWN_CTX) }.is_ok() {
                ctx.prev_vol.store(volume.to_bits(), Ordering::Relaxed);
            }
        }
    }

    let currently_active = ctx.stream_active.load(Ordering::Relaxed);
    if has_audio {
        TEARDOWN_REQUEST_TIME.store(0, Ordering::Release);
        if !currently_active {
            log(&format!("Foreground app {} has audio. Resuming proxy stream.", get_process_name(pid)));
            unsafe { let _ = ctx.client.Start(); }
            ctx.stream_active.store(true, Ordering::Relaxed);
        }
    } else {
        if currently_active {
            log(&format!("Foreground app {} has NO audio. Pausing proxy stream and scheduling teardown in 5s.", get_process_name(pid)));
            unsafe { let _ = ctx.client.Stop(); }
            ctx.stream_active.store(false, Ordering::Relaxed);
            
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            TEARDOWN_REQUEST_TIME.store(now + 5000, Ordering::Release);
            
            let handle_val = WAKE_HANDLE_PTR.load(Ordering::Acquire);
            if handle_val != 0 {
                unsafe { let _ = SetEvent(HANDLE(handle_val as *mut _)); }
            }
        }
    }
}

fn set_foreground_volume(device: &IMMDevice, volume: f32) -> Option<()> {
    let pid = foreground_pid();
    if pid == 0 { return None; }
    let mgr: IAudioSessionManager2 = unsafe { device.Activate(CLSCTX_ALL, None).ok()? };
    let list = unsafe { mgr.GetSessionEnumerator().ok()? };
    let n = unsafe { list.GetCount().ok()? };
    for i in 0..n {
        let Ok(ctrl) = (unsafe { list.GetSession(i) }) else { continue };
        let Ok(ctrl2) = ctrl.cast::<IAudioSessionControl2>() else { continue };
        if unsafe { ctrl2.GetProcessId().unwrap_or(0) } != pid { continue; }
        let Ok(vol) = ctrl.cast::<ISimpleAudioVolume>() else { continue };
        if unsafe { vol.SetMasterVolume(volume, &OWN_CTX) }.is_ok() {
            log(&format!("Set volume {} on {}", volume, get_process_name(pid)));
        }
    }
    Some(())
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

    let denum: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice = unsafe { denum.GetDefaultAudioEndpoint(eRender, eConsole)? };
    log("Got default audio endpoint.");

    let default_device_tracker: IMMNotificationClient = DefaultDeviceTracker {}.into();
    unsafe { denum.RegisterEndpointNotificationCallback(&default_device_tracker)? };
    let _dev_tracker = DeviceNotificationTracker {
        denum: denum.clone(),
        client: default_device_tracker,
    };

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
                        let events: IAudioSessionEvents = AppSessionEvents { session: ctrl2.clone() }.into();
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

    let wait_event = unsafe { CreateEventW(None, false, false, None)? };
    WAKE_UP_EVENT_PTR.store(wait_event.0 as usize, Ordering::Release);

    log("Waiting for foreground audio before creating proxy session...");
    loop {
        if REINIT_NEEDED.load(Ordering::Acquire) { 
            WAKE_UP_EVENT_PTR.store(0, Ordering::Release);
            unsafe { let _ = CloseHandle(wait_event); }
            return Ok(()); 
        }

        if get_foreground_volume(&device).is_some() {
            log("Foreground audio detected! Proceeding to create proxy session.");
            break;
        }

        let result = unsafe { MsgWaitForMultipleObjects(Some(&[wait_event]), false, 1000, QS_ALLINPUT) };
        if result.0 == WAIT_OBJECT_0.0 + 1 {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { let _ = TranslateMessage(&msg); }
                unsafe { DispatchMessageW(&msg); }
            }
        }
    }
    
    WAKE_UP_EVENT_PTR.store(0, Ordering::Release);
    unsafe { let _ = CloseHandle(wait_event); }

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
    
    let _wake_guard = EventHandleGuard(wake);

    let vol: ISimpleAudioVolume = unsafe { client.GetService()? };
    let ctrl: IAudioSessionControl = unsafe { client.GetService()? };
    unsafe { client.SetEventHandle(wake)? };

    let initial = get_foreground_volume(&device);
    let start_active = initial.is_some();
    let vol_val = initial.unwrap_or(0.5);
    log(&format!("Initial proxy volume set to {}", vol_val));
    unsafe { vol.SetMasterVolume(vol_val, &OWN_CTX)? };

    let prev_vol = Arc::new(AtomicU32::new(vol_val.to_bits()));
    let stream_active = Arc::new(AtomicBool::new(start_active));

    let hook_ctx = Arc::new(ForegroundHookContext {
        own_vol: vol.clone(),
        device: device.clone(),
        prev_vol: prev_vol.clone(),
        client: client.clone(),
        stream_active: stream_active.clone(),
    });

    if let Ok(mut lock) = FOREGROUND_HOOK_CTX.write() {
        *lock = Some(hook_ctx);
    }

    let events: IAudioSessionEvents = Proxy {
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

    if start_active {
        unsafe { client.Start()? };
        log("Audio client successfully started and proxy registered.");
    } else {
        log("Audio client paused (initial foreground has no audio).");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        TEARDOWN_REQUEST_TIME.store(now + 5000, Ordering::Release);
    }

    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let buf_frames = unsafe { client.GetBufferSize()? };
    unsafe {
        let buf = render.GetBuffer(buf_frames)?;
        std::ptr::write_bytes(buf, 0, buf_frames as usize * block_align);
        render.ReleaseBuffer(buf_frames, 0)?;
    }

    loop {
        let mut timeout = INFINITE;
        let td_time = TEARDOWN_REQUEST_TIME.load(Ordering::Acquire);
        if td_time != 0 {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
            if now >= td_time {
                log("Teardown timeout reached. Exiting run_session to clear mixer cache.");
                REINIT_NEEDED.store(true, Ordering::Release);
                timeout = 0;
            } else {
                timeout = (td_time - now) as u32;
            }
        }

        let result = unsafe { MsgWaitForMultipleObjects(Some(&[wake]), false, timeout, QS_ALLINPUT) };

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

        if result.0 == WAIT_TIMEOUT.0 {
            let td_time = TEARDOWN_REQUEST_TIME.load(Ordering::Acquire);
            if td_time != 0 {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
                if now >= td_time {
                    log("Teardown timeout reached. Exiting run_session to clear mixer cache.");
                    REINIT_NEEDED.store(true, Ordering::Release);
                    continue;
                }
            }
            continue;
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

    // Single instance across every copy (Stream Deck plugins, Startup...). Retry briefly:
    // on a self-respawn the old process still holds the mutex until it exits.
    let mut _instance = None;
    for _ in 0..25 {
        let h = unsafe { CreateMutexW(None, true, w!("Local\\xenside.VolumeProxy.Foreground")) };
        match h {
            Ok(h) if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS => { _instance = Some(h); break; }
            Ok(h) => unsafe { let _ = CloseHandle(h); },
            Err(_) => {}
        }
        thread::sleep(std::time::Duration::from_millis(200));
    }
    if _instance.is_none() {
        log("Another instance is already running. Exiting.");
        return Ok(());
    }

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
                log("run_session exited cleanly. Spawning new instance to clear Windows Volume Mixer cache.");
                if let Ok(exe) = std::env::current_exe() {
                    let mut cmd = std::process::Command::new(exe);
                    if VERBOSE_LOGGING.load(Ordering::Relaxed) {
                        cmd.arg("--verbose");
                    }
                    if cmd.spawn().is_ok() {
                        std::process::exit(0);
                    } else {
                        log("Failed to spawn new process. Falling back to same-process restart.");
                    }
                } else {
                    log("Failed to get current exe path. Falling back to same-process restart.");
                }
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