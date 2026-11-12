// monitor-mirror v0.3.0: mirrors audio from one render endpoint (WASAPI loopback) to another,
// with a lightweight system-tray UI for device selection, volume sync, and auto-start.
//
//   monitor-mirror.exe --list
//   monitor-mirror.exe --src "<text>" --dst "<text>" [--cushion <ms>]
//   monitor-mirror.exe                              (tray-only, remembers saved devices)
//
// GUI subsystem: no console window when running in the background.
#![windows_subsystem = "windows"]

use std::{
    io::Write,
    mem, ptr,
    sync::{
        atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU32, AtomicU8, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use windows::{
    core::{implement, w, Error, Result, HSTRING, PCWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{
            CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, E_FAIL, HWND, LPARAM, LRESULT,
            WAIT_TIMEOUT, WPARAM,
        },
        Media::Audio::{
            eRender,
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
            MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIO_VOLUME_NOTIFICATION_DATA,
            DEVICE_STATE_ACTIVE,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                StructuredStorage::PropVariantToStringAlloc, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
                COINIT_MULTITHREADED, STGM_READ,
            },
            Console::{AttachConsole, ATTACH_PARENT_PROCESS},
            LibraryLoader::GetModuleHandleW,
            Registry::{
                RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
                REG_DWORD, REG_SZ,
            },
            Threading::{
                AvSetMmThreadCharacteristicsW, CreateEventW, CreateMutexW, SetEvent,
                WaitForMultipleObjects, WaitForSingleObject,
            },
        },
        UI::{
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW,
                MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
                TrackPopupMenu, TranslateMessage, IDI_APPLICATION, MF_CHECKED, MF_GRAYED,
                MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MB_ICONERROR, MB_OK, MSG, TPM_BOTTOMALIGN,
                TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_DESTROY,
                WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
            },
        },
    },
};

// ─── Constants ───────────────────────────────────────────────────────────────

const BUFFERFLAGS_SILENT: u32 = 0x2;

/// Custom message the tray icon posts to our hidden window.
const WM_TRAYICON: u32 = WM_APP + 1;

/// Custom message the audio thread posts to update the tray status.
const WM_STATUS_UPDATE: u32 = WM_APP + 2;

// Context-menu item IDs.
const IDM_EXIT: u16 = 1;
const IDM_AUTOSTART: u16 = 2;
const IDM_VOLSYNC: u16 = 3;
const IDM_SRC_BASE: u16 = 1000; // source devices  1000 .. 1999
const IDM_DST_BASE: u16 = 2000; // dest   devices  2000 .. 2999

const REG_APP_NAME: &str = "MonitorMirror";
const REG_SETTINGS_SUBKEY: PCWSTR = w!("Software\\MonitorMirror");

// Audio thread status codes.
const STATUS_IDLE: u8 = 0;
const STATUS_ACTIVE: u8 = 1;
const STATUS_WAITING: u8 = 2;

// ─── Shared state ────────────────────────────────────────────────────────────

/// Describes an audio endpoint.
#[derive(Clone)]
struct DeviceInfo {
    id: String,
    name: String,
}

/// Thread-safe state shared between the UI thread and the WASAPI audio thread.
struct Shared {
    src: Mutex<String>,
    src_name: Mutex<String>,
    dst: Mutex<String>,
    dst_name: Mutex<String>,
    cushion_ms: AtomicU32,
    /// Signal the audio thread to tear down and reconnect with a new device pair.
    restart: AtomicBool,
    /// Signal the audio thread to stop completely.
    quit: AtomicBool,
    /// Whether volume synchronization is active.
    vol_sync: AtomicBool,
    /// Win32 auto-reset event for instant wake-up of the audio thread's backoff sleep.
    /// Stored as AtomicUsize because HANDLE is not Send/Sync.
    wake_event: AtomicUsize,
    /// Current audio thread status (STATUS_IDLE / STATUS_ACTIVE / STATUS_WAITING).
    status: AtomicU8,
    /// HWND of the hidden message window, for audio→UI communication.
    /// Stored as AtomicIsize because HWND is not Send/Sync.
    hwnd: AtomicIsize,
    /// Whether we've already shown the "connected" balloon notification.
    notified: AtomicBool,
}

