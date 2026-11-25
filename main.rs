// monitor-mirror v0.5.0: Ultra-low latency audio mirroring between WASAPI endpoints,
// featuring a system-tray UI with color-coded dark menus, embedded high-DPI logo icon,
// audio device cycling (SoundSwitch feature) via customizable hotkeys, and non-dismissing favorites.
//
// Subsystem: Windows (no console window).
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

use std::{
    cell::RefCell,
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
    core::{implement, w, Error, Result, HSTRING, PCSTR, PCWSTR},
    Win32::{
        Devices::FunctionDiscovery::PKEY_Device_FriendlyName,
        Foundation::{
            CloseHandle, GetLastError, COLORREF, ERROR_ALREADY_EXISTS, E_FAIL, HANDLE, HWND,
            LPARAM, LRESULT, POINT, PROPERTYKEY, RECT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
        },
        Graphics::Gdi::{
            CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetStockObject,
            InvalidateRect, ScreenToClient, SelectObject, SetBkMode, SetDCBrushColor, SetTextColor,
            UpdateWindow, DC_BRUSH, DT_END_ELLIPSIS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER,
            FONT_CHARSET, FONT_CLIP_PRECISION, FONT_OUTPUT_PRECISION, FONT_QUALITY, FW_BOLD,
            FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONEAREST,
            MonitorFromPoint, TRANSPARENT,
        },
        Media::Audio::{
            eConsole, eMultimedia, eRender, EDataFlow, ERole, DEVICE_STATE,
            Endpoints::{
                IAudioEndpointVolume, IAudioEndpointVolumeCallback,
                IAudioEndpointVolumeCallback_Impl,
            },
            IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
            IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
            AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
            AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIO_VOLUME_NOTIFICATION_DATA,
            DEVICE_STATE_ACTIVE, WAVEFORMATEX,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                StructuredStorage::PropVariantToStringAlloc, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
                COINIT_MULTITHREADED, STGM_READ,
            },
            Console::{AttachConsole, ATTACH_PARENT_PROCESS},
            LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW},
            Registry::{
                RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
                RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
                REG_DWORD, REG_SZ,
            },
            Threading::{
                AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
                CreateMutexW, ResetEvent, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
                INFINITE,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{
                RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
                MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_F11, VK_SCROLL,
            },
            Shell::{
                Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
                NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_INFOTIP_FLAGS,
            },
            WindowsAndMessaging::{
                AppendMenuW, CallNextHookEx, CreateIconFromResourceEx, CreatePopupMenu,
                CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
                DispatchMessageW, FindWindowExW, GetCursorPos, GetMessageW, GetWindowLongPtrW,
                LoadIconW, MenuItemFromPoint, MessageBoxW, PostMessageW, PostQuitMessage,
                RegisterClassW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
                SetWindowsHookExW, TrackPopupMenu, TranslateMessage, UnhookWindowsHookEx,
                GWL_EXSTYLE, HHOOK, HICON, HMENU, IDI_APPLICATION, LR_DEFAULTCOLOR, MENU_ITEM_FLAGS,
                MF_OWNERDRAW, MF_POPUP, MF_SEPARATOR, MB_ICONERROR, MB_OK, MSG, SWP_FRAMECHANGED,
                SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, TPM_BOTTOMALIGN,
                TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WH_MSGFILTER, WM_APP, WM_COMMAND,
                WM_DESTROY, WM_DRAWITEM, WM_HOTKEY, WM_INITMENUPOPUP, WM_LBUTTONDOWN,
                WM_LBUTTONUP, WM_MBUTTONUP, WM_MEASUREITEM, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
                WS_EX_DLGMODALFRAME, WS_EX_WINDOWEDGE, WS_POPUP,
            },
        },
    },
};

#[windows_core::implement(IMMNotificationClient)]
struct DeviceNotifier {
    shared: Arc<Shared>,
}

impl IMMNotificationClient_Impl for DeviceNotifier_Impl {
    fn OnDeviceStateChanged(&self, _device_id: &PCWSTR, _new_state: DEVICE_STATE) -> Result<()> {
        self.shared.restart.store(true, Ordering::Relaxed);
        self.shared.signal_wake();
        Ok(())
    }
    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> Result<()> {
        self.shared.restart.store(true, Ordering::Relaxed);
        self.shared.signal_wake();
        Ok(())
    }
    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> Result<()> {
        self.shared.restart.store(true, Ordering::Relaxed);
        self.shared.signal_wake();
        Ok(())
    }
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, default_device_id: &PCWSTR) -> Result<()> {
        if flow == eRender && (role == eMultimedia || role == eConsole) {
            let def_str = if !default_device_id.is_null() {
                unsafe { default_device_id.to_string().unwrap_or_default() }
            } else {
                String::new()
            };
            if let Some(is_src) = self.shared.matches_target_src(&def_str) {
                let new_paused = !is_src || self.shared.manual_paused.load(Ordering::Relaxed);
                let prev_paused = self.shared.paused.swap(new_paused, Ordering::Release);
                // Only restart WASAPI & notify UI when active/paused state actually changes
                // (avoids redundant restarts when eConsole and eMultimedia fire together).
                if prev_paused != new_paused {
                    self.shared.trigger_restart();
                    let hwnd_raw = self.shared.hwnd.load(Ordering::Relaxed);
                    if hwnd_raw != 0 {
                        unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd_raw as *mut _)),
                                WM_STATUS_UPDATE,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn OnPropertyValueChanged(&self, _device_id: &PCWSTR, _key: &PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn SetProcessWorkingSetSize(hProcess: *mut c_void, dwMinimumWorkingSetSize: usize, dwMaximumWorkingSetSize: usize) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
    fn GetCurrentThread() -> *mut c_void;
    fn GetCurrentThreadId() -> u32;
    fn SetThreadPriority(hThread: *mut c_void, nPriority: i32) -> i32;
}

#[allow(dead_code)]
#[link(name = "user32")]
extern "system" {
    fn SetProcessDpiAwarenessContext(value: isize) -> i32;
    fn SetMenuInfo(hMenu: HMENU, lpcmi: *const MENUINFO) -> windows::core::BOOL;
    fn FindWindowW(lpClassName: PCWSTR, lpWindowName: PCWSTR) -> HWND;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: HWND, dwAttribute: u32, pvAttribute: *const c_void, cbAttribute: u32) -> windows_core::HRESULT;
}

const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

// ─── Embedded Logo Asset (High-DPI 256x256 optimized for tray) ───────────────

const LOGO_PNG: &[u8] = include_bytes!("media/icon_256.png");

// ─── Undocumented COM interface: IPolicyConfig (SoundSwitch backend) ─────────

const CLSID_POLICY_CONFIG: windows::core::GUID =
    windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: windows::core::GUID =
    windows::core::GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
const IID_POLICY_CONFIG_VISTA: windows::core::GUID =
    windows::core::GUID::from_u128(0x568b9108_44bf_40b4_9006_86afe5b5a620);

#[allow(dead_code)]
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

#[link(name = "ole32")]
extern "system" {
    #[link_name = "CoCreateInstance"]
    fn CoCreateInstanceRaw(
        rclsid: *const windows::core::GUID,
        punkouter: *mut c_void,
        dwclscontext: u32,
        riid: *const windows::core::GUID,
        ppv: *mut *mut c_void,
    ) -> windows_core::HRESULT;
}

// ─── Owner-Drawn Menu Structs & Constants ───────────────────────────────────

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[repr(C)]
pub struct MENUINFO {
    pub cbSize: u32,
    pub fMask: u32,
    pub dwStyle: u32,
    pub cyMax: u32,
    pub hbrBack: HBRUSH,
    pub dwContextHelpID: u32,
    pub dwMenuData: usize,
}

pub const MIM_BACKGROUND: u32 = 0x00000002;
pub const MIM_APPLYTOSUBMENUS: u32 = 0x80000000;

pub const ODT_MENU: u32 = 1;
pub const ODS_SELECTED: u32 = 0x0001;
pub const ODS_GRAYED: u32 = 0x0002;
pub const ODS_DISABLED: u32 = 0x0004;

// ─── Constants ───────────────────────────────────────────────────────────────

const APP_VERSION: &str = "0.5.0";
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
const IDM_PAUSE_MIRROR: u16 = 6;

// Hotkey preset IDs (10 .. 15)
const IDM_HOTKEY_BASE: u16 = 10;
const IDM_HOTKEY_CTRL_ALT_A: u16 = 10;
const IDM_HOTKEY_CTRL_SHIFT_A: u16 = 11;
const IDM_HOTKEY_CTRL_ALT_S: u16 = 12;
const IDM_HOTKEY_WIN_ALT_A: u16 = 13;
const IDM_HOTKEY_F11: u16 = 14;
const IDM_HOTKEY_SCROLL: u16 = 15;

// Latency cushion preset IDs (20 .. 24)
const IDM_CUSHION_BASE: u16 = 20;
const IDM_CUSHION_END: u16 = 24;
const CUSHION_PRESETS: &[(u16, u32, &str)] = &[
    (20, 1, "1 ms (Ultra-Low)"),
    (21, 2, "2 ms (Low)"),
    (22, 3, "3 ms (Balanced)"),
    (23, 5, "5 ms (Safe)"),
    (24, 10, "10 ms (Buffered)"),
];

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
const CLR_SEPARATOR: COLORREF = COLORREF(0x003D3535); // #35353D divider

// Section accents:
const CLR_BLUE_ACCENT: COLORREF = COLORREF(0x00F6823B); // #3B82F6 vibrant blue
const CLR_RED_ACCENT: COLORREF = COLORREF(0x004444EF);  // #EF4444 vibrant red
const CLR_SILVER_ACCENT: COLORREF = COLORREF(0x00E1D5CB);// #CBD5E1 metallic platinum
const CLR_CYAN_ACCENT: COLORREF = COLORREF(0x00D4B606); // #06B6D4 vibrant cyan/teal
const CLR_GOLD_ACCENT: COLORREF = COLORREF(0x000B9EF5); // #F59E0B gold star
const CLR_EXIT_ACCENT: COLORREF = COLORREF(0x007171F8); // #F87171 soft coral
const CLR_GREEN_ACCENT: COLORREF = COLORREF(0x0081B910);// #10B981 active status green

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
    CushionOption,
    PauseMirror,
    VolumeSync,
    AutoStart,
    Exit,
    Separator,
}

struct CustomMenuItem {
    id: u16,
    kind: MenuItemKind,
    checked: bool,
    disabled: bool,
    text_len: usize,
    shortcut_len: usize,
    text_u16: Vec<u16>,
    shortcut_u16: Vec<u16>,
}

impl CustomMenuItem {
    fn new(id: u16, kind: MenuItemKind, checked: bool, text: &str, shortcut: &str) -> Self {
        let disabled = matches!(kind, MenuItemKind::HeaderSource | MenuItemKind::HeaderDest);
        Self {
            id,
            kind,
            checked,
            disabled,
            text_len: text.chars().count(),
            shortcut_len: shortcut.chars().count(),
            text_u16: text.encode_utf16().collect(),
            shortcut_u16: shortcut.encode_utf16().collect(),
        }
    }

    fn set_text(&mut self, text: &str) {
        self.text_len = text.chars().count();
        self.text_u16 = text.encode_utf16().collect();
    }

    fn set_shortcut(&mut self, shortcut: &str) {
        self.shortcut_len = shortcut.chars().count();
        self.shortcut_u16 = shortcut.encode_utf16().collect();
    }
}

// ─── Shared state ────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DeviceInfo {
    id: String,
    name: String,
}

