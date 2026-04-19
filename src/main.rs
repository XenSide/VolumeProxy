#![windows_subsystem = "windows"]

use std::sync::{atomic::{AtomicU32, Ordering}, Arc};
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

// Passed as event_context when we set our own volume to skip the re-entrant callback
const OWN_CTX: GUID = GUID {
    data1: 0x7A3B_5C1D,
    data2: 0xE2F4,
    data3: 0x4A6B,
    data4: [0x8C, 0x9D, 0xAE, 0xBF, 0xC0, 0xD1, 0xE2, 0xF3],
};

struct ForegroundHookContext {
    own_vol: ISimpleAudioVolume,
    device: IMMDevice,
    prev_vol: Arc<AtomicU32>,
}

static mut FOREGROUND_HOOK_CTX: *const ForegroundHookContext = std::ptr::null();

#[implement(IAudioSessionEvents)]
struct Proxy {
    own_vol: ISimpleAudioVolume,
    device: IMMDevice,
    // Tracks the volume we last reported so we can compute Elgato's step delta
    prev_vol: Arc<AtomicU32>,
}

impl IAudioSessionEvents_Impl for Proxy_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        new_volume: f32,
        _new_mute: BOOL,
        event_context: *const GUID,
    ) -> Result<()> {
        // Skip callbacks triggered by our own SetMasterVolume calls
        if !event_context.is_null() && unsafe { *event_context } == OWN_CTX {
            return Ok(());
        }

        let prev = f32::from_bits(self.prev_vol.load(Ordering::Relaxed));
        let delta = new_volume - prev;
        if delta.abs() < 0.001 {
            return Ok(());
        }

        // Apply delta to the foreground app; get its actual resulting volume
        let actual = apply_delta_to_foreground(&self.device, delta).unwrap_or(new_volume);

        // Sync our session volume to the foreground's real volume so Elgato's display is accurate
        let _ = unsafe { self.own_vol.SetMasterVolume(actual, &OWN_CTX) };

        self.prev_vol.store(actual.to_bits(), Ordering::Relaxed);

        Ok(())
    }

    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> {
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> Result<()> {
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
        if let Ok(cur) = unsafe { vol.GetMasterVolume() } {
            return Some(cur);
        }
    }

    None
}

fn get_foreground_volume(device: &IMMDevice) -> Option<f32> {
    let pid = foreground_pid();
    if pid == 0 {
        return None;
    }
    get_volume_for_pid(device, pid)
}

unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _dw_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND || hwnd.0.is_null() {
        return;
    }

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return;
    }

    let ctx = unsafe { FOREGROUND_HOOK_CTX };
    if ctx.is_null() {
        return;
    }
    let ctx = unsafe { &*ctx };

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
    if pid == 0 {
        return None;
    }

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

fn foreground_pid() -> u32 {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    let denum: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice =
        unsafe { denum.GetDefaultAudioEndpoint(eRender, eConsole)? };

    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

    let fmt = unsafe { client.GetMixFormat()? };
    let block_align = unsafe { (*fmt).nBlockAlign as usize };
    let mut period = 0i64;
    unsafe { client.GetDevicePeriod(Some(&mut period), None)? };

    // Retry initialization if device is in exclusive mode (indefinitely, every 30 seconds)
    loop {
        let result = unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                period,
                0,
                fmt,
                None,
            )
        };
        match result {
            Ok(()) => break,
            Err(e) if e.code() == AUDCLNT_E_DEVICE_IN_USE => {
                thread::sleep(std::time::Duration::from_secs(30));
            }
            Err(e) => return Err(e),
        }
    }
    unsafe { CoTaskMemFree(Some(fmt as *const core::ffi::c_void)); };

    let wake = unsafe { CreateEventW(None, false, false, None)? };
    unsafe { client.SetEventHandle(wake)? };

    let vol: ISimpleAudioVolume = unsafe { client.GetService()? };
    let ctrl: IAudioSessionControl = unsafe { client.GetService()? };

    // Start at the current foreground app volume, if available, otherwise fall back to 50%.
    let initial = get_foreground_volume(&device).unwrap_or(0.5f32);
    unsafe { vol.SetMasterVolume(initial, &OWN_CTX)? };

    let prev_vol = Arc::new(AtomicU32::new(initial.to_bits()));
    let hook_ctx = Box::new(ForegroundHookContext {
        own_vol: vol.clone(),
        device: device.clone(),
        prev_vol: prev_vol.clone(),
    });
    let hook_ctx_ptr = Box::into_raw(hook_ctx);
    unsafe { FOREGROUND_HOOK_CTX = hook_ctx_ptr };

    let _hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        )
    };

    let events: IAudioSessionEvents = Proxy {
        own_vol: vol.clone(),
        device: device.clone(),
        prev_vol: prev_vol.clone(),
    }
    .into();
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
        if result.0 == WAIT_OBJECT_0.0 {
            let padding_result = unsafe { client.GetCurrentPadding() };
            match padding_result {
                Ok(padding) => {
                    let avail = buf_frames - padding;
                    if avail > 0 {
                        match unsafe { render.GetBuffer(avail) } {
                            Ok(buf) => {
                                unsafe {
                                    std::ptr::write_bytes(buf, 0, avail as usize * block_align);
                                    if let Err(_) = render.ReleaseBuffer(avail, 0) {
                                        thread::sleep(std::time::Duration::from_secs(30));
                                    }
                                }
                            }
                            Err(_) => {
                                thread::sleep(std::time::Duration::from_secs(30));
                            }
                        }
                    }
                }
                Err(_) => {
                    thread::sleep(std::time::Duration::from_secs(30));
                }
            }
        } else if result.0 == WAIT_OBJECT_0.0 + 1 {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe { let _ = TranslateMessage(&msg); }
                unsafe { DispatchMessageW(&msg) };
            }
        } else if result.0 == WAIT_FAILED.0 {
            thread::sleep(std::time::Duration::from_secs(30));
        }
    }
}