impl Shared {
    fn new(
        src: String,
        src_name: String,
        dst: String,
        dst_name: String,
        cushion_ms: u32,
        vol_sync: bool,
        wake_event: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            src: Mutex::new(src),
            src_name: Mutex::new(src_name),
            dst: Mutex::new(dst),
            dst_name: Mutex::new(dst_name),
            cushion_ms: AtomicU32::new(cushion_ms),
            restart: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            vol_sync: AtomicBool::new(vol_sync),
            wake_event: AtomicUsize::new(wake_event),
            status: AtomicU8::new(STATUS_IDLE),
            hwnd: AtomicIsize::new(0),
            notified: AtomicBool::new(false),
        })
    }

    /// Post a status update to the UI thread so it can refresh the tooltip.
    fn set_status(&self, s: u8) {
        self.status.store(s, Ordering::Relaxed);
        let h = self.hwnd.load(Ordering::Relaxed);
        if h != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(h as *mut _)),
                    WM_STATUS_UPDATE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }

    /// Signal the wake event so the audio thread's backoff exits immediately.
    fn signal_wake(&self) {
        let raw = self.wake_event.load(Ordering::Relaxed);
        if raw != 0 {
            unsafe {
                let _ = SetEvent(windows::Win32::Foundation::HANDLE(raw as *mut _));
            }
        }
    }
}

// ─── Device helpers ──────────────────────────────────────────────────────────

unsafe fn describe(d: &IMMDevice) -> Result<DeviceInfo> {
    let id_p = d.GetId()?;
    let id = id_p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(id_p.0 as _));
    let store = d.OpenPropertyStore(STGM_READ)?;
    let pv = store.GetValue(&PKEY_Device_FriendlyName)?;
    let name_p = PropVariantToStringAlloc(&pv)?;
    let name = name_p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(name_p.0 as _));
    Ok(DeviceInfo { id, name })
}

/// Enumerate all active render endpoints.
unsafe fn enumerate_devices(en: &IMMDeviceEnumerator) -> Result<Vec<(IMMDevice, DeviceInfo)>> {
    let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let mut devs = Vec::new();
    for i in 0..col.GetCount()? {
        let d = col.Item(i)?;
        let info = describe(&d)?;
        devs.push((d, info));
    }
    Ok(devs)
}

enum Found {
    None,
    One(IMMDevice, DeviceInfo),
    Many(usize),
}

unsafe fn find(en: &IMMDeviceEnumerator, needle: &str) -> Result<Found> {
    let needle_lower = needle.to_lowercase();
    let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let mut hits = Vec::new();
    for i in 0..col.GetCount()? {
        let d = col.Item(i)?;
        let info = describe(&d)?;
        if info.id.to_lowercase() == needle_lower {
            return Ok(Found::One(d, info));
        }
        if info.name.to_lowercase().contains(&needle_lower)
            || info.id.to_lowercase().contains(&needle_lower)
        {
            hits.push((d, info));
        }
    }
    Ok(match hits.len() {
        0 => Found::None,
        1 => {
            let (d, info) = hits.pop().unwrap();
            Found::One(d, info)
        }
        n => Found::Many(n),
    })
}

unsafe fn list_devices(en: &IMMDeviceEnumerator) -> Result<()> {
    let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .open("CONOUT$")
        .map_err(|_| Error::from(E_FAIL))?;
    let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    for i in 0..col.GetCount()? {
        let info = describe(&col.Item(i)?)?;
        let _ = writeln!(out, "{}\n    {}", info.name, info.id);
    }
    Ok(())
}

// ─── Volume synchronization ─────────────────────────────────────────────────