struct Shared {
    src: Mutex<String>,
    resolved_src_id: Mutex<String>,
    src_name: Mutex<String>,
    dst: Mutex<String>,
    dst_name: Mutex<String>,
    cushion_ms: AtomicU32,
    restart: AtomicBool,
    quit: AtomicBool,
    paused: AtomicBool,
    manual_paused: AtomicBool,
    vol_sync: AtomicBool,
    wake_event: AtomicUsize,
    status: AtomicU8,
    hwnd: AtomicIsize,
    notified: AtomicBool,
    favorites: Mutex<HashSet<String>>,
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
        paused: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            src: Mutex::new(src),
            resolved_src_id: Mutex::new(String::new()),
            src_name: Mutex::new(src_name),
            dst: Mutex::new(dst),
            dst_name: Mutex::new(dst_name),
            cushion_ms: AtomicU32::new(cushion_ms),
            restart: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            paused: AtomicBool::new(paused),
            manual_paused: AtomicBool::new(false),
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

    /// Checks whether `endpoint_id` matches the configured mirror source (returns `None` if unconfigured).
    fn matches_target_src(&self, endpoint_id: &str) -> Option<bool> {
        let resolved = self.resolved_src_id.lock().unwrap();
        if !resolved.is_empty() {
            return Some(!endpoint_id.is_empty() && endpoint_id.eq_ignore_ascii_case(&resolved));
        }
        drop(resolved);

        let src = self.src.lock().unwrap();
        if src.is_empty() {
            None
        } else {
            Some(!endpoint_id.is_empty() && endpoint_id.eq_ignore_ascii_case(&src))
        }
    }

    fn trigger_restart(&self) {
        self.restart.store(true, Ordering::Release);
        self.signal_wake();
    }

