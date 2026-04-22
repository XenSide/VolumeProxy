#![windows_subsystem = "windows"]

use std::sync::{atomic::{AtomicBool, AtomicU32, Ordering}, Arc, RwLock};
use std::thread;
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

// --- Strutture per tracciare le sessioni audio globalmente ---

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
                    // Previene memory leaks rimuovendo le sessioni morte quando se ne crea una nuova
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
        // Ignora i cambiamenti da noi causati usando la const OWN_CTX per evitare loop infiniti
        if !event_context.is_null() && unsafe { *event_context } == OWN_CTX {
            return Ok(());
        }

        // Recupera il PID di questa sessione specifica in tempo reale e ignoralo se non è quello in foreground
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
            // Sincronizza il volume del proxy se il volume dell'app esterna è variato
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

// --- FINE NUOVE STRUTTURE ---

static REINIT_NEEDED: AtomicBool = AtomicBool::new(false);
static mut WAKE_HANDLE: HANDLE = HANDLE(std::ptr::null_mut());

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
        REINIT_NEEDED.store(true, Ordering::Release);
        unsafe {
            if !WAKE_HANDLE.0.is_null() {
                let _ = SetEvent(WAKE_HANDLE);
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
            result = Some(next);
        }
    }
    result
}

fn run_session(denum: &IMMDeviceEnumerator) -> Result<()> {
    REINIT_NEEDED.store(false, Ordering::Release);
    if let Ok(mut lock) = ALL_SESSIONS.write() {
        *lock = None;
    }

    let device: IMMDevice = unsafe { denum.GetDefaultAudioEndpoint(eRender, eConsole)? };
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
                thread::sleep(std::time::Duration::from_secs(2));
                if REINIT_NEEDED.load(Ordering::Acquire) { return Ok(()); }
            }
            Err(e) => return Err(e),
        }
    }
    unsafe { CoTaskMemFree(Some(fmt as *const core::ffi::c_void)) };

    let wake = unsafe { CreateEventW(None, false, false, None)? };
    unsafe { WAKE_HANDLE = wake };

    let vol: ISimpleAudioVolume = unsafe { client.GetService()? };
    let ctrl: IAudioSessionControl = unsafe { client.GetService()? };
    unsafe { client.SetEventHandle(wake)? };

    let initial = get_foreground_volume(&device).unwrap_or(0.5);
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

    // Registra ascolto sulle modifiche e creazioni sessioni del sistema 
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
        prev_vol,
    }.into();
    unsafe { ctrl.RegisterAudioSessionNotification(&events)? };
    unsafe { client.Start()? };

    let render: IAudioRenderClient = unsafe { client.GetService()? };
    let buf_frames = unsafe { client.GetBufferSize()? };
    unsafe {
        let buf = render.GetBuffer(buf_frames)?;
        std::ptr::write_bytes(buf, 0, buf_frames as usize * block_align);
        render.ReleaseBuffer(buf_frames, 0)?;
    }

    loop {
        let result = unsafe { MsgWaitForMultipleObjects(Some(&[wake]), false, INFINITE, QS_ALLINPUT) };

        if REINIT_NEEDED.load(Ordering::Acquire) {
            if let Ok(mut lock) = FOREGROUND_HOOK_CTX.write() {
                *lock = None;
            }
            if let Ok(mut lock) = ALL_SESSIONS.write() {
                *lock = None;
            }
            unsafe { WAKE_HANDLE = HANDLE(std::ptr::null_mut()); }
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
                unsafe { DispatchMessageW(&msg) };
            }
        }
    }
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    thread::spawn(|| {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() };
        let _hook = unsafe {
            SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND,
                None, Some(foreground_event_proc),
                0, 0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };

        let mut msg = MSG::default();
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
            unsafe { let _ = TranslateMessage(&msg); }
            unsafe { DispatchMessageW(&msg); }
        }
    });

    let denum: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    loop {
        match run_session(&denum) {
            Ok(()) => {
                thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(_) => {
                thread::sleep(std::time::Duration::from_secs(5));
            }
        }
    }
}