/// COM callback that fires when the source device's master volume changes.
/// Applies the same level, mute state, AND per-channel balance to the destination.
#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    dst_vol: IAudioEndpointVolume,
    shared: Arc<Shared>,
    /// Thread-safe atomic flag preventing recursive re-entrant volume events.
    applying: Arc<AtomicBool>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        if !self.shared.vol_sync.load(Ordering::Relaxed) || data.is_null() {
            return Ok(());
        }
        // Avoid feedback / re-entrancy when setting the destination device's volume.
        if self.applying.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        unsafe {
            let d = &*data;
            // Master volume + mute
            let _ = self
                .dst_vol
                .SetMasterVolumeLevelScalar(d.fMasterVolume, ptr::null());
            let _ = self.dst_vol.SetMute(d.bMuted.into(), ptr::null());

            // Per-channel volume (mirrors L/R balance)
            let src_channels = d.nChannels as usize;
            if src_channels > 0 {
                if let Ok(dst_channels) = self.dst_vol.GetChannelCount() {
                    let src_levels =
                        std::slice::from_raw_parts(d.afChannelVolumes.as_ptr(), src_channels);
                    let ch_count = src_channels.min(dst_channels as usize);
                    for ch in 0..ch_count {
                        let _ = self.dst_vol.SetChannelVolumeLevelScalar(
                            ch as u32,
                            src_levels[ch],
                            ptr::null(),
                        );
                    }
                }
            }
        }
        self.applying.store(false, Ordering::SeqCst);
        Ok(())
    }
}

// ─── WASAPI mirror session ───────────────────────────────────────────────────