    fn set_status(&self, s: u8) {
        if self.status.swap(s, Ordering::Relaxed) != s {
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

// ─── Windows Dark Mode & High-DPI Helpers ────────────────────────────────────

static ALLOW_DARK_MODE_PROC: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static ACTIVE_MENU_DPI: AtomicU32 = AtomicU32::new(96);

unsafe fn enable_dark_mode() {
    type FnSetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    if let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) {
        // Ordinal 135 is SetPreferredAppMode (Windows 10 1903+)
        if let Some(proc) = GetProcAddress(uxtheme, PCSTR(135 as usize as *const u8)) {
            let func: FnSetPreferredAppMode = mem::transmute(proc);
            func(2); // 2 = ForceDark
        } else if let Some(proc) = GetProcAddress(uxtheme, PCSTR(132 as usize as *const u8)) {
            let func: FnSetPreferredAppMode = mem::transmute(proc);
            func(1); // 1 = AllowDark
        }

        // Ordinal 133 is AllowDarkModeForWindow (Windows 10 1809+)
        if let Some(proc) = GetProcAddress(uxtheme, PCSTR(133 as usize as *const u8)) {
            ALLOW_DARK_MODE_PROC.store(proc as *mut c_void, Ordering::Relaxed);
        }
    }
}

static DARK_STYLED_HWNDS: [AtomicIsize; 3] = [
    AtomicIsize::new(0),
    AtomicIsize::new(0),
    AtomicIsize::new(0),
];

fn reset_dark_styled_hwnds() {
    for slot in &DARK_STYLED_HWNDS {
        slot.store(0, Ordering::Relaxed);
    }
}

/// Applies immersive dark mode, dark borders, and strips white non-client frames from Win32 menus
unsafe fn apply_menu_dark_mode(hwnd: HWND) {
    if hwnd.is_invalid() || hwnd.0.is_null() {
        return;
    }

    let raw = hwnd.0 as isize;
    for slot in &DARK_STYLED_HWNDS {
        let cur = slot.load(Ordering::Relaxed);
        if cur == raw {
            return; // Already styled during this popup session
        }
        if cur == 0 {
            slot.store(raw, Ordering::Relaxed);
            break;
        }
    }

    // 1. Opt-in menu window to Dark Mode via uxtheme
    let proc = ALLOW_DARK_MODE_PROC.load(Ordering::Relaxed);
    if !proc.is_null() {
        type FnAllow = unsafe extern "system" fn(HWND, windows::core::BOOL) -> windows::core::BOOL;
        let func: FnAllow = mem::transmute(proc);
        let _ = func(hwnd, windows::core::BOOL(1));
    }

    // 2. DWM Immersive Dark Mode attribute (20 for Win 11 / modern Win 10, 19 for older builds)
    let dark: i32 = 1;
    let _ = DwmSetWindowAttribute(hwnd, 20, &dark as *const _ as *const c_void, 4);
    let _ = DwmSetWindowAttribute(hwnd, 19, &dark as *const _ as *const c_void, 4);

    // 3. DWM Border Color (attribute 34): match menu background #1F1F23 to eliminate white outline!
    let border: u32 = 0x00231F1F;
    let _ = DwmSetWindowAttribute(hwnd, 34, &border as *const _ as *const c_void, 4);

    // 4. Strip bright non-client frames (WS_EX_DLGMODALFRAME / WS_EX_WINDOWEDGE)
    let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let mask = (WS_EX_DLGMODALFRAME.0 | WS_EX_WINDOWEDGE.0) as isize;
    if (ex & mask) != 0 {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex & !mask);
        let _ = SetWindowPos(
            hwnd,
            None,
            0, 0, 0, 0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED | SWP_NOACTIVATE,
        );
    }
}

/// Retrieves the exact physical DPI of the monitor containing the given screen point.
/// Caches the GetDpiForMonitor function pointer to avoid repeated LoadLibrary/GetProcAddress calls.
unsafe fn get_dpi_for_screen_point(pt: POINT) -> u32 {
    static DPI_FN: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
    static DPI_FN_INIT: AtomicBool = AtomicBool::new(false);

    type FnGetDpiForMonitor = unsafe extern "system" fn(HMONITOR, i32, *mut u32, *mut u32) -> windows_core::HRESULT;

    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    if mon.0.is_null() {
        return 96;
    }

    // One-time initialization of function pointer
    if !DPI_FN_INIT.load(Ordering::Relaxed) {
        if let Ok(shcore) = LoadLibraryW(w!("shcore.dll")) {
            if let Some(proc) = GetProcAddress(shcore, PCSTR(b"GetDpiForMonitor\0".as_ptr())) {
                DPI_FN.store(proc as *mut c_void, Ordering::Relaxed);
            }
        }
        DPI_FN_INIT.store(true, Ordering::Release);
    }

    let proc = DPI_FN.load(Ordering::Acquire);
    if !proc.is_null() {
        let func: FnGetDpiForMonitor = mem::transmute(proc);
        let mut dx = 0u32;
        let mut dy = 0u32;
        if func(mon, 0 /* MDT_EFFECTIVE_DPI */, &mut dx, &mut dy).is_ok() && dx > 0 {
            return dx;
        }
    }
    96
}

// ─── Image & Icon Loader (Native User32 PNG decoder, zero GDI+ overhead) ───────

/// Cached app icon — created once, reused for the lifetime of the process.
static CACHED_ICON: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

unsafe fn get_app_icon() -> HICON {
    let cached = CACHED_ICON.load(Ordering::Relaxed);
    if !cached.is_null() {
        return HICON(cached);
    }
    let icon = create_app_icon_inner();
    CACHED_ICON.store(icon.0, Ordering::Relaxed);
    icon
}

unsafe fn create_app_icon_inner() -> HICON {
    // Windows Vista+ CreateIconFromResourceEx natively decodes raw PNG bytes
    // in user32.dll without loading gdiplus.dll or shlwapi.dll.
    if let Ok(hicon) = CreateIconFromResourceEx(
        LOGO_PNG,
        true,
        0x0003_0000,
        0,
        0,
        LR_DEFAULTCOLOR,
    ) {
        if !hicon.is_invalid() {
            return hicon;
        }
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

// ─── Default Audio Output Device Controller (IPolicyConfig) ───────────────────

unsafe fn set_default_audio_device(endpoint_id: &str) -> bool {
    let wide: Vec<u16> = endpoint_id.encode_utf16().chain(std::iter::once(0)).collect();
    let pcwstr = PCWSTR(wide.as_ptr());

    let iids = [IID_POLICY_CONFIG, IID_POLICY_CONFIG_VISTA];
    for iid in &iids {
        let mut policy: *mut c_void = ptr::null_mut();
        let hr = CoCreateInstanceRaw(
            &CLSID_POLICY_CONFIG,
            ptr::null_mut(),
            CLSCTX_ALL.0,
            iid,
            &mut policy,
        );
        if hr.is_ok() && !policy.is_null() {
            let vtbl = *(policy as *const *const IPolicyConfigVtbl);
            let r0 = ((*vtbl).SetDefaultEndpoint)(policy, pcwstr, 0); // eConsole
            let r1 = ((*vtbl).SetDefaultEndpoint)(policy, pcwstr, 1); // eMultimedia
            let r2 = ((*vtbl).SetDefaultEndpoint)(policy, pcwstr, 2); // eCommunications
            let unk_vtbl = *(policy as *const *const windows_core::IUnknown_Vtbl);
            let _ = ((*unk_vtbl).Release)(policy);
            if r0.is_ok() || r1.is_ok() || r2.is_ok() {
                return true;
            }
        }
    }
    false
}

// ─── Device helpers & UI-thread COM cache ────────────────────────────────────

thread_local! {
    static UI_ENUMERATOR: RefCell<Option<IMMDeviceEnumerator>> = const { RefCell::new(None) };
}

unsafe fn with_ui_enumerator<R>(f: impl FnOnce(&IMMDeviceEnumerator) -> R) -> Option<R> {
    UI_ENUMERATOR.with(|cell| {
        if cell.borrow().is_none() {
            if let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
                *cell.borrow_mut() = Some(en);
            }
        }
        cell.borrow().as_ref().map(f)
    })
}

unsafe fn describe_id(d: &IMMDevice) -> Result<String> {
    let id_p = d.GetId()?;
    let id = id_p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(id_p.0 as _));
    Ok(id)
}

unsafe fn default_device_id(en: &IMMDeviceEnumerator) -> Option<String> {
    en.GetDefaultAudioEndpoint(eRender, eMultimedia)
        .or_else(|_| en.GetDefaultAudioEndpoint(eRender, eConsole))
        .ok()
        .and_then(|d| describe_id(&d).ok())
}

unsafe fn describe(d: &IMMDevice) -> Result<DeviceInfo> {
    let id = describe_id(d)?;
    let store = d.OpenPropertyStore(STGM_READ)?;
    let pv = store.GetValue(&PKEY_Device_FriendlyName)?;
    let name_p = PropVariantToStringAlloc(&pv)?;
    let name = name_p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(name_p.0 as _));
    Ok(DeviceInfo { id, name })
}

unsafe fn enumerate_devices(en: &IMMDeviceEnumerator) -> Result<Vec<(IMMDevice, DeviceInfo)>> {
    let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let count = col.GetCount()?;
    let mut devs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let d = col.Item(i)?;
        let info = describe(&d)?;
        devs.push((d, info));
    }
    devs.sort_by(|a, b| a.1.name.cmp(&b.1.name).then_with(|| a.1.id.cmp(&b.1.id)));
    Ok(devs)
}

enum Found {
    None,
    One(IMMDevice, DeviceInfo),
    Many(usize),
}

