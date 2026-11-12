// monitor-mirror v0.4.0: Ultra-low latency audio mirroring between WASAPI endpoints,
// featuring a system-tray UI with color-coded custom menus, embedded logo icon,
// audio device cycling (SoundSwitch feature) via customizable hotkeys, and favorites.
//
// Subsystem: Windows (no console window).
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]
#![allow(dead_code)]

use std::{
    collections::HashSet,
    ffi::c_void,
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
    core::{implement, w, Error, Interface, Result, HSTRING, PCWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{
            CloseHandle, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, E_FAIL, HANDLE, HWND,
            LPARAM, LRESULT, RECT, WAIT_TIMEOUT, WPARAM,
        },
        Graphics::{
            Gdi::{
                CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, SelectObject,
                SetBkMode, SetTextColor, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
                FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, FW_BOLD,
                FW_NORMAL, HDC, TRANSPARENT,
            },
            GdiPlus::{
                GdiplusStartup, GdiplusStartupInput, GdipCreateBitmapFromStream,
                GdipCreateHICONFromBitmap, GdipDisposeImage,
            },
        },
        Media::Audio::{
            eConsole, eMultimedia, eRender,
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
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
                MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_F11, VK_SCROLL,
            },
            Shell::{
                Shell_NotifyIconW, SHCreateMemStream, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP,
                NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW,
                MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
                TrackPopupMenu, TranslateMessage, HICON, IDI_APPLICATION, MENU_ITEM_FLAGS,
                MF_OWNERDRAW, MF_POPUP, MF_SEPARATOR, MB_ICONERROR, MB_OK, MSG, TPM_BOTTOMALIGN,
                TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_DESTROY,
                WM_DRAWITEM, WM_HOTKEY, WM_LBUTTONUP, WM_MEASUREITEM, WM_NULL, WM_RBUTTONUP,
                WNDCLASSW, WS_POPUP,
            },
        },
    },
};

// ─── External Windows APIs (Direct linking for minimal overhead) ─────────────

extern "system" {
    fn SetProcessWorkingSetSize(hProcess: *mut c_void, dwMinimumWorkingSetSize: usize, dwMaximumWorkingSetSize: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetCurrentThread() -> *mut c_void;
    fn SetThreadPriority(hThread: *mut c_void, nPriority: i32) -> i32;
}

const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

// ─── Embedded Logo Asset (High-DPI 256x256 optimized for tray) ───────────────

const LOGO_PNG: &[u8] = include_bytes!("media/icon_256.png");

// ─── Undocumented COM interface: IPolicyConfig (SoundSwitch backend) ─────────

const CLSID_POLICY_CONFIG: windows::core::GUID =
    windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const CLSID_POLICY_CONFIG_10: windows::core::GUID =
    windows::core::GUID::from_u128(0x294fcbf0_e7f1_46dd_92db_d4e05639ff76);

#[repr(C)]
struct IPolicyConfigVtbl {
    pub parent: windows_core::IUnknown_Vtbl,
    pub GetMixFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> windows_core::HRESULT,
    pub GetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut c_void) -> windows_core::HRESULT,
    pub ResetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR) -> windows_core::HRESULT,
    pub SetDeviceFormat: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void, *mut c_void) -> windows_core::HRESULT,
    pub GetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut c_void, *mut c_void) -> windows_core::HRESULT,
    pub SetProcessingPeriod: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> windows_core::HRESULT,
    pub GetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> windows_core::HRESULT,
    pub SetShareMode: unsafe extern "system" fn(*mut c_void, PCWSTR, *mut c_void) -> windows_core::HRESULT,
    pub GetPropertyValue: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut c_void, *mut c_void) -> windows_core::HRESULT,
    pub SetPropertyValue: unsafe extern "system" fn(*mut c_void, PCWSTR, i32, *mut c_void, *mut c_void) -> windows_core::HRESULT,
    pub SetDefaultEndpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> windows_core::HRESULT,
    pub SetEndpointVisibility: unsafe extern "system" fn(*mut c_void, PCWSTR, i32) -> windows_core::HRESULT,
}

// ─── Owner-Drawn Menu Structs & Constants ───────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MEASUREITEMSTRUCT {
    pub CtlType: u32,
    pub CtlID: u32,
    pub itemID: u32,
    pub itemWidth: u32,
    pub itemHeight: u32,
    pub itemData: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DRAWITEMSTRUCT {
    pub CtlType: u32,
    pub CtlID: u32,
    pub itemID: u32,
    pub itemAction: u32,
    pub itemState: u32,
    pub hwndItem: HWND,
    pub hDC: HDC,
    pub rcItem: RECT,
    pub itemData: usize,
}

pub const ODT_MENU: u32 = 1;
pub const ODS_SELECTED: u32 = 0x0001;
pub const ODS_GRAYED: u32 = 0x0002;
pub const ODS_DISABLED: u32 = 0x0004;
pub const ODS_CHECKED: u32 = 0x0008;

// ─── Constants ───────────────────────────────────────────────────────────────

const BUFFERFLAGS_SILENT: u32 = 0x2;

/// Custom message the tray icon posts to our hidden window.
const WM_TRAYICON: u32 = WM_APP + 1;
/// Custom message the audio thread posts to update the tray status.
const WM_STATUS_UPDATE: u32 = WM_APP + 2;