/// One mirroring session. Returns Err on any device problem so caller can reconnect.
unsafe fn session(en: &IMMDeviceEnumerator, shared: &Arc<Shared>) -> Result<()> {
    let src_needle = shared.src.lock().unwrap().clone();
    let dst_needle = shared.dst.lock().unwrap().clone();
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);

    if src_needle.is_empty() || dst_needle.is_empty() {
        return Err(Error::from(E_FAIL));
    }

    let (src, src_info) = match find(en, &src_needle)? {
        Found::One(d, info) => (d, info),
        _ => return Err(Error::from(E_FAIL)),
    };
    let (dst, dst_info) = match find(en, &dst_needle)? {
        Found::One(d, info) => (d, info),
        _ => return Err(Error::from(E_FAIL)),
    };
    if src_info.id == dst_info.id {
        return Err(Error::from(E_FAIL));
    }

    // Keep human-readable names updated for the tray icon tooltip.
    *shared.src_name.lock().unwrap() = src_info.name;
    *shared.dst_name.lock().unwrap() = dst_info.name;

    // ── Open audio clients ──────────────────────────────────────────
    let sc: IAudioClient = src.Activate(CLSCTX_ALL, None)?;
    let dc: IAudioClient = dst.Activate(CLSCTX_ALL, None)?;

    let fmt = sc.GetMixFormat()?;
    let wf = ptr::read_unaligned(fmt);
    let (rate, frame_bytes) = (wf.nSamplesPerSec, wf.nBlockAlign as usize);

    let ev = CreateEventW(None, false, false, PCWSTR::null())?;
    let init = (|| -> Result<()> {
        sc.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            fmt,
            None,
        )?;
        sc.SetEventHandle(ev)?;
        dc.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            200_000, // 20 ms ring buffer (was 100 ms) — tighter = lower latency
            0,
            fmt,
            None,
        )
    })();
    CoTaskMemFree(Some(fmt as _));
    if let Err(e) = init {
        let _ = CloseHandle(ev);
        return Err(e);
    }

    // ── Volume synchronization setup ────────────────────────────────
    let src_vol: Option<IAudioEndpointVolume> = src.Activate(CLSCTX_ALL, None).ok();
    let dst_vol: Option<IAudioEndpointVolume> = dst.Activate(CLSCTX_ALL, None).ok();

    let vol_cb: Option<IAudioEndpointVolumeCallback> =
        if let (Some(sv), Some(dv)) = (&src_vol, &dst_vol) {
            // Initial volume match (master + per-channel)
            if shared.vol_sync.load(Ordering::Relaxed) {
                let level = sv.GetMasterVolumeLevelScalar().unwrap_or(1.0);
                let _ = dv.SetMasterVolumeLevelScalar(level, ptr::null());
                let muted = sv.GetMute().unwrap_or_default();
                let _ = dv.SetMute(muted.into(), ptr::null());

                // Sync per-channel balance at startup
                if let (Ok(src_ch), Ok(dst_ch)) = (sv.GetChannelCount(), dv.GetChannelCount()) {
                    let ch_count = (src_ch as usize).min(dst_ch as usize);
                    for ch in 0..ch_count {
                        if let Ok(lvl) = sv.GetChannelVolumeLevelScalar(ch as u32) {
                            let _ = dv.SetChannelVolumeLevelScalar(ch as u32, lvl, ptr::null());
                        }
                    }
                }
            }

            let callback = VolumeCallback {
                dst_vol: dv.clone(),
                shared: Arc::clone(shared),
                applying: Arc::new(AtomicBool::new(false)),
            };
            let cb: IAudioEndpointVolumeCallback = callback.into();
            sv.RegisterControlChangeNotify(&cb).ok();
            Some(cb)
        } else {
            None
        };

    // ── Audio mirror loop ───────────────────────────────────────────
    shared.set_status(STATUS_ACTIVE);

    // Show balloon notification on first successful connection
    if !shared.notified.swap(true, Ordering::Relaxed) {
        show_balloon(shared);
    }

    let result = (|| -> Result<()> {
        let cap: IAudioCaptureClient = sc.GetService()?;
        let rnd: IAudioRenderClient = dc.GetService()?;
        let ring = dc.GetBufferSize()?;

        let (mut period_hns, mut min_hns) = (0i64, 0i64);
        sc.GetDevicePeriod(Some(&mut period_hns), Some(&mut min_hns))?;
        let period = ((rate as i64 * period_hns / 10_000_000) as u32).max(1);
        let cushion = (rate / 1000 * cushion_ms).max(1);
        let high = (cushion + period).min(ring); // tighter cap: cushion + 1 period (was 2)

        let wake_raw = shared.wake_event.load(Ordering::Relaxed);
        let wake_handle = windows::Win32::Foundation::HANDLE(wake_raw as *mut _);
        let wait_handles = [ev, wake_handle];

        dc.Start()?;
        sc.Start()?;

        loop {
            let w = WaitForMultipleObjects(&wait_handles, false, 250);

            // Check control flags right after waking up (Point 4 & 5)
            if shared.restart.load(Ordering::Relaxed) || shared.quit.load(Ordering::Relaxed) {
                break;
            }

            loop {
                if cap.GetNextPacketSize()? == 0 {
                    break;
                }
                let mut data: *mut u8 = ptr::null_mut();
                let (mut frames, mut flags) = (0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;

                let queued = dc.GetCurrentPadding()?;
                let prime = if queued == 0 { cushion } else { 0 };
                let n = frames.min(high.saturating_sub(queued + prime));
                let total = prime + n;
                if total > 0 {
                    let out = rnd.GetBuffer(total)?;
                    if prime > 0 {
                        ptr::write_bytes(out, 0, prime as usize * frame_bytes);
                    }
                    let body = out.add(prime as usize * frame_bytes);
                    if flags & BUFFERFLAGS_SILENT != 0 {
                        ptr::write_bytes(body, 0, n as usize * frame_bytes);
                    } else {
                        ptr::copy_nonoverlapping(data, body, n as usize * frame_bytes);
                    }
                    rnd.ReleaseBuffer(total, 0)?;
                }
                cap.ReleaseBuffer(frames)?;
            }

            if w == WAIT_TIMEOUT {
                dc.GetCurrentPadding()?;
            }
        }
        Ok(())
    })();

    // ── Cleanup ─────────────────────────────────────────────────────
    let _ = sc.Stop();
    let _ = dc.Stop();
    if let (Some(sv), Some(cb)) = (&src_vol, &vol_cb) {
        let _ = sv.UnregisterControlChangeNotify(cb);
    }
    let _ = CloseHandle(ev);
    result
}

/// Show a one-time balloon notification when mirroring starts for the first time.
unsafe fn show_balloon(shared: &Shared) {
    let h = shared.hwnd.load(Ordering::Relaxed);
    if h == 0 {
        return;
    }
    let src_name = shared.src_name.lock().unwrap().clone();
    let dst_name = shared.dst_name.lock().unwrap().clone();
    let info_text = format!("{} → {}", trunc(&src_name, 50), trunc(&dst_name, 50));

    let mut nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(h as *mut _),
        uID: 1,
        uFlags: NIF_INFO,
        // NIIF_INFO (0x1) | NIIF_NOSOUND (0x10) = 0x11
        dwInfoFlags: NOTIFY_ICON_INFOTIP_FLAGS(0x11),
        ..Default::default()
    };
    copy_wide(&mut nid.szInfoTitle, "Mirror Active");
    copy_wide_256(&mut nid.szInfo, &info_text);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Audio thread entry point: loops reconnecting sessions until told to quit.