unsafe fn find(en: &IMMDeviceEnumerator, needle: &str) -> Result<Found> {
    // O(1) direct endpoint lookup when full endpoint ID is known
    if needle.starts_with('{') {
        if let Ok(d) = en.GetDevice(&HSTRING::from(needle)) {
            if let Ok(state) = d.GetState() {
                if state == DEVICE_STATE_ACTIVE {
                    return Ok(Found::One(d.clone(), describe(&d)?));
                }
            }
        }
    }

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

/// Resolves both source and destination endpoints with O(1) ID lookup when possible,
/// falling back to a single endpoint enumeration pass if partial names are used.
unsafe fn find_pair(
    en: &IMMDeviceEnumerator,
    src_needle: &str,
    dst_needle: &str,
) -> Result<((IMMDevice, DeviceInfo), (IMMDevice, DeviceInfo))> {
    let try_direct = |id: &str| -> Option<(IMMDevice, DeviceInfo)> {
        if !id.starts_with('{') {
            return None;
        }
        let d = en.GetDevice(&HSTRING::from(id)).ok()?;
        if d.GetState().ok()? != DEVICE_STATE_ACTIVE {
            return None;
        }
        let info = describe(&d).ok()?;
        Some((d, info))
    };

    if let (Some(s), Some(t)) = (try_direct(src_needle), try_direct(dst_needle)) {
        return Ok((s, t));
    }

    let s = match find(en, src_needle)? {
        Found::One(d, info) => (d, info),
        _ => return Err(Error::from(E_FAIL)),
    };
    let t = match find(en, dst_needle)? {
        Found::One(d, info) => (d, info),
        _ => return Err(Error::from(E_FAIL)),
    };
    Ok((s, t))
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

unsafe fn wait_while_paused(shared: &Shared) {
    shared.set_status(STATUS_IDLE);
    let wake_raw = shared.wake_event.load(Ordering::Relaxed);
    if wake_raw != 0 {
        let wake_handle = HANDLE(wake_raw as *mut _);
        // Zero-CPU infinite wait: OnDefaultDeviceChanged / hotkey / menu immediately signals wake_event.
        let _ = WaitForSingleObject(wake_handle, INFINITE);
    } else {
        thread::sleep(Duration::from_millis(500));
    }
}

unsafe fn formats_equal(a: *const WAVEFORMATEX, b: *const WAVEFORMATEX) -> bool {
    let wa = ptr::read_unaligned(a);
    let wb = ptr::read_unaligned(b);
    let len_a = mem::size_of::<WAVEFORMATEX>() + wa.cbSize as usize;
    let len_b = mem::size_of::<WAVEFORMATEX>() + wb.cbSize as usize;
    len_a == len_b
        && std::slice::from_raw_parts(a as *const u8, len_a)
            == std::slice::from_raw_parts(b as *const u8, len_b)
}

/// Computes `(prime_silence_frames, copy_frames)` for a captured loopback packet.
/// Adds `jitter_headroom` to `high` to prevent micro-clicks on early timer wakeups,
/// while snapping drifted buffering back toward `cushion` whenever silent packets arrive.
#[inline]
fn compute_mirror_write(frames: u32, queued: u32, cushion: u32, high: u32, silent: bool) -> (u32, u32) {
    let prime = if queued == 0 { cushion } else { 0 };
    let mut n = frames.min(high.saturating_sub(queued + prime));
    if silent && queued > cushion {
        n = n.min((cushion + frames).saturating_sub(queued + prime));
    }
    (prime, n)
}

unsafe fn session(en: &IMMDeviceEnumerator, shared: &Arc<Shared>) -> Result<()> {
    if shared.paused.load(Ordering::Relaxed) || shared.manual_paused.load(Ordering::Relaxed) {
        wait_while_paused(shared);
        return Ok(());
    }

    let (src_needle, dst_needle) = {
        let s = shared.src.lock().unwrap().clone();
        let d = shared.dst.lock().unwrap().clone();
        (s, d)
    };
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);

    if src_needle.is_empty() || dst_needle.is_empty() {
        return Err(Error::from(E_FAIL));
    }

    // Fast pre-check before touching devices when full ID is configured
    let def_id = default_device_id(en);
    if src_needle.starts_with('{') {
        if let Some(ref cur_def) = def_id {
            if !cur_def.eq_ignore_ascii_case(&src_needle) {
                shared.paused.store(true, Ordering::Relaxed);
                wait_while_paused(shared);
                return Ok(());
            }
        }
    }

    let ((src, src_info), (dst, dst_info)) = find_pair(en, &src_needle, &dst_needle)?;
    if src_info.id == dst_info.id {
        return Err(Error::from(E_FAIL));
    }

    // Verify default output matches resolved source (handles CLI partial names too)
    if let Some(ref cur_def) = def_id {
        if !cur_def.eq_ignore_ascii_case(&src_info.id) {
            *shared.resolved_src_id.lock().unwrap() = src_info.id;
            shared.paused.store(true, Ordering::Relaxed);
            wait_while_paused(shared);
            return Ok(());
        }
    }

    *shared.resolved_src_id.lock().unwrap() = src_info.id.clone();
    *shared.src_name.lock().unwrap() = src_info.name.clone();
    *shared.dst_name.lock().unwrap() = dst_info.name.clone();

    // ── Open audio clients ──────────────────────────────────────────
    let sc: IAudioClient = src.Activate(CLSCTX_ALL, None)?;
    let mut dc: IAudioClient = dst.Activate(CLSCTX_ALL, None)?;

    let fmt = sc.GetMixFormat()?;
    let wf = ptr::read_unaligned(fmt);
    let (rate, frame_bytes) = (wf.nSamplesPerSec, wf.nBlockAlign as usize);

    // Query device period BEFORE Initialize so destination buffer is sized tightly to period + cushion
    let (mut period_hns, mut min_hns) = (100_000i64, 0i64);
    let _ = sc.GetDevicePeriod(Some(&mut period_hns), Some(&mut min_hns));
    if period_hns <= 0 {
        period_hns = 100_000;
    }

    // Skip Windows' Audio Resampler MFT when source and destination mix formats match identically
    let mut dst_flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if let Ok(dfmt) = dc.GetMixFormat() {
        if formats_equal(fmt, dfmt) {
            dst_flags = 0;
        }
        CoTaskMemFree(Some(dfmt as _));
    }

    let req_hns = (period_hns + (cushion_ms as i64 + 2) * 10_000).max(130_000);

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
        if dc
            .Initialize(AUDCLNT_SHAREMODE_SHARED, dst_flags, req_hns, 0, fmt, None)
            .is_err()
            && dst_flags == 0
        {
            // Fallback with AUTOCONVERTPCM on a freshly activated client if direct match is rejected
            dc = dst.Activate(CLSCTX_ALL, None)?;
            dc.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                req_hns,
                0,
                fmt,
                None,
            )?;
        }
        Ok(())
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
        show_balloon(
            shared,
            "Mirror Active",
            &format!("{} → {} ({} ms)", trunc(&src_info.name, 36), trunc(&dst_info.name, 36), cushion_ms),
        );
    }

    let result = (|| -> Result<()> {
        let cap: IAudioCaptureClient = sc.GetService()?;
        let rnd: IAudioRenderClient = dc.GetService()?;
        let ring = dc.GetBufferSize()?;

        let period = ((rate as i64 * period_hns / 10_000_000) as u32).max(1);
        let cushion = ((rate as u64 * cushion_ms as u64 / 1000) as u32).max(1);
        // +2 ms jitter headroom avoids truncating live audio when Windows fires slightly early
        let jitter_headroom = (rate / 500).max(1);
        let high = (cushion + period + jitter_headroom).min(ring);

        let wake_raw = shared.wake_event.load(Ordering::Relaxed);
        let wake_handle = HANDLE(wake_raw as *mut _);
        let wait_handles = [ev, wake_handle];

        // Drain any stale wake signal accumulated during setup, then verify no state flags flipped
        if !wake_handle.is_invalid() {
            let _ = ResetEvent(wake_handle);
        }
        if shared.restart.load(Ordering::Acquire)
            || shared.quit.load(Ordering::Acquire)
            || shared.paused.load(Ordering::Acquire)
            || shared.manual_paused.load(Ordering::Acquire)
        {
            return Ok(());
        }

        dc.Start()?;
        sc.Start()?;

        loop {
            // 1000 ms timeout cuts dormant idle wakeups by 4x-10x; IMMNotificationClient + wake_event still react in 0 ms
            let w = WaitForMultipleObjects(&wait_handles, false, 1000);

            if w == windows::Win32::Foundation::WAIT_EVENT(WAIT_OBJECT_0.0 + 1)
                || shared.restart.load(Ordering::Acquire)
                || shared.quit.load(Ordering::Acquire)
            {
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
                let silent = (flags & BUFFERFLAGS_SILENT) != 0;
                let (prime, n) = compute_mirror_write(frames, queued, cushion, high, silent);
                let total = prime + n;
                if total > 0 {
                    let out = rnd.GetBuffer(total)?;
                    if silent {
                        // Zero-copy WASAPI silence flag skips memset of silent audio buffers
                        rnd.ReleaseBuffer(total, BUFFERFLAGS_SILENT)?;
                    } else {
                        if prime > 0 {
                            ptr::write_bytes(out, 0, prime as usize * frame_bytes);
                        }
                        let body = out.add(prime as usize * frame_bytes);
                        ptr::copy_nonoverlapping(data, body, n as usize * frame_bytes);
                        rnd.ReleaseBuffer(total, 0)?;
                    }
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
        let co_init = CoInitializeEx(None, COINIT_MULTITHREADED);

        let mut task_idx = 0u32;
        let avrt_handle = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_idx).ok();
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);

        if let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            let notifier: IMMNotificationClient =
                DeviceNotifier { shared: Arc::clone(&shared) }.into();
            let _ = en.RegisterEndpointNotificationCallback(&notifier);

            while !shared.quit.load(Ordering::Acquire) {
                let _ = session(&en, &shared);

                if shared.quit.load(Ordering::Acquire) {
                    break;
                }

                if shared.restart.swap(false, Ordering::AcqRel) {
                    continue;
                }

                if shared.paused.load(Ordering::Acquire) {
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

            let _ = en.UnregisterEndpointNotificationCallback(&notifier);
            drop(notifier);
            drop(en);
        }

        shared.set_status(STATUS_IDLE);
        if let Some(h) = avrt_handle {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
        if co_init.is_ok() {
            CoUninitialize();
        }
    }
}

// ─── Sound Switch Feature (Audio Output Cycling with Favorites) ──────────────

unsafe fn cycle_audio_device(hwnd: HWND) {
    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;

    let Some((devices, current_default_id)) = with_ui_enumerator(|en| {
        let devs = enumerate_devices(en).ok()?;
        if devs.is_empty() {
            return None;
        }
        let def = default_device_id(en).unwrap_or_else(|| shared.src.lock().unwrap().clone());
        Some((devs, def))
    })
    .flatten() else {
        return;
    };

    // Determine candidate list: if at least 2 favorites exist, cycle ONLY among favorites; else all devices.
    let candidates: Vec<&(IMMDevice, DeviceInfo)> = {
        let favorites = shared.favorites.lock().unwrap();
        let fav_candidates: Vec<&(IMMDevice, DeviceInfo)> =
            devices.iter().filter(|d| favorites.contains(&d.1.id)).collect();
        if fav_candidates.len() > 1 {
            fav_candidates
        } else {
            devices.iter().collect()
        }
    };

    if candidates.len() <= 1 {
        return;
    }

    let next_idx = match candidates.iter().position(|d| d.1.id == current_default_id) {
        Some(pos) => (pos + 1) % candidates.len(),
        None => 0,
    };
    let next_device = candidates[next_idx];

    let switched = set_default_audio_device(&next_device.1.id);

    // Check if the newly selected default output device is our configured mirror source
    let is_mirror_src = shared.matches_target_src(&next_device.1.id).unwrap_or(false);

    // Do NOT touch shared.src or shared.dst!
    // If switched to the mirror source monitor (and not manually paused), mirror is ACTIVE.
    // If switched to headset or other device, mirror is PAUSED so monitors stay silent!
    let new_paused = !is_mirror_src || shared.manual_paused.load(Ordering::Relaxed);
    shared.paused.store(new_paused, Ordering::Release);
    shared.trigger_restart();

    update_tooltip(hwnd, shared);
    if switched {
        let balloon_msg = if is_mirror_src && !shared.manual_paused.load(Ordering::Relaxed) {
            format!("Default: {}\n(Mirror Active)", next_device.1.name)
        } else if is_mirror_src {
            format!("Default: {}\n(Manually Paused)", next_device.1.name)
        } else {
            format!("Default: {}\n(Monitors Silent)", next_device.1.name)
        };
        show_balloon(shared, "Audio Output Switched", &balloon_msg);
    }
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

// ─── Modal Menu Active Context for Non-Dismissing Favorites ──────────────────

struct ActiveMenuContext {
    fav_menu: HMENU,
    devices: Vec<DeviceInfo>,
    items_ptr: *mut Vec<Box<CustomMenuItem>>,
    menu_brush: HBRUSH,
}

unsafe impl Send for ActiveMenuContext {}
unsafe impl Sync for ActiveMenuContext {}

static ACTIVE_MENU_CTX: Mutex<Option<ActiveMenuContext>> = Mutex::new(None);
static LAST_MENU_DEVICES: Mutex<Vec<DeviceInfo>> = Mutex::new(Vec::new());

/// Global Windows message hook that intercepts clicks inside the Favorites submenu
/// and toggles favorites in place without closing the right-click menu!
unsafe extern "system" fn menu_msg_filter_hook(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if ncode == 2 /* MSGF_MENU */ {
        let msg = &mut *(lparam.0 as *mut MSG);
        if !msg.hwnd.is_invalid() && !msg.hwnd.0.is_null() {
            apply_menu_dark_mode(msg.hwnd);
        }

        if msg.message == WM_LBUTTONDOWN || msg.message == WM_LBUTTONUP {
            if let Ok(guard) = ACTIVE_MENU_CTX.lock() {
                if let Some(ctx) = guard.as_ref() {
                    let mut idx = MenuItemFromPoint(None, ctx.fav_menu, msg.pt);
                    if idx < 0 && !msg.hwnd.is_invalid() && !msg.hwnd.0.is_null() {
                        let mut client_pt = msg.pt;
                        let _ = ScreenToClient(msg.hwnd, &mut client_pt);
                        idx = MenuItemFromPoint(Some(msg.hwnd), ctx.fav_menu, client_pt);
                    }

                    if idx >= 0 {
                        if msg.message == WM_LBUTTONUP {
                            let uidx = idx as usize;
                            if uidx < ctx.devices.len() {
                                let dev_id = &ctx.devices[uidx].id;
                                let p = GLOBAL_SHARED.load(Ordering::Relaxed);
                                if !p.is_null() {
                                    let shared = &*p;
                                    let mut favs = shared.favorites.lock().unwrap();
                                    let was_fav = favs.contains(dev_id);
                                    if was_fav {
                                        favs.remove(dev_id);
                                    } else {
                                        favs.insert(dev_id.clone());
                                    }
                                    let has_favs = !favs.is_empty();
                                    drop(favs);
                                    save_current_settings(shared);

                                    let items = &mut *ctx.items_ptr;
                                    for it in items.iter_mut() {
                                        if it.id == IDM_FAV_BASE + uidx as u16 {
                                            it.checked = !was_fav;
                                            it.set_shortcut(if !was_fav { "★" } else { "" });
                                        } else if it.kind == MenuItemKind::FavoritesHeader {
                                            it.checked = has_favs;
                                            it.set_text(if !has_favs {
                                                "★ Favorites (None set)"
                                            } else {
                                                "★ Favorites"
                                            });
                                        }
                                    }
                                }
                            } else if uidx == ctx.devices.len() + 1 /* Clear All Favorites */ {
                                let p = GLOBAL_SHARED.load(Ordering::Relaxed);
                                if !p.is_null() {
                                    let shared = &*p;
                                    shared.favorites.lock().unwrap().clear();
                                    save_current_settings(shared);

                                    let items = &mut *ctx.items_ptr;
                                    for it in items.iter_mut() {
                                        if it.kind == MenuItemKind::ItemFavorite {
                                            it.checked = false;
                                            it.set_shortcut("");
                                        } else if it.kind == MenuItemKind::FavoritesHeader {
                                            it.checked = false;
                                            it.set_text("★ Favorites (None set)");
                                        }
                                    }
                                }
                            }

                            // Instantly repaint all active popup menus (both submenu and root)
                            let mut cur = FindWindowExW(None, None, w!("#32768"), PCWSTR::null()).ok();
                            while let Some(w) = cur {
                                if !w.is_invalid() && !w.0.is_null() {
                                    let _ = InvalidateRect(Some(w), None, true);
                                    let _ = UpdateWindow(w);
                                }
                                cur = FindWindowExW(None, Some(w), w!("#32768"), PCWSTR::null()).ok();
                            }
                        }

                        // Neutralize message so Windows menu loop NEVER terminates on favorites clicks!
                        msg.message = WM_NULL;
                        return LRESULT(1);
                    }
                }
            }
        }
    }
    CallNextHookEx(None, ncode, wparam, lparam)
}

// ─── Tray Icon & Color-Coded Right-Click Menu ────────────────────────────────

static GLOBAL_SHARED: AtomicPtr<Shared> = AtomicPtr::new(ptr::null_mut());
static FONT_NORMAL: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static FONT_BOLD: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static FONT_CACHED_DPI: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAYICON => {
            let event = lp.0 as u32 & 0xFFFF;
            if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
                show_context_menu(hwnd);
            } else if event == WM_MBUTTONUP {
                cycle_audio_device(hwnd);
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
        WM_INITMENUPOPUP => {
            // Apply native dark mode and background brush to the menu and its window
            let hmenu = HMENU(wp.0 as _);
            if let Ok(guard) = ACTIVE_MENU_CTX.lock() {
                if let Some(ctx) = guard.as_ref() {
                    let mi = MENUINFO {
                        cbSize: mem::size_of::<MENUINFO>() as u32,
                        fMask: MIM_BACKGROUND | MIM_APPLYTOSUBMENUS,
                        dwStyle: 0,
                        cyMax: 0,
                        hbrBack: ctx.menu_brush,
                        dwContextHelpID: 0,
                        dwMenuData: 0,
                    };
                    let _ = SetMenuInfo(hmenu, &mi);
                }
            }
            // Remove white outline: set dark mode attribute on the popup menu window (#32768)
            let menu_hwnd = FindWindowW(w!("#32768"), PCWSTR::null());
            apply_menu_dark_mode(menu_hwnd);
            LRESULT(0)
        }
        WM_MEASUREITEM => {
            let mis = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
            if mis.CtlType == ODT_MENU && mis.itemData != 0 {
                let item = &*(mis.itemData as *const CustomMenuItem);
                let dpi = ACTIVE_MENU_DPI.load(Ordering::Relaxed);
                let dpi = if dpi == 0 { 96 } else { dpi };
                let scale = dpi as f32 / 96.0;

                match item.kind {
                    MenuItemKind::Separator => {
                        mis.itemHeight = (9.0 * scale).round() as u32;
                        mis.itemWidth = (140.0 * scale).round() as u32;
                    }
                    MenuItemKind::HeaderSource | MenuItemKind::HeaderDest | MenuItemKind::FavoritesHeader => {
                        mis.itemHeight = (26.0 * scale).round() as u32;
                        let text_w = (item.text_len * 8 + item.shortcut_len * 8 + 72) as f32 * scale;
                        mis.itemWidth = text_w.max(330.0 * scale).round() as u32;
                    }
                    _ => {
                        mis.itemHeight = (28.0 * scale).round() as u32;
                        let text_w = (item.text_len * 8 + item.shortcut_len * 8 + 84) as f32 * scale;
                        mis.itemWidth = text_w.max(330.0 * scale).round() as u32;
                    }
                }
            }
            LRESULT(1)
        }
        WM_DRAWITEM => {
            let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
            if dis.CtlType == ODT_MENU && dis.itemData != 0 {
                let item = &*(dis.itemData as *const CustomMenuItem);
                draw_menu_item(dis, item, hwnd);
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
            let old_n = FONT_NORMAL.swap(ptr::null_mut(), Ordering::Relaxed);
            let old_b = FONT_BOLD.swap(ptr::null_mut(), Ordering::Relaxed);
            if !old_n.is_null() {
                let _ = DeleteObject(HGDIOBJ(old_n));
            }
            if !old_b.is_null() {
                let _ = DeleteObject(HGDIOBJ(old_b));
            }
            let old_icon = CACHED_ICON.swap(ptr::null_mut(), Ordering::Relaxed);
            if !old_icon.is_null() {
                let _ = DestroyIcon(HICON(old_icon));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

#[inline]
unsafe fn fill_solid(hdc: HDC, rc: &RECT, color: COLORREF) {
    SetDCBrushColor(hdc, color);
    let dc_brush = HBRUSH(GetStockObject(DC_BRUSH).0);
    FillRect(hdc, rc, dc_brush);
}

#[inline]
unsafe fn draw_u16_slice(
    hdc: HDC,
    src: &[u16],
    rect: &mut RECT,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let mut stack_buf = [0u16; 256];
    let n = src.len().min(stack_buf.len());
    stack_buf[..n].copy_from_slice(&src[..n]);
    DrawTextW(hdc, &mut stack_buf[..n], rect, flags);
}

unsafe fn draw_menu_item(dis: &DRAWITEMSTRUCT, item: &CustomMenuItem, _hwnd: HWND) {
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let selected = (dis.itemState & ODS_SELECTED) != 0;
    let disabled = item.disabled || (dis.itemState & ODS_GRAYED) != 0 || (dis.itemState & ODS_DISABLED) != 0;

    let dpi = ACTIVE_MENU_DPI.load(Ordering::Relaxed);
    let dpi = if dpi == 0 { 96 } else { dpi };
    let scale = dpi as f32 / 96.0;

    let bg_color = if item.disabled {
        CLR_BG
    } else {
        match item.kind {
            MenuItemKind::ItemSource if selected => CLR_BG_SRC_HOVER,
            MenuItemKind::ItemDest if selected => CLR_BG_DST_HOVER,
            _ if selected => CLR_BG_HOVER,
            _ => CLR_BG,
        }
    };

    fill_solid(hdc, &rc, bg_color);

    if item.kind == MenuItemKind::Separator {
        let pad_h = (12.0 * scale) as i32;
        let sep_rect = RECT {
            left: rc.left + pad_h,
            top: rc.top + (4.0 * scale) as i32,
            right: rc.right - pad_h,
            bottom: rc.top + (5.0 * scale).max(rc.top as f32 + 1.0) as i32,
        };
        fill_solid(hdc, &sep_rect, CLR_SEPARATOR);
        return;
    }

    SetBkMode(hdc, TRANSPARENT);

    // Font selection: crisp native point size proportional to physical monitor DPI.
    // Fonts are cached per (DPI, weight) to avoid CreateFontW/DeleteObject per menu item.
    let font_weight = match item.kind {
        MenuItemKind::HeaderSource | MenuItemKind::HeaderDest | MenuItemKind::FavoritesHeader => FW_BOLD,
        MenuItemKind::ItemSource | MenuItemKind::ItemDest if item.checked => FW_BOLD,
        MenuItemKind::SoundSwitch => FW_BOLD,
        _ => FW_NORMAL,
    };

    let cached_dpi = FONT_CACHED_DPI.load(Ordering::Relaxed);
    if cached_dpi != dpi {
        let old_n = FONT_NORMAL.swap(ptr::null_mut(), Ordering::Relaxed);
        let old_b = FONT_BOLD.swap(ptr::null_mut(), Ordering::Relaxed);
        if !old_n.is_null() {
            let _ = DeleteObject(HGDIOBJ(old_n));
        }
        if !old_b.is_null() {
            let _ = DeleteObject(HGDIOBJ(old_b));
        }
        FONT_CACHED_DPI.store(dpi, Ordering::Relaxed);
    }

    let font_slot = if font_weight == FW_BOLD { &FONT_BOLD } else { &FONT_NORMAL };
    let mut font_ptr = font_slot.load(Ordering::Relaxed);
    if font_ptr.is_null() {
        let font_height = -((10 * dpi as i32 + 36) / 72);
        let new_font = CreateFontW(
            font_height,
            0, 0, 0,
            font_weight.0 as i32,
            0, 0, 0,
            FONT_CHARSET(1), // DEFAULT_CHARSET
            FONT_OUTPUT_PRECISION(0),
            FONT_CLIP_PRECISION(0),
            FONT_QUALITY(5), // CLEARTYPE_QUALITY
            0,
            w!("Segoe UI"),
        );
        font_ptr = new_font.0;
        if !font_ptr.is_null() {
            font_slot.store(font_ptr, Ordering::Relaxed);
        }
    }
    let old_font = if !font_ptr.is_null() {
        Some(SelectObject(hdc, HFONT(font_ptr).into()))
    } else {
        None
    };

    // Left accent pill
    let pill_w = (4.0 * scale).max(3.0) as i32;
    let pill_color = match item.kind {
        MenuItemKind::HeaderSource => Some(CLR_BLUE_ACCENT),
        MenuItemKind::HeaderDest => Some(CLR_RED_ACCENT),
        MenuItemKind::SoundSwitch => Some(CLR_CYAN_ACCENT),
        _ => None,
    };
    if let Some(pclr) = pill_color {
        let pill_rect = RECT {
            left: rc.left + (5.0 * scale) as i32,
            top: rc.top + (5.0 * scale) as i32,
            right: rc.left + (5.0 * scale) as i32 + pill_w,
            bottom: rc.bottom - (5.0 * scale) as i32,
        };
        fill_solid(hdc, &pill_rect, pclr);
    }

    // Checkmark or star glyph drawing
    if item.checked {
        let check_color = match item.kind {
            MenuItemKind::ItemSource => CLR_BLUE_ACCENT,
            MenuItemKind::ItemDest => CLR_RED_ACCENT,
            MenuItemKind::ItemFavorite => CLR_GOLD_ACCENT,
            MenuItemKind::VolumeSync | MenuItemKind::CushionOption => CLR_SILVER_ACCENT,
            MenuItemKind::PauseMirror => CLR_GOLD_ACCENT,
            _ => CLR_TEXT,
        };
        SetTextColor(hdc, check_color);
        let mut check_glyph = if item.kind == MenuItemKind::ItemFavorite {
            [0x2605u16] // ★
        } else {
            [0x2713u16] // ✓
        };
        let mut check_rect = RECT {
            left: rc.left + (10.0 * scale) as i32,
            top: rc.top,
            right: rc.left + (26.0 * scale) as i32,
            bottom: rc.bottom,
        };
        DrawTextW(hdc, &mut check_glyph, &mut check_rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    }

    // Text color
    let text_color = match item.kind {
        MenuItemKind::HeaderSource => CLR_BLUE_ACCENT,
        MenuItemKind::HeaderDest => CLR_RED_ACCENT,
        MenuItemKind::SoundSwitch => CLR_CYAN_ACCENT,
        MenuItemKind::FavoritesHeader | MenuItemKind::ItemFavorite => CLR_GOLD_ACCENT,
        MenuItemKind::VolumeSync => CLR_SILVER_ACCENT,
        MenuItemKind::PauseMirror if item.checked => CLR_GOLD_ACCENT,
        MenuItemKind::Exit => CLR_EXIT_ACCENT,
        _ if disabled => CLR_TEXT_MUTED,
        _ => CLR_TEXT,
    };
    SetTextColor(hdc, text_color);

    // Reserve clean right margin for shortcut/status badges so text never overlaps
    let right_reserve = if item.shortcut_u16.is_empty() {
        16.0
    } else {
        (item.shortcut_len as f32 * 7.5 + 26.0).max(84.0)
    };
    let mut text_rect = RECT {
        left: rc.left + (30.0 * scale) as i32,
        top: rc.top,
        right: (rc.right - (right_reserve * scale) as i32).max(rc.left + 60),
        bottom: rc.bottom,
    };
    draw_u16_slice(
        hdc,
        &item.text_u16,
        &mut text_rect,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
    );

    // Shortcut or status badge (right aligned)
    if !item.shortcut_u16.is_empty() {
        let sc_color = if item.kind == MenuItemKind::HeaderSource {
            if item.shortcut_u16.first().copied() == Some('●' as u16) {
                CLR_GREEN_ACCENT
            } else {
                CLR_GOLD_ACCENT
            }
        } else if matches!(item.kind, MenuItemKind::ItemSource | MenuItemKind::ItemDest) {
            CLR_CYAN_ACCENT
        } else {
            CLR_TEXT_MUTED
        };
        SetTextColor(hdc, sc_color);
        let mut sc_rect = RECT {
            left: rc.right - (150.0 * scale) as i32,
            top: rc.top,
            right: rc.right - (16.0 * scale) as i32,
            bottom: rc.bottom,
        };
        draw_u16_slice(
            hdc,
            &item.shortcut_u16,
            &mut sc_rect,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    if let Some(prev) = old_font {
        SelectObject(hdc, prev);
    }
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
    menu: HMENU,
    id: u16,
    kind: MenuItemKind,
    checked: bool,
    text: &str,
    shortcut: &str,
) {
    let boxed = Box::new(CustomMenuItem::new(id, kind, checked, text, shortcut));
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
    parent_menu: HMENU,
    sub_menu: HMENU,
    kind: MenuItemKind,
    checked: bool,
    text: &str,
) {
    let boxed = Box::new(CustomMenuItem::new(0, kind, checked, text, "▶"));
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
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let dpi = get_dpi_for_screen_point(pt);
    ACTIVE_MENU_DPI.store(dpi, Ordering::Relaxed);

    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let (devices, default_id) = with_ui_enumerator(|en| {
        let devs = enumerate_devices(en).unwrap_or_default();
        let def = default_device_id(en).unwrap_or_default();
        (devs, def)
    })
    .unwrap_or_default();

    let dev_infos: Vec<DeviceInfo> = devices.iter().map(|d| d.1.clone()).collect();
    *LAST_MENU_DEVICES.lock().unwrap() = dev_infos.clone();

    let mut items: Vec<Box<CustomMenuItem>> = Vec::new();
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);
    let is_manual_paused = shared.manual_paused.load(Ordering::Relaxed);
    let is_paused = shared.paused.load(Ordering::Relaxed);

    let status_badge = if is_manual_paused {
        "⏸ PAUSED (MANUAL)".to_string()
    } else if is_paused {
        "⏸ PAUSED".to_string()
    } else {
        match shared.status.load(Ordering::Relaxed) {
            STATUS_ACTIVE => format!("● ACTIVE  {} ms", cushion_ms),
            STATUS_WAITING => "⏳ WAITING".to_string(),
            _ => format!("{} ms", cushion_ms),
        }
    };

    // ── 1. SOURCE SECTION (BLUE) ──────────────────────────────────────────
    push_custom_item(&mut items, menu, 0, MenuItemKind::HeaderSource, false, "SOURCE (Capture Output)", &status_badge);
    let current_src = shared.src.lock().unwrap().clone();
    for (i, info) in dev_infos.iter().enumerate() {
        let is_chk = is_match(info, &current_src);
        let badge = if !default_id.is_empty() && info.id.eq_ignore_ascii_case(&default_id) {
            "Default"
        } else {
            ""
        };
        push_custom_item(&mut items, menu, IDM_SRC_BASE + i as u16, MenuItemKind::ItemSource, is_chk, &info.name, badge);
    }

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 2. DESTINATION SECTION (RED) ──────────────────────────────────────
    push_custom_item(&mut items, menu, 0, MenuItemKind::HeaderDest, false, "DESTINATION (Mirror Target)", "");
    let current_dst = shared.dst.lock().unwrap().clone();
    for (i, info) in dev_infos.iter().enumerate() {
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

    for (i, info) in dev_infos.iter().enumerate() {
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

    // ── 6. LATENCY CUSHION SUBMENU ────────────────────────────────────────
    let cushion_menu = CreatePopupMenu().unwrap_or_default();
    for &(id, ms, label) in CUSHION_PRESETS {
        push_custom_item(&mut items, cushion_menu, id, MenuItemKind::CushionOption, cushion_ms == ms, label, "");
    }
    let cushion_label = format!("Latency Cushion ({} ms)", cushion_ms);
    push_custom_popup(&mut items, menu, cushion_menu, MenuItemKind::VolumeSync, false, &cushion_label);

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 7. PAUSE / VOLUME SYNC / AUTOSTART ────────────────────────────────
    push_custom_item(&mut items, menu, IDM_PAUSE_MIRROR, MenuItemKind::PauseMirror, is_manual_paused, "Pause Mirroring", "");

    let vol_flag = shared.vol_sync.load(Ordering::Relaxed);
    push_custom_item(&mut items, menu, IDM_VOLSYNC, MenuItemKind::VolumeSync, vol_flag, "Volume Synchronization", "");

    let auto_flag = autostart_enabled();
    push_custom_item(&mut items, menu, IDM_AUTOSTART, MenuItemKind::AutoStart, auto_flag, "Start with Windows", "");

    push_custom_item(&mut items, menu, 0, MenuItemKind::Separator, false, "", "");

    // ── 8. EXIT ───────────────────────────────────────────────────────────
    push_custom_item(&mut items, menu, IDM_EXIT, MenuItemKind::Exit, false, "Exit", "");

    // Set menu background brush to eliminate white gaps
    let menu_brush = CreateSolidBrush(CLR_BG);
    let mi = MENUINFO {
        cbSize: mem::size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND | MIM_APPLYTOSUBMENUS,
        dwStyle: 0,
        cyMax: 0,
        hbrBack: menu_brush,
        dwContextHelpID: 0,
        dwMenuData: 0,
    };
    let _ = SetMenuInfo(menu, &mi);
    let _ = SetMenuInfo(fav_menu, &mi);
    let _ = SetMenuInfo(hk_menu, &mi);
    let _ = SetMenuInfo(cushion_menu, &mi);

    // Set up active menu context and message hook to keep Favorites open on click!
    let items_ptr: *mut Vec<Box<CustomMenuItem>> = &mut items;
    *ACTIVE_MENU_CTX.lock().unwrap() = Some(ActiveMenuContext {
        fav_menu,
        devices: dev_infos,
        items_ptr,
        menu_brush,
    });

    let hook: HHOOK = SetWindowsHookExW(
        WH_MSGFILTER,
        Some(menu_msg_filter_hook),
        None,
        GetCurrentThreadId(),
    ).unwrap_or_default();

    // Track popup menu with reliable outside-click dismissal
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

    if !hook.is_invalid() {
        let _ = UnhookWindowsHookEx(hook);
    }
    *ACTIVE_MENU_CTX.lock().unwrap() = None;
    reset_dark_styled_hwnds();

    let _ = DeleteObject(menu_brush.into());
    let _ = DestroyMenu(cushion_menu);
    let _ = DestroyMenu(fav_menu);
    let _ = DestroyMenu(hk_menu);
    let _ = DestroyMenu(menu);

    // Only trim working set on menu close when not actively streaming audio,
    // avoiding soft page faults on the real-time Pro Audio thread.
    if shared.paused.load(Ordering::Relaxed) || shared.status.load(Ordering::Relaxed) != STATUS_ACTIVE {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

fn is_match(info: &DeviceInfo, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if info.id.eq_ignore_ascii_case(needle) {
        return true;
    }
    let n = needle.to_lowercase();
    info.name.to_lowercase().contains(&n) || info.id.to_lowercase().contains(&n)
}

unsafe fn resolve_menu_device(idx: usize) -> Option<DeviceInfo> {
    LAST_MENU_DEVICES
        .lock()
        .unwrap()
        .get(idx)
        .cloned()
        .or_else(|| {
            with_ui_enumerator(|en| {
                enumerate_devices(en)
                    .ok()
                    .and_then(|devs| devs.get(idx).map(|d| d.1.clone()))
            })
            .flatten()
        })
}

unsafe fn handle_menu_command(hwnd: HWND, id: u16) {
    let p = GLOBAL_SHARED.load(Ordering::Relaxed);
    if p.is_null() {
        return;
    }
    let shared = &*p;

    match id {
        IDM_EXIT => {
            shared.quit.store(true, Ordering::Release);
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
            shared.trigger_restart();
        }
        IDM_PAUSE_MIRROR => {
            let next_manual = !shared.manual_paused.load(Ordering::Relaxed);
            shared.manual_paused.store(next_manual, Ordering::Release);
            if next_manual {
                shared.paused.store(true, Ordering::Release);
            } else {
                // Resume only if current Windows default output matches mirror source
                let is_src = with_ui_enumerator(|en| default_device_id(en))
                    .flatten()
                    .and_then(|def| shared.matches_target_src(&def))
                    .unwrap_or(false);
                shared.paused.store(!is_src, Ordering::Release);
            }
            shared.trigger_restart();
            update_tooltip(hwnd, shared);
        }
        IDM_CYCLE_DEVICE => {
            cycle_audio_device(hwnd);
        }
        IDM_CLEAR_FAVORITES => {
            shared.favorites.lock().unwrap().clear();
            save_current_settings(shared);
            show_balloon(shared, "Favorites Cleared", "Audio switcher will cycle through all active devices.");
        }
        _ if id >= IDM_CUSHION_BASE && id <= IDM_CUSHION_END => {
            if let Some(&(_, ms, _)) = CUSHION_PRESETS.iter().find(|&&(cid, _, _)| cid == id) {
                shared.cushion_ms.store(ms, Ordering::Release);
                save_current_settings(shared);
                shared.trigger_restart();
                update_tooltip(hwnd, shared);
            }
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
    if let Some(info) = resolve_menu_device(idx) {
        let mut favs = shared.favorites.lock().unwrap();
        if favs.contains(&info.id) {
            favs.remove(&info.id);
        } else {
            favs.insert(info.id);
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
    if let Some(info) = resolve_menu_device(idx) {
        if is_src {
            *shared.src.lock().unwrap() = info.id.clone();
            *shared.resolved_src_id.lock().unwrap() = info.id.clone();
            *shared.src_name.lock().unwrap() = info.name;
            set_default_audio_device(&info.id);
            shared.manual_paused.store(false, Ordering::Release);
            shared.paused.store(false, Ordering::Release);
        } else {
            *shared.dst.lock().unwrap() = info.id;
            *shared.dst_name.lock().unwrap() = info.name;
            if let Some(Some(def_id)) = with_ui_enumerator(|en| default_device_id(en)) {
                if shared.matches_target_src(&def_id).unwrap_or(false)
                    && !shared.manual_paused.load(Ordering::Relaxed)
                {
                    shared.paused.store(false, Ordering::Release);
                }
            }
        }
        shared.trigger_restart();
    }
}

// ─── Tray icon management ────────────────────────────────────────────────────

unsafe fn add_tray_icon(hwnd: HWND) {
    let icon = get_app_icon();
    let mut nid = NOTIFYICONDATAW {
        cbSize: mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide(&mut nid.szTip, &format!("Monitor Mirror v{APP_VERSION}"));
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
    static LAST_TIP_HASH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    let src = shared.src_name.lock().unwrap().clone();
    let dst = shared.dst_name.lock().unwrap().clone();
    let is_paused = shared.paused.load(Ordering::Relaxed);
    let is_manual = shared.manual_paused.load(Ordering::Relaxed);
    let cushion_ms = shared.cushion_ms.load(Ordering::Relaxed);

    let tip = if is_manual {
        format!("Monitor Mirror v{APP_VERSION}\n⏸ Mirror Paused (Manual)")
    } else if is_paused {
        format!("Monitor Mirror v{APP_VERSION}\n⏸ Paused (Monitors Silent)")
    } else if src.is_empty() && dst.is_empty() {
        format!("Monitor Mirror v{APP_VERSION}\nIdle (no devices selected)")
    } else {
        let status = match shared.status.load(Ordering::Relaxed) {
            STATUS_ACTIVE => "▶",
            STATUS_WAITING => "⏳",
            _ => "•",
        };
        format!(
            "Monitor Mirror v{APP_VERSION}\n{} {} ms: {} → {}",
            status,
            cushion_ms,
            trunc(&src, 26),
            trunc(&dst, 26)
        )
    };

    // FNV-1a hash: skip Shell_NotifyIconW IPC if tooltip text is unchanged
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in tip.as_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    if LAST_TIP_HASH.swap(hash, Ordering::Relaxed) == hash {
        return;
    }

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

struct ComGuard(bool);

impl Drop for ComGuard {
    fn drop(&mut self) {
        UI_ENUMERATOR.with(|cell| *cell.borrow_mut() = None);
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn main() {
    unsafe {
        // Enable native Per-Monitor V2 DPI awareness: prevents any blur or bitmap scaling on high-DPI displays!
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Enable Windows 10/11 Dark Mode for Win32 menus: eliminates the white border!
        enable_dark_mode();

        let _com_guard = ComGuard(CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok());

        let (do_list, mut cfg) = parse();

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
            with_ui_enumerator(|en| {
                for (what, needle) in [("--src", &cfg.src), ("--dst", &cfg.dst)] {
                    if !needle.is_empty() {
                        if let Ok(Found::Many(n)) = find(en, needle) {
                            fatal(&format!(
                                "{what} \"{needle}\" matches {n} devices. Use the full endpoint ID from --list."
                            ));
                        }
                    }
                }
            });
        }

        // Detect if current Windows default audio device is our mirror source
        let initial_paused = with_ui_enumerator(|en| {
            let def_id = default_device_id(en);
            if let Some(ref cur_def) = def_id {
                if !cfg.src.is_empty() {
                    if cfg.src.starts_with('{') {
                        !cur_def.eq_ignore_ascii_case(&cfg.src)
                    } else if let Ok(Found::One(_, info)) = find(en, &cfg.src) {
                        !cur_def.eq_ignore_ascii_case(&info.id)
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        })
        .unwrap_or(false);

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
            initial_paused,
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

        // Trim process working set memory: releases cold startup pages, dropping resident RAM to ~3.5 MB!
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);

        // Win32 message loop (dormant when idle, 0.00% CPU)
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // Cleanup and graceful exit
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_CYCLE_ID);
        shared.quit.store(true, Ordering::Release);
        shared.signal_wake();
        let _ = audio_handle.join();
        GLOBAL_SHARED.store(ptr::null_mut(), Ordering::SeqCst);
        let _ = CloseHandle(wake_event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_icon_decodes_natively_at_256px() {
        unsafe {
            let hicon = CreateIconFromResourceEx(
                LOGO_PNG,
                true,
                0x0003_0000,
                0,
                0,
                LR_DEFAULTCOLOR,
            )
            .expect("CreateIconFromResourceEx should natively decode embedded PNG");
            assert!(!hicon.is_invalid());
            let _ = DestroyIcon(hicon);
        }
    }

    #[test]
    fn device_matching_and_helpers_work() {
        let info = DeviceInfo {
            id: "{0.0.0.00000000}.{ABC-123}".to_string(),
            name: "PGO32UFS (NVIDIA High Definition Audio)".to_string(),
        };
        assert!(is_match(&info, "{0.0.0.00000000}.{abc-123}"));
        assert!(is_match(&info, "pgo32ufs"));
        assert!(!is_match(&info, ""));
        assert_eq!(get_hotkey_description(MOD_CONTROL.0 | MOD_ALT.0, 0x41), "Ctrl+Alt+A");
        assert_eq!(trunc("Short", 10), "Short");
        assert_eq!(trunc("LongDeviceName", 4), "Long…");
    }

    #[test]
    fn mirror_write_math_handles_startup_jitter_overflow_and_silence() {
        // 48 kHz, 10 ms period (480), 3 ms cushion (144), +2 ms jitter headroom (96) -> high = 720
        let (period, cushion, high) = (480, 144, 720);

        // 1. Cold start (queued == 0) primes cushion + copies full packet
        assert_eq!(compute_mirror_write(period, 0, cushion, high, false), (144, 480));

        // 2. Steady state (queued == cushion) copies full packet with no extra prime
        assert_eq!(compute_mirror_write(period, 144, cushion, high, false), (0, 480));

        // 3. Early wakeup (+1.5 ms jitter above cushion) does NOT drop live audio frames
        assert_eq!(compute_mirror_write(period, 216, cushion, high, false), (0, 480));

        // 4. Genuine drift above high caps cleanly
        assert_eq!(compute_mirror_write(period, 300, cushion, high, false), (0, 420));

        // 5. Silent packet trims drift back so post-drain queued settles at cushion (216 + 408 - 480 = 144)
        assert_eq!(compute_mirror_write(period, 216, cushion, high, true), (0, 408));
    }

    #[test]
    fn formats_equal_compares_waveformat_cleanly() {
        let wf1 = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 192000,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        let mut wf2 = wf1;
        unsafe {
            assert!(formats_equal(&wf1, &wf2));
            wf2.nSamplesPerSec = 44100;
            assert!(!formats_equal(&wf1, &wf2));
        }
    }

    #[test]
    fn custom_menu_item_preencodes_utf16_and_updates() {
        let mut item = CustomMenuItem::new(1, MenuItemKind::FavoritesHeader, true, "★ Favorites", "▶");
        assert_eq!(item.text_len, 11);
        assert_eq!(item.text_u16.len(), 11);
        assert_eq!(item.shortcut_len, 1);
        item.set_text("★ Favorites (None set)");
        item.set_shortcut("");
        assert_eq!(item.text_len, 22);
        assert_eq!(item.shortcut_len, 0);
        assert!(item.shortcut_u16.is_empty());
    }
}