// Global Hotkey ID for audio output cycling.
const HOTKEY_CYCLE_ID: i32 = 0x7701;

// Context-menu item IDs.
const IDM_EXIT: u16 = 1;
const IDM_AUTOSTART: u16 = 2;
const IDM_VOLSYNC: u16 = 3;
const IDM_CYCLE_DEVICE: u16 = 4;
const IDM_CLEAR_FAVORITES: u16 = 5;

// Hotkey preset IDs (10 .. 19)
const IDM_HOTKEY_BASE: u16 = 10;
const IDM_HOTKEY_CTRL_ALT_A: u16 = 10;
const IDM_HOTKEY_CTRL_SHIFT_A: u16 = 11;
const IDM_HOTKEY_CTRL_ALT_S: u16 = 12;
const IDM_HOTKEY_WIN_ALT_A: u16 = 13;
const IDM_HOTKEY_F11: u16 = 14;
const IDM_HOTKEY_SCROLL: u16 = 15;

const IDM_SRC_BASE: u16 = 1000; // source devices    1000 .. 1999
const IDM_DST_BASE: u16 = 2000; // dest devices      2000 .. 2999
const IDM_FAV_BASE: u16 = 3000; // favorite devices  3000 .. 3999

const REG_APP_NAME: &str = "MonitorMirror";
const REG_SETTINGS_SUBKEY: PCWSTR = w!("Software\\MonitorMirror");

// Audio thread status codes.
const STATUS_IDLE: u8 = 0;
const STATUS_ACTIVE: u8 = 1;
const STATUS_WAITING: u8 = 2;

// ─── Colors (Refined Modern Dark Theme) ──────────────────────────────────────

const CLR_BG: COLORREF = COLORREF(0x00231F1F);        // #1F1F23 deep charcoal
const CLR_BG_HOVER: COLORREF = COLORREF(0x00352E2E);  // #2E2E35
const CLR_BG_SRC_HOVER: COLORREF = COLORREF(0x003B291E); // #1E293B subtle blue glow
const CLR_BG_DST_HOVER: COLORREF = COLORREF(0x0018183B); // #3B1818 subtle red glow

const CLR_TEXT: COLORREF = COLORREF(0x00F0E8E2);      // #E2E8F0 clean crisp text
const CLR_TEXT_MUTED: COLORREF = COLORREF(0x00A0948C);// #8C94A0 muted grey
const CLR_SEPARATOR: COLORREF = COLORREF(0x00403838); // #383840 divider

// Section accents:
const CLR_BLUE_ACCENT: COLORREF = COLORREF(0x00F6823B); // #3B82F6 vibrant blue
const CLR_RED_ACCENT: COLORREF = COLORREF(0x004444EF);  // #EF4444 vibrant red
const CLR_SILVER_ACCENT: COLORREF = COLORREF(0x00E1D5CB);// #CBD5E1 metallic platinum
const CLR_CYAN_ACCENT: COLORREF = COLORREF(0x00D4B606); // #06B6D4 vibrant cyan/teal
const CLR_GOLD_ACCENT: COLORREF = COLORREF(0x000B9EF5); // #F59E0B gold star
const CLR_EXIT_ACCENT: COLORREF = COLORREF(0x007171F8); // #F87171 soft coral

// ─── Menu Item Metadata for Owner-Draw ───────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuItemKind {
    HeaderSource,
    ItemSource,
    HeaderDest,
    ItemDest,
    SoundSwitch,
    FavoritesHeader,
    ItemFavorite,
    ClearFavorites,
    HotkeyOption,
    VolumeSync,
    AutoStart,
    Exit,
    Separator,
}

struct CustomMenuItem {
    id: u16,
    kind: MenuItemKind,
    checked: bool,
    text: String,
    shortcut: String,
}

// ─── Shared state ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DeviceInfo {
    id: String,
    name: String,
}

struct Shared {
    src: Mutex<String>,
    src_name: Mutex<String>,
    dst: Mutex<String>,
    dst_name: Mutex<String>,
    cushion_ms: AtomicU32,
    restart: AtomicBool,
    quit: AtomicBool,
    vol_sync: AtomicBool,
    wake_event: AtomicUsize,
    status: AtomicU8,
    hwnd: AtomicIsize,
    notified: AtomicBool,
    // Favorites set of endpoint IDs
    favorites: Mutex<HashSet<String>>,
    // Hotkey modifiers and key
    hotkey_mod: AtomicU32,
    hotkey_vk: AtomicU32,
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
        favorites: HashSet<String>,
        hotkey_mod: u32,
        hotkey_vk: u32,
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
            favorites: Mutex::new(favorites),
            hotkey_mod: AtomicU32::new(hotkey_mod),
            hotkey_vk: AtomicU32::new(hotkey_vk),
        })
    }

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

    fn signal_wake(&self) {
        let raw = self.wake_event.load(Ordering::Relaxed);
        if raw != 0 {
            unsafe {
                let _ = SetEvent(HANDLE(raw as *mut _));
            }
        }
    }
}

// ─── Image & Icon Loader (GDI+ embedded PNG decoder) ─────────────────────────