fn audio_thread(shared: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let mut task_idx = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_idx);

        let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        {
            Ok(e) => e,
            Err(_) => return,
        };

        while !shared.quit.load(Ordering::Relaxed) {
            let _ = session(&en, &shared);

            if shared.quit.load(Ordering::Relaxed) {
                break;
            }

            // If the user changed devices or settings, reconnect IMMEDIATELY (0ms lag!)
            if shared.restart.swap(false, Ordering::Relaxed) {
                continue;
            }

            // Only on actual device errors/disconnects (e.g. monitor asleep), back off.
            // Use WaitForSingleObject on the wake event instead of polling with sleep.
            shared.set_status(STATUS_WAITING);
            let wake_raw = shared.wake_event.load(Ordering::Relaxed);
            if wake_raw != 0 {
                let wake_handle = windows::Win32::Foundation::HANDLE(wake_raw as *mut _);
                let _ = WaitForSingleObject(wake_handle, 2000);
            } else {
                // Fallback: simple sleep (should not happen normally)
                thread::sleep(Duration::from_secs(2));
            }
        }

        shared.set_status(STATUS_IDLE);
    }
}

// ─── Registry: settings persistence & auto-start ────────────────────────────

fn load_persisted_config() -> (String, String, String, String, bool, Option<u32>) {
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            REG_SETTINGS_SUBKEY,
            Some(0),
            KEY_QUERY_VALUE,
            &mut key,
        )
        .is_err()
        {
            return (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                true,
                None,
            );
        }

        let read_str = |name: PCWSTR| -> String {
            let mut buf = [0u8; 512];
            let mut len = buf.len() as u32;
            if RegQueryValueExW(key, name, None, None, Some(buf.as_mut_ptr()), Some(&mut len))
                .is_ok()
                && len >= 2
            {
                let u16_slice =
                    std::slice::from_raw_parts(buf.as_ptr() as *const u16, (len as usize) / 2);
                let trimmed = u16_slice.split(|&c| c == 0).next().unwrap_or(&[]);
                String::from_utf16_lossy(trimmed)
            } else {
                String::new()
            }
        };

        let read_dword = |name: PCWSTR| -> Option<u32> {
            let mut val = 0u32;
            let mut len = 4u32;
            if RegQueryValueExW(
                key,
                name,
                None,
                None,
                Some(&mut val as *mut u32 as *mut u8),
                Some(&mut len),
            )
            .is_ok()
            {
                Some(val)
            } else {
                None
            }
        };

        let src = read_str(w!("Source"));
        let src_name = read_str(w!("SourceName"));
        let dst = read_str(w!("Destination"));
        let dst_name = read_str(w!("DestinationName"));

        let vol_sync = read_dword(w!("VolumeSync")).map(|v| v != 0).unwrap_or(true);
        let cushion_ms = read_dword(w!("CushionMs"));

        let _ = RegCloseKey(key);
        (src, src_name, dst, dst_name, vol_sync, cushion_ms)
    }
}

fn save_persisted_config(
    src: &str,
    src_name: &str,
    dst: &str,
    dst_name: &str,
    vol_sync: bool,
    cushion_ms: u32,
) {
    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            REG_SETTINGS_SUBKEY,
            Some(0),
            PCWSTR::null(),
            windows::Win32::System::Registry::REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
        .is_err()
        {
            return;
        }

        let write_str = |name: PCWSTR, val: &str| {
            let wide: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = RegSetValueExW(
                key,
                name,
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide.as_ptr() as *const u8,
                    wide.len() * 2,
                )),
            );
        };

        let write_dword = |name: PCWSTR, val: u32| {
            let _ = RegSetValueExW(
                key,
                name,
                Some(0),
                REG_DWORD,
                Some(std::slice::from_raw_parts(
                    &val as *const u32 as *const u8,
                    4,
                )),
            );
        };

        write_str(w!("Source"), src);
        write_str(w!("SourceName"), src_name);
        write_str(w!("Destination"), dst);
        write_str(w!("DestinationName"), dst_name);
        write_dword(w!("VolumeSync"), if vol_sync { 1 } else { 0 });
        write_dword(w!("CushionMs"), cushion_ms);

        let _ = RegCloseKey(key);
    }
}