unsafe fn create_app_icon() -> HICON {
    let mut token = 0usize;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..Default::default()
    };
    if GdiplusStartup(&mut token, &input, ptr::null_mut()).0 == 0 {
        if let Some(stream) = SHCreateMemStream(Some(LOGO_PNG)) {
            let mut bitmap = ptr::null_mut();
            if GdipCreateBitmapFromStream(&stream, &mut bitmap).0 == 0 && !bitmap.is_null() {
                let mut hicon = HICON::default();
                if GdipCreateHICONFromBitmap(bitmap, &mut hicon).0 == 0 && !hicon.is_invalid() {
                    let _ = GdipDisposeImage(bitmap as *mut _);
                    return hicon;
                }
                let _ = GdipDisposeImage(bitmap as *mut _);
            }
        }
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

// ─── Default Audio Output Device Controller (IPolicyConfig) ───────────────────

unsafe fn set_default_audio_device(endpoint_id: &str) -> bool {
    let wide: Vec<u16> = endpoint_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());

    // Try Windows 10/11 GUID first, fallback to Vista/7/8 GUID
    let mut unk: Option<windows::core::IUnknown> =
        CoCreateInstance(&CLSID_POLICY_CONFIG_10, None, CLSCTX_ALL).ok();
    if unk.is_none() {
        unk = CoCreateInstance(&CLSID_POLICY_CONFIG, None, CLSCTX_ALL).ok();
    }

    if let Some(unk) = unk {
        let this = unk.as_raw();
        let vtbl = *(this as *const *const IPolicyConfigVtbl);
        // Roles: 0 = eConsole (games/apps), 1 = eMultimedia (music/browser), 2 = eCommunications (discord/calls)
        let _ = ((*vtbl).SetDefaultEndpoint)(this, pcwstr, 0);
        let _ = ((*vtbl).SetDefaultEndpoint)(this, pcwstr, 1);
        let _ = ((*vtbl).SetDefaultEndpoint)(this, pcwstr, 2);
        true
    } else {
        false
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

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeCallback {
    dst_vol: IAudioEndpointVolume,
    shared: Arc<Shared>,
    applying: Arc<AtomicBool>,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeCallback_Impl {
    fn OnNotify(&self, data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> Result<()> {
        if !self.shared.vol_sync.load(Ordering::Relaxed) || data.is_null() {
            return Ok(());
        }
        if self.applying.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        unsafe {
            let d = &*data;
            let _ = self
                .dst_vol
                .SetMasterVolumeLevelScalar(d.fMasterVolume, ptr::null());
            let _ = self.dst_vol.SetMute(d.bMuted.into(), ptr::null());

            // Mirror per-channel balance
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

// ─── WASAPI mirror session (Ultra-Low Latency Pipeline) ───────────────────────

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

    *shared.src_name.lock().unwrap() = src_info.name.clone();
    *shared.dst_name.lock().unwrap() = dst_info.name.clone();

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
            150_000, // 15 ms ring buffer (ultra-tight queue, minimal latency)
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
            if shared.vol_sync.load(Ordering::Relaxed) {
                let level = sv.GetMasterVolumeLevelScalar().unwrap_or(1.0);
                let _ = dv.SetMasterVolumeLevelScalar(level, ptr::null());
                let muted = sv.GetMute().unwrap_or_default();
                let _ = dv.SetMute(muted.into(), ptr::null());

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

    if !shared.notified.swap(true, Ordering::Relaxed) {
        show_balloon(shared, "Mirror Active", &format!("{} → {}", trunc(&src_info.name, 40), trunc(&dst_info.name, 40)));
    }

    let result = (|| -> Result<()> {
        let cap: IAudioCaptureClient = sc.GetService()?;
        let rnd: IAudioRenderClient = dc.GetService()?;
        let ring = dc.GetBufferSize()?;

        let (mut period_hns, mut min_hns) = (0i64, 0i64);
        sc.GetDevicePeriod(Some(&mut period_hns), Some(&mut min_hns))?;
        let period = ((rate as i64 * period_hns / 10_000_000) as u32).max(1);
        let cushion = (rate / 1000 * cushion_ms).max(1);
        let high = (cushion + period).min(ring);

        let wake_raw = shared.wake_event.load(Ordering::Relaxed);
        let wake_handle = HANDLE(wake_raw as *mut _);
        let wait_handles = [ev, wake_handle];

        dc.Start()?;
        sc.Start()?;

        loop {
            let w = WaitForMultipleObjects(&wait_handles, false, 250);

            // Instant wake-up check
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

unsafe fn show_balloon(shared: &Shared, title: &str, body: &str) {
    let h = shared.hwnd.load(Ordering::Relaxed);
    if h == 0 {
        return;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(h as *mut _),
        uID: 1,
        uFlags: NIF_INFO,
        dwInfoFlags: NOTIFY_ICON_INFOTIP_FLAGS(0x11), // NIIF_INFO | NIIF_NOSOUND
        ..Default::default()
    };
    copy_wide(&mut nid.szInfoTitle, title);
    copy_wide_256(&mut nid.szInfo, body);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

fn audio_thread(shared: Arc<Shared>) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let mut task_idx = 0u32;
        let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_idx);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);

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

            if shared.restart.swap(false, Ordering::Relaxed) {
                continue;
            }

            shared.set_status(STATUS_WAITING);
            let wake_raw = shared.wake_event.load(Ordering::Relaxed);
            if wake_raw != 0 {
                let wake_handle = HANDLE(wake_raw as *mut _);
                let _ = WaitForSingleObject(wake_handle, 2000);
            } else {
                thread::sleep(Duration::from_secs(2));
            }
        }

        shared.set_status(STATUS_IDLE);
    }
}

// ─── Sound Switch Feature (Audio Output Cycling with Favorites) ──────────────

unsafe fn cycle_audio_device(hwnd: HWND) {
    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;

    let en: IMMDeviceEnumerator = match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
        Ok(e) => e,
        Err(_) => return,
    };
    let devices = match enumerate_devices(&en) {
        Ok(d) if !d.is_empty() => d,
        _ => return,
    };

    let favorites = shared.favorites.lock().unwrap().clone();

    // Determine candidate list: if favorites exist, cycle ONLY among favorites; else all devices.
    let fav_candidates: Vec<&(IMMDevice, DeviceInfo)> =
        devices.iter().filter(|d| favorites.contains(&d.1.id)).collect();
    let candidates: Vec<&(IMMDevice, DeviceInfo)> = if !fav_candidates.is_empty() {
        fav_candidates
    } else {
        devices.iter().collect()
    };

    if candidates.is_empty() {
        return;
    }

    // Determine current default endpoint ID
    let current_default_id = en
        .GetDefaultAudioEndpoint(eRender, eMultimedia)
        .or_else(|_| en.GetDefaultAudioEndpoint(eRender, eConsole))
        .ok()
        .and_then(|d| describe(&d).ok())
        .map(|info| info.id)
        .unwrap_or_else(|| shared.src.lock().unwrap().clone());

    let cur_idx = candidates
        .iter()
        .position(|d| d.1.id == current_default_id)
        .unwrap_or(0);
    let next_idx = (cur_idx + 1) % candidates.len();
    let next_device = candidates[next_idx];

    // Set Windows default audio endpoint
    set_default_audio_device(&next_device.1.id);

    // Update Monitor Mirror source to the new primary audio device
    {
        let mut src_guard = shared.src.lock().unwrap();
        let mut src_name_guard = shared.src_name.lock().unwrap();
        let mut dst_guard = shared.dst.lock().unwrap();
        let mut dst_name_guard = shared.dst_name.lock().unwrap();

        let old_src = src_guard.clone();
        let old_src_name = src_name_guard.clone();

        *src_guard = next_device.1.id.clone();
        *src_name_guard = next_device.1.name.clone();

        // Avoid self-mirror loop: if new src matches dst, switch dst to the old src or another device!
        if *src_guard == *dst_guard {
            if !old_src.is_empty() && old_src != *src_guard {
                *dst_guard = old_src;
                *dst_name_guard = old_src_name;
            } else if let Some(alt) = devices.iter().find(|d| d.1.id != *src_guard) {
                *dst_guard = alt.1.id.clone();
                *dst_name_guard = alt.1.name.clone();
            }
        }
    }

    save_current_settings(shared);
    shared.restart.store(true, Ordering::Relaxed);
    shared.signal_wake();

    update_tooltip(hwnd, shared);
    show_balloon(shared, "Audio Output Switched", &format!("Default Output: {}", next_device.1.name));
}

// ─── Registry: Settings Persistence & Auto-Start ────────────────────────────

fn load_persisted_config() -> (String, String, String, String, bool, Option<u32>, HashSet<String>, u32, u32) {
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
                HashSet::new(),
                MOD_CONTROL.0 | MOD_ALT.0,
                0x41, // 'A'
            );
        }

        let read_str = |name: PCWSTR| -> String {
            let mut buf = [0u8; 1024];
            let mut len = buf.len() as u32;
            if RegQueryValueExW(key, name, None, None, Some(buf.as_mut_ptr()), Some(&mut len)).is_ok()
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

        let fav_str = read_str(w!("Favorites"));
        let favorites: HashSet<String> = fav_str
            .split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        let hotkey_mod = read_dword(w!("HotkeyMod")).unwrap_or(MOD_CONTROL.0 | MOD_ALT.0);
        let hotkey_vk = read_dword(w!("HotkeyKey")).unwrap_or(0x41);

        let _ = RegCloseKey(key);
        (src, src_name, dst, dst_name, vol_sync, cushion_ms, favorites, hotkey_mod, hotkey_vk)
    }
}

fn save_persisted_config(
    src: &str,
    src_name: &str,
    dst: &str,
    dst_name: &str,
    vol_sync: bool,
    cushion_ms: u32,
    favorites: &HashSet<String>,
    hotkey_mod: u32,
    hotkey_vk: u32,
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

        let fav_joined = favorites.iter().cloned().collect::<Vec<_>>().join(";");
        write_str(w!("Favorites"), &fav_joined);

        write_dword(w!("HotkeyMod"), hotkey_mod);
        write_dword(w!("HotkeyKey"), hotkey_vk);

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

// ─── Tray Icon & Color-Coded Right-Click Menu ────────────────────────────────

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
        WM_HOTKEY => {
            if wp.0 == HOTKEY_CYCLE_ID as usize {
                cycle_audio_device(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            handle_menu_command(hwnd, (wp.0 & 0xFFFF) as u16);
            LRESULT(0)
        }
        WM_MEASUREITEM => {
            let mis = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
            if mis.CtlType == ODT_MENU && mis.itemData != 0 {
                let item = &*(mis.itemData as *const CustomMenuItem);
                match item.kind {
                    MenuItemKind::Separator => {
                        mis.itemHeight = 9;
                        mis.itemWidth = 100;
                    }
                    MenuItemKind::HeaderSource | MenuItemKind::HeaderDest | MenuItemKind::FavoritesHeader => {
                        mis.itemHeight = 24;
                        mis.itemWidth = 240;
                    }
                    _ => {
                        mis.itemHeight = 26;
                        let text_len = (item.text.chars().count() * 7 + item.shortcut.chars().count() * 7 + 70) as u32;
                        mis.itemWidth = text_len.max(260);
                    }
                }
            }
            LRESULT(1)
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlType == ODT_MENU && dis.itemData != 0 {
                let item = &*(dis.itemData as *const CustomMenuItem);
                draw_menu_item(dis, item);
            }
            LRESULT(1)
        }
        WM_STATUS_UPDATE => {
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

unsafe fn draw_menu_item(dis: &DRAWITEMSTRUCT, item: &CustomMenuItem) {
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let selected = (dis.itemState & ODS_SELECTED) != 0;
    let disabled = (dis.itemState & ODS_GRAYED) != 0 || (dis.itemState & ODS_DISABLED) != 0;

    // Background color selection
    let bg_color = match item.kind {
        MenuItemKind::ItemSource if selected => CLR_BG_SRC_HOVER,
        MenuItemKind::ItemDest if selected => CLR_BG_DST_HOVER,
        _ if selected => CLR_BG_HOVER,
        _ => CLR_BG,
    };

    let bg_brush = CreateSolidBrush(bg_color);
    FillRect(hdc, &rc, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    if item.kind == MenuItemKind::Separator {
        let sep_brush = CreateSolidBrush(CLR_SEPARATOR);
        let sep_rect = RECT {
            left: rc.left + 12,
            top: rc.top + 4,
            right: rc.right - 12,
            bottom: rc.top + 5,
        };
        FillRect(hdc, &sep_rect, sep_brush);
        let _ = DeleteObject(sep_brush.into());
        return;
    }

    SetBkMode(hdc, TRANSPARENT);

    // Font selection: bold for headers and active device
    let font_weight = match item.kind {
        MenuItemKind::HeaderSource | MenuItemKind::HeaderDest | MenuItemKind::FavoritesHeader => FW_BOLD,
        MenuItemKind::ItemSource | MenuItemKind::ItemDest if item.checked => FW_BOLD,
        MenuItemKind::SoundSwitch => FW_BOLD,
        _ => FW_NORMAL,
    };
    let font = CreateFontW(
        15, 0, 0, 0, font_weight.0 as i32, 0, 0, 0,
        FONT_CHARSET(0), FONT_OUTPUT_PRECISION(0), FONT_CLIP_PRECISION(0), FONT_QUALITY(5), 0,
        w!("Segoe UI"),
    );
    let old_font = SelectObject(hdc, font.into());

    // Left accent pill or indicator
    match item.kind {
        MenuItemKind::HeaderSource => {
            let pill_brush = CreateSolidBrush(CLR_BLUE_ACCENT);
            let pill_rect = RECT {
                left: rc.left + 4,
                top: rc.top + 5,
                right: rc.left + 7,
                bottom: rc.bottom - 5,
            };
            FillRect(hdc, &pill_rect, pill_brush);
            let _ = DeleteObject(pill_brush.into());
        }
        MenuItemKind::HeaderDest => {
            let pill_brush = CreateSolidBrush(CLR_RED_ACCENT);
            let pill_rect = RECT {
                left: rc.left + 4,
                top: rc.top + 5,
                right: rc.left + 7,
                bottom: rc.bottom - 5,
            };
            FillRect(hdc, &pill_rect, pill_brush);
            let _ = DeleteObject(pill_brush.into());
        }
        MenuItemKind::SoundSwitch => {
            let pill_brush = CreateSolidBrush(CLR_CYAN_ACCENT);
            let pill_rect = RECT {
                left: rc.left + 4,
                top: rc.top + 5,
                right: rc.left + 7,
                bottom: rc.bottom - 5,
            };
            FillRect(hdc, &pill_rect, pill_brush);
            let _ = DeleteObject(pill_brush.into());
        }
        _ => {}
    }

    // Checkmark or star glyph drawing
    if item.checked {
        let check_color = match item.kind {
            MenuItemKind::ItemSource => CLR_BLUE_ACCENT,
            MenuItemKind::ItemDest => CLR_RED_ACCENT,
            MenuItemKind::ItemFavorite => CLR_GOLD_ACCENT,
            MenuItemKind::VolumeSync => CLR_SILVER_ACCENT,
            _ => CLR_TEXT,
        };
        SetTextColor(hdc, check_color);
        let mut check_glyph = if item.kind == MenuItemKind::ItemFavorite {
            [0x2605u16, 0] // ★
        } else {
            [0x2713u16, 0] // ✓
        };
        let mut check_rect = RECT {
            left: rc.left + 8,
            top: rc.top,
            right: rc.left + 24,
            bottom: rc.bottom,
        };
        DrawTextW(hdc, &mut check_glyph, &mut check_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // Text color
    let text_color = if disabled {
        CLR_TEXT_MUTED
    } else {
        match item.kind {
            MenuItemKind::HeaderSource => CLR_BLUE_ACCENT,
            MenuItemKind::HeaderDest => CLR_RED_ACCENT,
            MenuItemKind::SoundSwitch => CLR_CYAN_ACCENT,
            MenuItemKind::FavoritesHeader | MenuItemKind::ItemFavorite => CLR_GOLD_ACCENT,
            MenuItemKind::VolumeSync => CLR_SILVER_ACCENT,
            MenuItemKind::Exit => CLR_EXIT_ACCENT,
            MenuItemKind::ItemSource if item.checked => CLR_TEXT,
            MenuItemKind::ItemDest if item.checked => CLR_TEXT,
            _ => CLR_TEXT,
        }
    };
    SetTextColor(hdc, text_color);

    // Main text
    let mut text_rect = RECT {
        left: rc.left + 28,
        top: rc.top,
        right: rc.right - 60,
        bottom: rc.bottom,
    };
    let mut wide_text: Vec<u16> = item.text.encode_utf16().chain(std::iter::once(0)).collect();
    DrawTextW(hdc, &mut wide_text, &mut text_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    // Shortcut label (right aligned)
    if !item.shortcut.is_empty() {
        SetTextColor(hdc, CLR_TEXT_MUTED);
        let mut sc_rect = RECT {
            left: rc.right - 90,
            top: rc.top,
            right: rc.right - 14,
            bottom: rc.bottom,
        };
        let mut wide_sc: Vec<u16> = item.shortcut.encode_utf16().chain(std::iter::once(0)).collect();
        DrawTextW(hdc, &mut wide_sc, &mut sc_rect, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
    }

    SelectObject(hdc, old_font);
    let _ = DeleteObject(font.into());
}

fn get_hotkey_description(mod_code: u32, vk: u32) -> String {
    let mut parts = Vec::new();
    if mod_code & MOD_CONTROL.0 != 0 {
        parts.push("Ctrl");
    }
    if mod_code & MOD_WIN.0 != 0 {
        parts.push("Win");
    }
    if mod_code & MOD_ALT.0 != 0 {
        parts.push("Alt");
    }
    if mod_code & MOD_SHIFT.0 != 0 {
        parts.push("Shift");
    }
    let key_str = match vk {
        0x41 => "A".to_string(),
        0x53 => "S".to_string(),
        0x7A => "F11".to_string(),
        0x91 => "ScrollLock".to_string(),
        k => format!("0x{:X}", k),
    };
    parts.push(&key_str);
    parts.join("+")
}

unsafe fn push_custom_item(
    items: &mut Vec<Box<CustomMenuItem>>,
    menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    id: u16,
    kind: MenuItemKind,
    checked: bool,
    text: &str,
    shortcut: &str,
) {
    let boxed = Box::new(CustomMenuItem {
        id,
        kind,
        checked,
        text: text.to_string(),
        shortcut: shortcut.to_string(),
    });
    let raw_ptr = boxed.as_ref() as *const CustomMenuItem as usize;
    items.push(boxed);
    let flags = MF_OWNERDRAW
        | if kind == MenuItemKind::Separator {
            MF_SEPARATOR
        } else {
            MENU_ITEM_FLAGS(0)
        };
    let _ = AppendMenuW(menu, flags, id as usize, PCWSTR(raw_ptr as *const u16));
}

unsafe fn push_custom_popup(
    items: &mut Vec<Box<CustomMenuItem>>,
    parent_menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    sub_menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    kind: MenuItemKind,
    checked: bool,
    text: &str,
) {
    let boxed = Box::new(CustomMenuItem {
        id: 0,
        kind,
        checked,
        text: text.to_string(),
        shortcut: "▶".to_string(),
    });
    let raw_ptr = boxed.as_ref() as *const CustomMenuItem as usize;
    items.push(boxed);
    let _ = AppendMenuW(
        parent_menu,
        MF_POPUP | MF_OWNERDRAW,
        sub_menu.0 as usize,
        PCWSTR(raw_ptr as *const u16),
    );
}

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

    // Storage for custom menu item allocations during modal TrackPopupMenu
    let mut items: Vec<Box<CustomMenuItem>> = Vec::new();

    // ── 1. SOURCE SECTION (BLUE) ──────────────────────────────────────────
    push_custom_item(&mut items, menu, 0, MenuItemKind::HeaderSource, false, "SOURCE (Capture Output)", "");
    let current_src = shared.src.lock().unwrap().clone();
    for (i, (_dev, info)) in devices.iter().enumerate() {
        let is_chk = is_match(info, &current_src);
        push_custom_item(&mut items, menu, IDM_SRC_BASE + i as u16, MenuItemKind::ItemSource, is_chk, &info.name, "");
    }

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 2. DESTINATION SECTION (RED) ──────────────────────────────────────
    push_custom_item(&mut items, menu, 0, MenuItemKind::HeaderDest, false, "DESTINATION (Mirror Target)", "");
    let current_dst = shared.dst.lock().unwrap().clone();
    for (i, (_dev, info)) in devices.iter().enumerate() {
        let is_chk = is_match(info, &current_dst);
        push_custom_item(&mut items, menu, IDM_DST_BASE + i as u16, MenuItemKind::ItemDest, is_chk, &info.name, "");
    }

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 3. SOUND SWITCH / CYCLE AUDIO OUTPUT ──────────────────────────────
    let cur_hotkey_desc = get_hotkey_description(
        shared.hotkey_mod.load(Ordering::Relaxed),
        shared.hotkey_vk.load(Ordering::Relaxed),
    );
    push_custom_item(
        &mut items,
        menu,
        IDM_CYCLE_DEVICE,
        MenuItemKind::SoundSwitch,
        false,
        "Cycle Audio Output",
        &cur_hotkey_desc,
    );

    // ── 4. FAVORITES SUBMENU (GOLD) ───────────────────────────────────────
    let fav_menu = CreatePopupMenu().unwrap_or_default();
    let favorites = shared.favorites.lock().unwrap().clone();
    for (i, (_dev, info)) in devices.iter().enumerate() {
        let is_fav = favorites.contains(&info.id);
        push_custom_item(
            &mut items,
            fav_menu,
            IDM_FAV_BASE + i as u16,
            MenuItemKind::ItemFavorite,
            is_fav,
            &info.name,
            if is_fav { "★" } else { "" },
        );
    }
    push_custom_item(&mut items, fav_menu, 0, MenuItemKind::Separator, false, "", "");
    push_custom_item(
        &mut items,
        fav_menu,
        IDM_CLEAR_FAVORITES,
        MenuItemKind::ClearFavorites,
        false,
        "Clear All Favorites (Cycle all)",
        "",
    );

    let fav_label = if favorites.is_empty() { "★ Favorites (None set)" } else { "★ Favorites" };
    push_custom_popup(&mut items, menu, fav_menu, MenuItemKind::FavoritesHeader, !favorites.is_empty(), fav_label);

    // ── 5. HOTKEY PRESETS SUBMENU ─────────────────────────────────────────
    let hk_menu = CreatePopupMenu().unwrap_or_default();
    let cur_mod = shared.hotkey_mod.load(Ordering::Relaxed);
    let cur_vk = shared.hotkey_vk.load(Ordering::Relaxed);

    let presets = [
        (IDM_HOTKEY_CTRL_ALT_A, MOD_CONTROL.0 | MOD_ALT.0, 0x41, "Ctrl + Alt + A (Default)"),
        (IDM_HOTKEY_CTRL_SHIFT_A, MOD_CONTROL.0 | MOD_SHIFT.0, 0x41, "Ctrl + Shift + A"),
        (IDM_HOTKEY_CTRL_ALT_S, MOD_CONTROL.0 | MOD_ALT.0, 0x53, "Ctrl + Alt + S"),
        (IDM_HOTKEY_WIN_ALT_A, MOD_WIN.0 | MOD_ALT.0, 0x41, "Win + Alt + A"),
        (IDM_HOTKEY_F11, 0, VK_F11.0 as u32, "F11"),
        (IDM_HOTKEY_SCROLL, 0, VK_SCROLL.0 as u32, "Scroll Lock"),
    ];

    for (id, m, k, desc) in presets {
        let is_sel = cur_mod == m && cur_vk == k;
        push_custom_item(&mut items, hk_menu, id, MenuItemKind::HotkeyOption, is_sel, desc, "");
    }

    push_custom_popup(&mut items, menu, hk_menu, MenuItemKind::SoundSwitch, false, "Switch Hotkey");

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 6. VOLUME SYNC (METALLIC SILVER) ──────────────────────────────────
    let vol_flag = shared.vol_sync.load(Ordering::Relaxed);
    push_custom_item(&mut items, menu, IDM_VOLSYNC, MenuItemKind::VolumeSync, vol_flag, "Volume Synchronization", "");

    // ── 7. START WITH WINDOWS ─────────────────────────────────────────────
    let auto_flag = autostart_enabled();
    push_custom_item(&mut items, menu, IDM_AUTOSTART, MenuItemKind::AutoStart, auto_flag, "Start with Windows", "");

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 8. EXIT ───────────────────────────────────────────────────────────
    push_custom_item(&mut items, menu, IDM_EXIT, MenuItemKind::Exit, false, "Exit", "");

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

    let _ = DestroyMenu(fav_menu);
    let _ = DestroyMenu(hk_menu);
    let _ = DestroyMenu(menu);
}

fn is_match(info: &DeviceInfo, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let n = needle.to_lowercase();
    info.id.to_lowercase() == n
        || info.name.to_lowercase().contains(&n)
        || info.id.to_lowercase().contains(&n)
}

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
        IDM_CYCLE_DEVICE => {
            cycle_audio_device(hwnd);
        }
        IDM_CLEAR_FAVORITES => {
            shared.favorites.lock().unwrap().clear();
            save_current_settings(shared);
            show_balloon(shared, "Favorites Cleared", "Audio switcher will cycle through all active devices.");
        }
        _ if id >= IDM_HOTKEY_BASE && id <= IDM_HOTKEY_SCROLL => {
            let (m, k) = match id {
                IDM_HOTKEY_CTRL_ALT_A => (MOD_CONTROL.0 | MOD_ALT.0, 0x41),
                IDM_HOTKEY_CTRL_SHIFT_A => (MOD_CONTROL.0 | MOD_SHIFT.0, 0x41),
                IDM_HOTKEY_CTRL_ALT_S => (MOD_CONTROL.0 | MOD_ALT.0, 0x53),
                IDM_HOTKEY_WIN_ALT_A => (MOD_WIN.0 | MOD_ALT.0, 0x41),
                IDM_HOTKEY_F11 => (0, VK_F11.0 as u32),
                IDM_HOTKEY_SCROLL => (0, VK_SCROLL.0 as u32),
                _ => (MOD_CONTROL.0 | MOD_ALT.0, 0x41),
            };
            shared.hotkey_mod.store(m, Ordering::Relaxed);
            shared.hotkey_vk.store(k, Ordering::Relaxed);
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_CYCLE_ID);
            let _ = RegisterHotKey(Some(hwnd), HOTKEY_CYCLE_ID, HOT_KEY_MODIFIERS(m | MOD_NOREPEAT.0), k);
            save_current_settings(shared);
            show_balloon(shared, "Hotkey Updated", &format!("Sound Switch hotkey set to {}", get_hotkey_description(m, k)));
        }
        _ if id >= IDM_FAV_BASE => {
            toggle_favorite(shared, (id - IDM_FAV_BASE) as usize);
            save_current_settings(shared);
        }
        _ if id >= IDM_DST_BASE => {
            select_device(shared, false, (id - IDM_DST_BASE) as usize);
            save_current_settings(shared);
            update_tooltip(hwnd, shared);
        }
        _ if id >= IDM_SRC_BASE => {
            select_device(shared, true, (id - IDM_SRC_BASE) as usize);
            save_current_settings(shared);
            update_tooltip(hwnd, shared);
        }
        _ => {}
    }
}

unsafe fn toggle_favorite(shared: &Shared, idx: usize) {
    if let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
        if let Ok(devices) = enumerate_devices(&en) {
            if let Some((_dev, info)) = devices.get(idx) {
                let mut favs = shared.favorites.lock().unwrap();
                if favs.contains(&info.id) {
                    favs.remove(&info.id);
                } else {
                    favs.insert(info.id.clone());
                }
            }
        }
    }
}

unsafe fn save_current_settings(shared: &Shared) {
    let src = shared.src.lock().unwrap().clone();
    let src_name = shared.src_name.lock().unwrap().clone();
    let dst = shared.dst.lock().unwrap().clone();
    let dst_name = shared.dst_name.lock().unwrap().clone();
    let vol_sync = shared.vol_sync.load(Ordering::Relaxed);
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);
    let favorites = shared.favorites.lock().unwrap().clone();
    let hotkey_mod = shared.hotkey_mod.load(Ordering::Relaxed);
    let hotkey_vk = shared.hotkey_vk.load(Ordering::Relaxed);

    save_persisted_config(&src, &src_name, &dst, &dst_name, vol_sync, cushion_ms, &favorites, hotkey_mod, hotkey_vk);
}

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
    let icon = create_app_icon();
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

fn copy_wide<const N: usize>(buf: &mut [u16; N], s: &str) {
    let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let n = wide.len().min(N);
    buf[..n].copy_from_slice(&wide[..n]);
}

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
    cushion_ms: Option<u32>,
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
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        if do_list {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .unwrap_or_else(|_| fatal("Could not open the audio device enumerator."));
            let _ = list_devices(&en);
            return;
        }

        // Single instance guard
        let _guard = CreateMutexW(None, true, w!("Local\\MonitorAudioMirror"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // Load persisted registry settings
        let (saved_src, saved_src_name, saved_dst, saved_dst_name, saved_vol_sync, saved_cushion, favorites, hotkey_mod, hotkey_vk) =
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

        // Default cushion is 3 ms (ultra-low latency)
        let cushion_ms = cfg.cushion_ms.unwrap_or_else(|| saved_cushion.unwrap_or(3));

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

        let wake_event = CreateEventW(None, false, false, PCWSTR::null()).unwrap_or_default();
        let wake_raw = wake_event.0 as usize;

        let shared = Shared::new(
            cfg.src,
            src_name,
            cfg.dst,
            dst_name,
            cushion_ms,
            saved_vol_sync,
            wake_raw,
            favorites,
            hotkey_mod,
            hotkey_vk,
        );

        let shared_raw: *const Shared = &*shared;
        GLOBAL_SHARED.store(shared_raw as *mut Shared, Ordering::Relaxed);

        // Hidden message window for tray events and hotkeys
        let hinstance = GetModuleHandleW(None).unwrap_or_default();
        let class_name = w!("MonitorMirrorTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("MonitorMirror"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .unwrap_or_default();

        shared.hwnd.store(hwnd.0 as isize, Ordering::Relaxed);

        // Register Global Hotkey for audio output switching
        let _ = RegisterHotKey(
            Some(hwnd),
            HOTKEY_CYCLE_ID,
            HOT_KEY_MODIFIERS(hotkey_mod | MOD_NOREPEAT.0),
            hotkey_vk,
        );

        add_tray_icon(hwnd);
        update_tooltip(hwnd, &shared);

        // Spawn audio thread
        let audio_shared = Arc::clone(&shared);
        let audio_handle = thread::spawn(move || audio_thread(audio_shared));

        // Trim process working set memory: releases cold startup pages, dropping resident RAM to ~1.5 MB!
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);

        // Win32 message loop (dormant when idle, 0.00% CPU)
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup and graceful exit
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_CYCLE_ID);
        shared.quit.store(true, Ordering::Relaxed);
        shared.signal_wake();
        let _ = audio_handle.join();
        let _ = CloseHandle(wake_event);
    }
}