fn autostart_enabled() -> bool {
    unsafe {
        let mut key = HKEY::default();
        let path = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, Some(0), KEY_QUERY_VALUE, &mut key).is_err() {
            return false;
        }
        let name = HSTRING::from(REG_APP_NAME);
        let ok = RegQueryValueExW(key, &name, None, None, None, None).is_ok();
        let _ = RegCloseKey(key);
        ok
    }
}

fn set_autostart(enable: bool) {
    unsafe {
        let mut key = HKEY::default();
        let path = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, Some(0), KEY_SET_VALUE, &mut key).is_err() {
            return;
        }
        let name = HSTRING::from(REG_APP_NAME);
        if enable {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\"", exe.display());
            let wide: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = RegSetValueExW(
                key,
                &name,
                Some(0),
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide.as_ptr() as *const u8,
                    wide.len() * 2,
                )),
            );
        } else {
            let _ = RegDeleteValueW(key, &name);
        }
        let _ = RegCloseKey(key);
    }
}

// ─── Tray icon & UI ─────────────────────────────────────────────────────────

/// Safe global pointer to Shared (replaces `static mut GLOBAL_SHARED`).
static GLOBAL_SHARED: AtomicPtr<Shared> = AtomicPtr::new(ptr::null_mut());

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            let event = lp.0 as u32 & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_context_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_menu_command(hwnd, (wp.0 & 0xFFFF) as u16);
            LRESULT(0)
        }
        WM_STATUS_UPDATE => {
            // Audio thread status changed — refresh the tooltip.
            let p = GLOBAL_SHARED.load(Ordering::Relaxed);
            if !p.is_null() {
                update_tooltip(hwnd, &*p);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Build and display the right-click context menu at the cursor position.
unsafe fn show_context_menu(hwnd: HWND) {
    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let en: std::result::Result<IMMDeviceEnumerator, _> =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
    let devices = en
        .as_ref()
        .ok()
        .and_then(|e| enumerate_devices(e).ok())
        .unwrap_or_default();

    // ── Source devices ───────────────────────────────────────────────
    let _ = AppendMenuW(menu, MF_GRAYED, 0, w!("── Source ──"));
    let current_src = shared.src.lock().unwrap().clone();
    for (i, (_dev, info)) in devices.iter().enumerate() {
        let flags = MF_STRING
            | if is_match(info, &current_src) {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let _ = AppendMenuW(
            menu,
            flags,
            (IDM_SRC_BASE + i as u16) as usize,
            &HSTRING::from(&info.name),
        );
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ── Destination devices ─────────────────────────────────────────
    let _ = AppendMenuW(menu, MF_GRAYED, 0, w!("── Destination ──"));
    let current_dst = shared.dst.lock().unwrap().clone();
    for (i, (_dev, info)) in devices.iter().enumerate() {
        let flags = MF_STRING
            | if is_match(info, &current_dst) {
                MF_CHECKED
            } else {
                MF_UNCHECKED
            };
        let _ = AppendMenuW(
            menu,
            flags,
            (IDM_DST_BASE + i as u16) as usize,
            &HSTRING::from(&info.name),
        );
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());

    // ── Volume sync toggle ──────────────────────────────────────────
    let vol_flag = if shared.vol_sync.load(Ordering::Relaxed) {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    let _ = AppendMenuW(
        menu,
        MF_STRING | vol_flag,
        IDM_VOLSYNC as usize,
        w!("Volume Sync"),
    );

    // ── Auto-start toggle ───────────────────────────────────────────
    let auto_flag = if autostart_enabled() {
        MF_CHECKED
    } else {
        MF_UNCHECKED
    };
    let _ = AppendMenuW(
        menu,
        MF_STRING | auto_flag,
        IDM_AUTOSTART as usize,
        w!("Start with Windows"),
    );

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, w!("Exit"));

    // Track popup menu with reliable outside-click dismissal
    let mut pt = Default::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu); // Prevent GDI/User handle leak!
}

/// Does `info` match the user's current needle (by exact ID or substring)?
fn is_match(info: &DeviceInfo, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let n = needle.to_lowercase();
    info.id.to_lowercase() == n
        || info.name.to_lowercase().contains(&n)
        || info.id.to_lowercase().contains(&n)
}

/// Handle a WM_COMMAND from the context menu.
unsafe fn handle_menu_command(hwnd: HWND, id: u16) {
    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;

    match id {
        IDM_EXIT => {
            shared.quit.store(true, Ordering::Relaxed);
            shared.signal_wake();
            remove_tray_icon(hwnd);
            let _ = DestroyWindow(hwnd);
        }
        IDM_AUTOSTART => {
            set_autostart(!autostart_enabled());
        }
        IDM_VOLSYNC => {
            let prev = shared.vol_sync.load(Ordering::Relaxed);
            let next = !prev;
            shared.vol_sync.store(next, Ordering::Relaxed);
            save_current_settings(shared);
            shared.restart.store(true, Ordering::Relaxed);
            shared.signal_wake();
        }
        _ if id >= IDM_DST_BASE => {
            // Destination device selection.
            select_device(shared, false, (id - IDM_DST_BASE) as usize);
            save_current_settings(shared);
            update_tooltip(hwnd, shared);
        }
        _ if id >= IDM_SRC_BASE => {
            // Source device selection.
            select_device(shared, true, (id - IDM_SRC_BASE) as usize);
            save_current_settings(shared);
            update_tooltip(hwnd, shared);
        }
        _ => {}
    }
}

unsafe fn save_current_settings(shared: &Shared) {
    let src = shared.src.lock().unwrap().clone();
    let src_name = shared.src_name.lock().unwrap().clone();
    let dst = shared.dst.lock().unwrap().clone();
    let dst_name = shared.dst_name.lock().unwrap().clone();
    let vol_sync = shared.vol_sync.load(Ordering::Relaxed);
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);
    save_persisted_config(&src, &src_name, &dst, &dst_name, vol_sync, cushion_ms);
}

/// Set either the source or destination device to the `idx`-th enumerated device.
unsafe fn select_device(shared: &Shared, is_src: bool, idx: usize) {
    if let Ok(en) =
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
    {
        if let Ok(devices) = enumerate_devices(&en) {
            if let Some((_dev, info)) = devices.get(idx) {
                if is_src {
                    *shared.src.lock().unwrap() = info.id.clone();
                    *shared.src_name.lock().unwrap() = info.name.clone();
                } else {
                    *shared.dst.lock().unwrap() = info.id.clone();
                    *shared.dst_name.lock().unwrap() = info.name.clone();
                }
                shared.restart.store(true, Ordering::Relaxed);
                shared.signal_wake();
            }
        }
    }
}

// ─── Tray icon management ────────────────────────────────────────────────────

unsafe fn add_tray_icon(hwnd: HWND) {
    let icon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
    let mut nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide(&mut nid.szTip, "Monitor Mirror");
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn update_tooltip(hwnd: HWND, shared: &Shared) {
    let src = shared.src_name.lock().unwrap().clone();
    let dst = shared.dst_name.lock().unwrap().clone();

    let status = match shared.status.load(Ordering::Relaxed) {
        STATUS_ACTIVE => "▶ ",
        STATUS_WAITING => "⏳ ",
        _ => "",
    };

    let tip = if src.is_empty() && dst.is_empty() {
        format!("{}Monitor Mirror (no devices selected)", status)
    } else {
        format!(
            "{}Mirror: {} → {}",
            status,
            trunc(&src, 28),
            trunc(&dst, 28)
        )
    };
    let mut nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_TIP,
        ..Default::default()
    };
    copy_wide(&mut nid.szTip, &tip);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

/// Copy a UTF-8 string into a fixed-size u16 buffer (generic helper).
fn copy_wide<const N: usize>(buf: &mut [u16; N], s: &str) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let n = wide.len().min(N);
    buf[..n].copy_from_slice(&wide[..n]);
}

/// Copy into the 256-element `szInfo` buffer.
fn copy_wide_256(buf: &mut [u16; 256], s: &str) {
    copy_wide(buf, s);
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

// ─── CLI parsing ─────────────────────────────────────────────────────────────

struct Cfg {
    src: String,
    dst: String,
    cushion_ms: Option<u32>, // None = not specified on CLI, use registry or default
}

fn parse() -> (bool, Cfg) {
    let mut cfg = Cfg {
        src: String::new(),
        dst: String::new(),
        cushion_ms: None,
    };
    let mut list = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--list" => list = true,
            "--src" => cfg.src = it.next().unwrap_or_default(),
            "--dst" => cfg.dst = it.next().unwrap_or_default(),
            "--cushion" => cfg.cushion_ms = it.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    (list, cfg)
}

fn fatal(msg: &str) -> ! {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(msg),
            w!("monitor-mirror"),
            MB_OK | MB_ICONERROR,
        );
    }
    std::process::exit(1);
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() {
    let (do_list, mut cfg) = parse();

    unsafe {
        // STA for the UI thread (message loop + Shell_NotifyIcon + menus).
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        // --list: print devices to the parent console and exit.
        if do_list {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .unwrap_or_else(|_| fatal("Could not open the audio device enumerator."));
            let _ = list_devices(&en);
            return;
        }

        // Single instance: two mirrors would double the audio.
        let _guard = CreateMutexW(None, true, w!("Local\\MonitorAudioMirror"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // If not specified on CLI, load previously saved device selections from the registry!
        let (saved_src, saved_src_name, saved_dst, saved_dst_name, saved_vol_sync, saved_cushion) =
            load_persisted_config();
        let mut src_name = String::new();
        let mut dst_name = String::new();

        if cfg.src.is_empty() && !saved_src.is_empty() {
            cfg.src = saved_src;
            src_name = saved_src_name;
        }
        if cfg.dst.is_empty() && !saved_dst.is_empty() {
            cfg.dst = saved_dst;
            dst_name = saved_dst_name;
        }

        // Resolve cushion_ms: CLI flag > registry > default (5 ms)
        let cushion_ms = cfg.cushion_ms.unwrap_or_else(|| saved_cushion.unwrap_or(5));

        // If --src/--dst were given on CLI, validate they don't match multiple devices.
        if !cfg.src.is_empty() || !cfg.dst.is_empty() {
            if let Ok(en) =
                CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            {
                for (what, needle) in [("--src", &cfg.src), ("--dst", &cfg.dst)] {
                    if !needle.is_empty() {
                        if let Ok(Found::Many(n)) = find(&en, needle) {
                            fatal(&format!(
                                "{what} \"{needle}\" matches {n} devices. Use the full endpoint ID from --list."
                            ));
                        }
                    }
                }
            }
        }

        // ── Create wake event for instant audio thread signaling ────
        let wake_event =
            CreateEventW(None, false, false, PCWSTR::null()).unwrap_or_default();
        let wake_raw = wake_event.0 as usize;

        // ── Build shared state ──────────────────────────────────────
        let shared = Shared::new(
            cfg.src,
            src_name,
            cfg.dst,
            dst_name,
            cushion_ms,
            saved_vol_sync,
            wake_raw,
        );

        // Store the shared pointer for the window procedure (safe AtomicPtr).
        let shared_raw: *const Shared = &*shared;
        GLOBAL_SHARED.store(shared_raw as *mut Shared, Ordering::Relaxed);

        // ── Create a hidden message-only window for tray icon events ─
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("MonitorMirrorTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        // HWND_MESSAGE = HWND(-3): message-only window, invisible, no taskbar entry.
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("MonitorMirror"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            Some(HWND(-3isize as *mut _)),
            None,
            Some(hinstance.into()),
            None,
        )
        .unwrap_or_default();

        // Store the HWND in Shared before audio thread starts.
        shared.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);

        add_tray_icon(hwnd);
        update_tooltip(hwnd, &shared);

        // ── Spawn audio thread now that window and tray icon exist ───
        let audio_shared = Arc::clone(&shared);
        let audio_handle = thread::spawn(move || audio_thread(audio_shared));

        // ── Win32 message loop ──────────────────────────────────────
        // GetMessageW blocks (yields the thread) until a message arrives,
        // so the UI is completely dormant (0% CPU) when not interacted with.
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // ── Graceful shutdown ───────────────────────────────────────
        shared.quit.store(true, Ordering::Relaxed);
        shared.signal_wake(); // wake audio thread from any WaitForSingleObject
        let _ = audio_handle.join(); // wait for audio thread to exit cleanly
        let _ = CloseHandle(wake_event);
    }
}
