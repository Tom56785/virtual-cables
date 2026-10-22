//! The native Win32 window: the header with the server, where the cables run
//! and the Windows Sound status, and the Options menu, the cable list and
//! add/remove buttons. The cables run in this process (on demand) or in the
//! Windows service, which the window then manages through its pipe.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CLEARTYPE_QUALITY, COLOR_3DLIGHT, COLOR_BTNFACE, COLOR_GRAYTEXT, COLOR_HOTLIGHT, COLOR_WINDOW,
    COLOR_WINDOWTEXT, CreateFontW, DEFAULT_CHARSET, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
    DeleteObject, DrawTextW, EndPaint, FW_NORMAL, FW_SEMIBOLD, FillRect, GetDC, GetDeviceCaps, GetSysColor,
    GetSysColorBrush, GetTextExtentPoint32W, HBRUSH, HDC, HFONT, InvalidateRect, LOGPIXELSY, PAINTSTRUCT, ReleaseDC,
    ScreenToClient, SelectObject, SetBkColor, SetBkMode, SetTextColor, TRANSPARENT, UpdateWindow,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    EM_SETLIMITTEXT, EM_SETSEL, ICC_LISTVIEW_CLASSES, ILC_COLOR32, INITCOMMONCONTROLSEX, ImageList_Create,
    ImageList_Destroy, InitCommonControlsEx, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_FOCUSED, LVIS_SELECTED,
    LVITEMW, LVM_DELETEALLITEMS, LVM_ENSUREVISIBLE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETCOLUMNWIDTH, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETIMAGELIST, LVM_SETITEMSTATE, LVM_SETITEMTEXTW,
    LVN_ITEMCHANGED, LVNI_SELECTED, LVS_EX_DOUBLEBUFFER, LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, LVSCW_AUTOSIZE_USEHEADER, LVSIL_SMALL, NMHDR, SetWindowTheme, WC_LISTVIEWW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    ExtractIconExW, FOLDERID_Downloads, SHGetKnownFolderPath, SetCurrentProcessExplicitAppUserModelID, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::attach::{combined_output, hide_command_window, single_line};
use super::control::{self, Snapshot, Tone};
use super::download::{download_latest_usbip_installer, is_name_resolution_error};
use super::remote::{self, Reply, Request};
use super::service::{ServiceState, run_elevated, service_state, start_service};
use super::win::{system_proc, wide};
use crate::config::{
    CONFIG_FILE, Cable, MAX_CABLE_NAME, MAX_CABLES, application_data_dir, config_cables, config_file_path,
    load_config_content, new_cable, parse_config, service_data_dir,
};
use crate::logf;
use crate::{AppServer, VERSION};

const ID_ADD: i32 = 1001;
const ID_REMOVE: i32 = 1002;
const ID_LIST: i32 = 1003;
const ID_DRIVER: i32 = 1005;
const ID_SETTINGS: i32 = 1006;
const ID_OPTIONS: i32 = 1007;
const ID_SYNC: i32 = 1008;
const ID_SERVICE: i32 = 1009;
const ID_UNINSTALL: i32 = 1010;

/// Posted from any thread when what the window shows may have changed.
const WM_APP_REFRESH: u32 = WM_APP + 1;
/// Posted to run the cables in this process; a non-zero WPARAM loads them
/// from config.toml instead of keeping the ones it had.
const WM_APP_RUN_ON_DEMAND: u32 = WM_APP + 2;

const BS_PUSHBUTTON_STYLE: u32 = 0;

// Layout, in 96-DPI pixels; `scale` converts them for the window's monitor.
const MARGIN: i32 = 16;
const GAP: i32 = 8;
const HEADER_HEIGHT: i32 = 114;
const BUTTON_HEIGHT: i32 = 30;
const OPTIONS_Y: i32 = 12;
const CABLE_BUTTONS_Y: i32 = 50;
const BUTTON_WIDTH: i32 = 128; // every header button, so they line up
const ROW_HEIGHT: i32 = 28;
const CABLE_COLUMN_WIDTH: i32 = 200;
const BUS_COLUMN_WIDTH: i32 = 130;
const WINDOW_WIDTH: i32 = 820;
const WINDOW_HEIGHT: i32 = 540;
// Leaves the header's status lines about 400 px beside the cable buttons.
const MIN_WINDOW_WIDTH: i32 = 720;
const MIN_WINDOW_HEIGHT: i32 = 360;

const OK_COLOR: COLORREF = rgb(16, 124, 16);
const PENDING_COLOR: COLORREF = rgb(214, 136, 0);
const ERROR_COLOR: COLORREF = rgb(196, 43, 28);

const USBIP_RELEASE_PAGE: &str = "https://github.com/vadimgrn/usbip-win2/releases/latest";

static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);
static ADD_BUTTON: AtomicIsize = AtomicIsize::new(0);
static REMOVE_BUTTON: AtomicIsize = AtomicIsize::new(0);
static OPTIONS_BUTTON: AtomicIsize = AtomicIsize::new(0);
static OPTIONS_MENU: AtomicIsize = AtomicIsize::new(0);
static LIST_VIEW: AtomicIsize = AtomicIsize::new(0);
static UI_FONT: AtomicIsize = AtomicIsize::new(0);
static TITLE_FONT: AtomicIsize = AtomicIsize::new(0);
static DPI: AtomicU32 = AtomicU32::new(96);
/// The window's own message for the status line, such as the progress of a
/// driver installation, with the Windows Sound status it covers: it shows
/// until that status changes.
static NOTICE: Mutex<Option<Notice>> = Mutex::new(None);
/// Where the status line's Retry link was last painted, in client
/// coordinates; None while there is no link.
static RETRY_LINK: Mutex<Option<RECT>> = Mutex::new(None);
/// The cables the list shows, and whether the server was running.
static LISTED: Mutex<(Vec<Cable>, bool)> = Mutex::new((Vec::new(), true));
static SERVICE: Mutex<ServiceView> = Mutex::new(ServiceView {
    mode: Mode::OnDemand,
    state: ServiceState::NotInstalled,
    connected: false,
    remote: Snapshot { cables: Vec::new(), server_error: String::new(), status: None },
    version: String::new(),
    busy: None,
});

struct Notice {
    line: (Tone, String),
    covers: Option<(Tone, String)>,
}

/// Where the cables run.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// In this process, until the window closes.
    OnDemand,
    /// In the Windows service, which this window manages.
    Service,
}

/// What the window knows of the Windows service.
#[derive(Clone, PartialEq)]
struct ServiceView {
    mode: Mode,
    state: ServiceState,
    /// Whether the service answered the last request.
    connected: bool,
    /// The service's cables and status, as last reported.
    remote: Snapshot,
    /// The service's version, as last reported.
    version: String,
    /// What the window is doing to the service, while it does it.
    busy: Option<&'static str>,
}

fn tone_color(tone: Tone) -> COLORREF {
    match tone {
        Tone::Ok => OK_COLOR,
        Tone::Pending => PENDING_COLOR,
        Tone::Error => ERROR_COLOR,
    }
}

type GetDpiForWindowFn = unsafe extern "system" fn(HWND) -> u32;

fn handle(slot: &AtomicIsize) -> HWND {
    slot.load(Ordering::SeqCst) as HWND
}

fn store(slot: &AtomicIsize, hwnd: HWND) {
    slot.store(hwnd as isize, Ordering::SeqCst);
}

pub fn main_window() -> HWND {
    handle(&MAIN_WINDOW)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

fn service_view() -> ServiceView {
    lock(&SERVICE).clone()
}

fn mode() -> Mode {
    lock(&SERVICE).mode
}

/// The server and cables the window shows, wherever they run.
fn view() -> Snapshot {
    let service = service_view();
    match service.mode {
        Mode::OnDemand => control::snapshot(),
        Mode::Service if service.connected => service.remote,
        Mode::Service => Snapshot { server_error: "the Windows service is not running".into(), ..service.remote },
    }
}

/// Asks for a repaint from the latest state; safe from any thread.
fn post_refresh() {
    let window = main_window();
    if !window.is_null() {
        // SAFETY: posting to a window is safe from any thread.
        unsafe { PostMessageW(window, WM_APP_REFRESH, 0, 0) };
    }
}

/// Records a reply from the service.
fn record_reply(reply: &Reply) {
    let mut service = lock(&SERVICE);
    service.connected = true;
    service.remote = reply.snapshot.clone();
    service.version = reply.version.clone();
}

fn set_busy(busy: Option<&'static str>) {
    lock(&SERVICE).busy = busy;
    post_refresh();
}

pub fn run_gui(manager: &Arc<AppServer>) {
    control::init(manager);
    control::on_change(post_refresh);
    let logger = control::logger();
    // A running service has the cables; otherwise this window runs them. One
    // still starting, as at sign-in, is given the time to.
    let service = call_service_within(if service_state() == ServiceState::Pending { 15 } else { 0 });
    match &service {
        Ok(reply) => {
            record_reply(reply);
            lock(&SERVICE).mode = Mode::Service;
            logf!(logger, "Managing the Windows service, version {}", reply.version);
        }
        Err(_) => logf!(logger, "Running on demand"),
    }
    lock(&SERVICE).state = service_state();

    // SAFETY: Win32 window creation and the message loop on this thread; all
    // strings are NUL-terminated UTF-16 that outlive the calls.
    unsafe {
        // Give the taskbar a stable application identity even when launched from an MSIX.
        SetCurrentProcessExplicitAppUserModelID(wide("VirtualCables.VirtualCables").as_ptr());

        let controls =
            INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_LISTVIEW_CLASSES };
        InitCommonControlsEx(&controls);
        // The window is created at the primary monitor's DPI; WM_CREATE and
        // WM_DPICHANGED pick up the DPI of the monitor it actually lands on.
        DPI.store(screen_dpi(), Ordering::SeqCst);

        let instance = GetModuleHandleW(null());
        let (mut icon_big, mut icon_small) = load_application_icons(instance);
        if icon_big.is_null() {
            icon_big = LoadIconW(null_mut(), IDI_APPLICATION);
        }
        if icon_small.is_null() {
            icon_small = icon_big;
        }

        let class_name = wide("VirtualCablesNativeWindow");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(window_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: icon_big,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as isize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
            hIconSm: icon_small,
        };
        if RegisterClassExW(&class) == 0 {
            show_error("Could not register the native window class.");
            return;
        }

        let window = CreateWindowExW(
            0,
            class_name.as_ptr(),
            wide("Virtual Cables").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            scale(WINDOW_WIDTH),
            scale(WINDOW_HEIGHT),
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if window.is_null() {
            show_error("Could not create the Virtual Cables window.");
            return;
        }

        // Explicitly set both the window-class icons and the per-window icons.
        // Windows can otherwise keep showing a generic executable icon in the
        // title bar or taskbar even though the PE resource contains our ICO.
        set_window_and_class_icons(window, icon_big, icon_small);
        ShowWindow(window, SW_SHOW);
        UpdateWindow(window);
        set_window_and_class_icons(window, icon_big, icon_small);
        if mode() == Mode::OnDemand {
            if service_view().state == ServiceState::Stopped && ask_to_start_service() {
                run_switch("Starting the Windows service\u{2026}", || switch_to_service(false));
            } else {
                run_on_demand(manager.cables());
            }
        }
        std::thread::Builder::new().name("service monitor".into()).spawn(monitor_service).ok();

        let mut message: MSG = std::mem::zeroed();
        while GetMessageW(&mut message, null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// The Options button's dropdown: synchronization, the Windows service, the
/// settings folder and the driver installer.
unsafe fn create_options_menu() -> HMENU {
    // SAFETY: menu creation with NUL-terminated strings that outlive each call.
    unsafe {
        let menu = CreatePopupMenu();
        AppendMenuW(menu, MF_STRING, ID_SYNC as usize, wide("S&ync with Windows Sound").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        // update_options_menu names this item for the service's state.
        AppendMenuW(menu, MF_STRING, ID_SERVICE as usize, wide("&Install as Windows service").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_UNINSTALL as usize, wide("&Uninstall Windows service").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        AppendMenuW(menu, MF_STRING, ID_SETTINGS as usize, wide("Open &settings folder").as_ptr());
        AppendMenuW(menu, MF_STRING, ID_DRIVER as usize, wide("&Download and install driver").as_ptr());
        menu
    }
}

/// Names and enables the menu items for the cables' and the service's state.
fn update_options_menu(menu: HMENU) {
    let service = service_view();
    let idle = service.busy.is_none();
    let state = |enabled: bool| if enabled { MF_ENABLED } else { MF_GRAYED };
    let (text, enabled) = match service.state {
        ServiceState::NotInstalled => ("&Install as Windows service", idle),
        ServiceState::Stopped => ("S&tart Windows service", idle),
        ServiceState::Pending | ServiceState::Running => ("S&tart Windows service", false),
    };
    let reachable = service.mode == Mode::OnDemand || service.connected;
    // SAFETY: menu calls on the Options menu with a NUL-terminated string.
    unsafe {
        let flags = MF_BYCOMMAND | MF_STRING | state(enabled);
        ModifyMenuW(menu, ID_SERVICE as u32, flags, ID_SERVICE as usize, wide(text).as_ptr());
        let installed = service.state != ServiceState::NotInstalled;
        EnableMenuItem(menu, ID_UNINSTALL as u32, MF_BYCOMMAND | state(idle && installed));
        EnableMenuItem(menu, ID_SYNC as u32, MF_BYCOMMAND | state(idle && reachable));
    }
}

/// Drops the Options menu down from under the Options button, right-aligned
/// with it. The chosen item arrives as an ordinary WM_COMMAND.
unsafe fn show_options_menu(window: HWND) {
    // SAFETY: GetWindowRect and TrackPopupMenu on valid handles of this thread.
    unsafe {
        let mut button: RECT = std::mem::zeroed();
        GetWindowRect(handle(&OPTIONS_BUTTON), &mut button);
        let menu = OPTIONS_MENU.load(Ordering::SeqCst) as HMENU;
        update_options_menu(menu);
        TrackPopupMenu(menu, TPM_RIGHTALIGN | TPM_TOPALIGN, button.right, button.bottom, 0, window, null());
    }
}

/// Loads the application icon: the embedded resource, an ICO file next to the
/// executable, or the executable's first icon, in that order.
unsafe fn load_application_icons(instance: *mut c_void) -> (HICON, HICON) {
    let pair = |large: HICON, small: HICON| -> Option<(HICON, HICON)> {
        match (large.is_null(), small.is_null()) {
            (true, true) => None,
            (true, false) => Some((small, small)),
            (false, true) => Some((large, large)),
            (false, false) => Some((large, small)),
        }
    };
    // SAFETY: icon loading with valid instance handles, paths and out pointers.
    unsafe {
        // First the icon group the build embeds as resource 1.
        if let Some(icons) = pair(load_embedded_icon(instance, 32), load_embedded_icon(instance, 16)) {
            return icons;
        }

        let Ok(exe) = std::env::current_exe() else {
            return (null_mut(), null_mut());
        };
        // Also support a copied ICO as a fallback, in case the resources were
        // left out of the build.
        if let Some(dir) = exe.parent() {
            let ico_path = dir.join("ICON").join("Virtual Cables.ico");
            if ico_path.exists() {
                let ico = wide(&ico_path.display().to_string());
                let large = LoadImageW(null_mut(), ico.as_ptr(), IMAGE_ICON, 32, 32, LR_LOADFROMFILE);
                let small = LoadImageW(null_mut(), ico.as_ptr(), IMAGE_ICON, 16, 16, LR_LOADFROMFILE);
                if let Some(icons) = pair(large, small) {
                    return icons;
                }
            }
        }

        // Finally extract the first icon from the running executable.
        let (mut large, mut small): (HICON, HICON) = (null_mut(), null_mut());
        let path = wide(&exe.display().to_string());
        if ExtractIconExW(path.as_ptr(), 0, &mut large, &mut small, 1) > 0
            && let Some(icons) = pair(large, small)
        {
            return icons;
        }
        (null_mut(), null_mut())
    }
}

unsafe fn load_embedded_icon(instance: *mut c_void, size: i32) -> HICON {
    let resource = std::ptr::without_provenance::<u16>(1); // MAKEINTRESOURCE(1)
    // SAFETY: loads resource 1 of this module.
    unsafe {
        let icon = LoadImageW(instance, resource, IMAGE_ICON, size, size, 0);
        if !icon.is_null() {
            return icon;
        }
        LoadIconW(instance, resource)
    }
}

unsafe fn set_window_and_class_icons(window: HWND, large: HICON, small: HICON) {
    // SAFETY: the window handle is valid on this thread.
    unsafe {
        if !large.is_null() {
            SetClassLongPtrW(window, GCLP_HICON, large as isize);
            SendMessageW(window, WM_SETICON, ICON_BIG as WPARAM, large as LPARAM);
        }
        if !small.is_null() {
            SetClassLongPtrW(window, GCLP_HICONSM, small as isize);
            SendMessageW(window, WM_SETICON, ICON_SMALL as WPARAM, small as LPARAM);
        }
    }
}

fn loword(v: usize) -> u32 {
    (v & 0xffff) as u32
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on the GUI thread with valid arguments.
    unsafe {
        match message {
            WM_CREATE => {
                store(&MAIN_WINDOW, window);
                DPI.store(window_dpi(window), Ordering::SeqCst);
                create_fonts();
                let push = WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON_STYLE;
                store(&ADD_BUTTON, create_control(0, "BUTTON", "+  Add cable", push, window, ID_ADD));
                store(&REMOVE_BUTTON, create_control(0, "BUTTON", "\u{2212}  Remove cable", push, window, ID_REMOVE));
                store(&OPTIONS_BUTTON, create_control(0, "BUTTON", "Options  \u{25be}", push, window, ID_OPTIONS));
                OPTIONS_MENU.store(create_options_menu() as isize, Ordering::SeqCst);
                store(&LIST_VIEW, create_list_view(window));
                layout(window);
                refresh_list(None);
                return 0;
            }
            WM_SIZE => {
                layout(window);
                return 0;
            }
            WM_PAINT => {
                paint(window);
                return 0;
            }
            // The buttons sit on the gray header band, so their themed corners
            // must blend with that rather than the window background.
            WM_CTLCOLORBTN => {
                return GetSysColorBrush(COLOR_BTNFACE) as LRESULT;
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(lparam as *mut MINMAXINFO);
                info.ptMinTrackSize.x = scale(MIN_WINDOW_WIDTH);
                info.ptMinTrackSize.y = scale(MIN_WINDOW_HEIGHT);
                return 0;
            }
            WM_DPICHANGED => {
                DPI.store(loword(wparam), Ordering::SeqCst);
                create_fonts();
                set_row_height(handle(&LIST_VIEW));
                // Windows suggests a rect that keeps the window's physical size
                // proportional on the new monitor; WM_SIZE then lays it out.
                let r = &*(lparam as *const RECT);
                SetWindowPos(
                    window,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                layout(window);
                InvalidateRect(window, null(), 1);
                return 0;
            }
            WM_COMMAND => match loword(wparam) as i32 {
                ID_ADD => {
                    add_cable();
                    return 0;
                }
                ID_REMOVE => {
                    remove_cable();
                    return 0;
                }
                ID_DRIVER => {
                    download_and_install_driver();
                    return 0;
                }
                ID_SETTINGS => {
                    open_settings_folder_from_gui();
                    return 0;
                }
                ID_OPTIONS => {
                    show_options_menu(window);
                    return 0;
                }
                ID_SYNC => {
                    sync_now();
                    return 0;
                }
                ID_SERVICE => {
                    match service_view().state {
                        ServiceState::NotInstalled => install_service_from_gui(),
                        ServiceState::Stopped => start_service_from_gui(),
                        ServiceState::Pending | ServiceState::Running => {}
                    }
                    return 0;
                }
                ID_UNINSTALL => {
                    uninstall_service_from_gui();
                    return 0;
                }
                _ => {}
            },
            WM_APP_REFRESH => {
                refresh();
                return 0;
            }
            WM_APP_RUN_ON_DEMAND => {
                let cables = if wparam != 0 {
                    config_cables(&parse_config(&load_config_content()).unwrap_or_default())
                } else {
                    control::snapshot().cables
                };
                run_on_demand(cables);
                return 0;
            }
            WM_LBUTTONUP => {
                let (x, y) = ((lparam & 0xffff) as i16 as i32, ((lparam >> 16) & 0xffff) as i16 as i32);
                if on_retry_link(x, y) {
                    sync_now();
                }
                return 0;
            }
            WM_SETCURSOR if wparam as HWND == window && loword(lparam as usize) == HTCLIENT => {
                let mut point = POINT { x: 0, y: 0 };
                GetCursorPos(&mut point);
                ScreenToClient(window, &mut point);
                if on_retry_link(point.x, point.y) {
                    SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                    return 1;
                }
            }
            // Remove acts on the selected cable, so it follows the selection.
            WM_NOTIFY => {
                let header = &*(lparam as *const NMHDR);
                if header.idFrom == ID_LIST as usize && header.code == LVN_ITEMCHANGED {
                    update_cable_buttons();
                }
            }
            WM_DESTROY => {
                // The service keeps its cables after the window closes.
                if mode() == Mode::OnDemand {
                    control::stop();
                }
                PostQuitMessage(0);
                return 0;
            }
            _ => {}
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Converts a 96-DPI length to pixels at the window's current DPI.
fn scale(value: i32) -> i32 {
    let dpi = DPI.load(Ordering::SeqCst) as i32;
    (value * dpi + 48) / 96
}

/// The DPI of the primary monitor (the system DPI).
fn screen_dpi() -> u32 {
    // SAFETY: a screen DC, released before returning.
    unsafe {
        let dc = GetDC(null_mut());
        let dpi = GetDeviceCaps(dc, LOGPIXELSY as i32);
        ReleaseDC(null_mut(), dc);
        if dpi > 0 { dpi as u32 } else { 96 }
    }
}

/// The DPI of the monitor showing `window`. GetDpiForWindow needs Windows 10
/// 1607, so older systems use the system DPI.
fn window_dpi(window: HWND) -> u32 {
    if let Some(get_dpi) = system_proc::<GetDpiForWindowFn>("user32.dll", c"GetDpiForWindow") {
        // SAFETY: the export has this signature; the window handle is valid.
        let dpi = unsafe { get_dpi(window) };
        if dpi > 0 {
            return dpi;
        }
    }
    screen_dpi()
}

/// (Re)creates the UI and title fonts for the current DPI and applies them to
/// every control, deleting the fonts they replace.
unsafe fn create_fonts() {
    let font = |points: i32, weight: u32, face: &str| -> HFONT {
        let height = -(points * DPI.load(Ordering::SeqCst) as i32 + 36) / 72;
        let face = wide(face);
        // SAFETY: a font from a NUL-terminated face name.
        unsafe {
            CreateFontW(
                height,
                0,
                0,
                0,
                weight as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                0,
                0,
                CLEARTYPE_QUALITY as u32,
                0,
                face.as_ptr(),
            )
        }
    };
    let ui = font(9, FW_NORMAL, "Segoe UI");
    let title = font(15, FW_SEMIBOLD, "Segoe UI Semibold");
    let old_ui = UI_FONT.swap(ui as isize, Ordering::SeqCst);
    let old_title = TITLE_FONT.swap(title as isize, Ordering::SeqCst);
    // SAFETY: WM_SETFONT to valid (or null, ignored) controls; the old fonts
    // are no longer selected anywhere once replaced.
    unsafe {
        for slot in [&ADD_BUTTON, &REMOVE_BUTTON, &OPTIONS_BUTTON, &LIST_VIEW] {
            let control = handle(slot);
            if !control.is_null() {
                SendMessageW(control, WM_SETFONT, ui as WPARAM, 1);
            }
        }
        for old in [old_ui, old_title] {
            if old != 0 {
                DeleteObject(old as _);
            }
        }
    }
}

fn ui_font() -> HFONT {
    UI_FONT.load(Ordering::SeqCst) as HFONT
}

unsafe fn create_control(ex_style: u32, class: &str, text: &str, style: u32, parent: HWND, id: i32) -> HWND {
    let (class, text) = (wide(class), wide(text));
    // SAFETY: creates a child window of a valid parent on the GUI thread;
    // `layout` positions it.
    unsafe {
        let control = CreateWindowExW(
            ex_style,
            class.as_ptr(),
            text.as_ptr(),
            style,
            0,
            0,
            0,
            0,
            parent,
            id as isize as HMENU,
            null_mut(),
            null(),
        );
        if !control.is_null() {
            SendMessageW(control, WM_SETFONT, ui_font() as WPARAM, 1);
        }
        control
    }
}

/// A report-mode ListView with Explorer styling and one row per cable.
unsafe fn create_list_view(parent: HWND) -> HWND {
    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS | LVS_NOSORTHEADER;
    // SAFETY: ListView messages to the control created here, with structs and
    // strings that outlive each call.
    unsafe {
        let list = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEWW,
            null(),
            style,
            0,
            0,
            0,
            0,
            parent,
            ID_LIST as isize as HMENU,
            null_mut(),
            null(),
        );
        if list.is_null() {
            return list;
        }
        SendMessageW(list, WM_SETFONT, ui_font() as WPARAM, 1);
        SetWindowTheme(list, wide("Explorer").as_ptr(), null());
        let extended = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
        SendMessageW(list, LVM_SETEXTENDEDLISTVIEWSTYLE, extended as WPARAM, extended as LPARAM);
        for (index, (title, width)) in
            [("Name", CABLE_COLUMN_WIDTH), ("USB/IP bus ID", BUS_COLUMN_WIDTH), ("Status", 0)].into_iter().enumerate()
        {
            let mut title = wide(title);
            let mut column: LVCOLUMNW = std::mem::zeroed();
            column.mask = LVCF_TEXT | LVCF_WIDTH;
            column.cx = scale(width);
            column.pszText = title.as_mut_ptr();
            SendMessageW(list, LVM_INSERTCOLUMNW, index, &column as *const _ as LPARAM);
        }
        set_row_height(list);
        list
    }
}

/// ListView rows are as tall as their small icons, so an empty image list of
/// the wanted height gives the rows some breathing room.
fn set_row_height(list: HWND) {
    // SAFETY: image-list calls; the list view returns the image list it
    // replaces, which nothing else owns.
    unsafe {
        let images = ImageList_Create(1, scale(ROW_HEIGHT), ILC_COLOR32, 0, 0);
        let old = SendMessageW(list, LVM_SETIMAGELIST, LVSIL_SMALL as WPARAM, images as LPARAM);
        if old != 0 {
            ImageList_Destroy(old);
        }
    }
}

/// Positions the controls for the window's client size and DPI.
fn layout(window: HWND) {
    // SAFETY: GetClientRect and MoveWindow on valid windows of this thread.
    unsafe {
        let mut client: RECT = std::mem::zeroed();
        GetClientRect(window, &mut client);
        let (w, h) = (client.right, client.bottom);
        if w == 0 || h == 0 {
            return; // minimized
        }
        let x = HeaderButtons::for_width(w);
        let button_h = scale(BUTTON_HEIGHT);
        MoveWindow(handle(&OPTIONS_BUTTON), x.options, scale(OPTIONS_Y), scale(BUTTON_WIDTH), button_h, 1);
        MoveWindow(handle(&ADD_BUTTON), x.add, scale(CABLE_BUTTONS_Y), scale(BUTTON_WIDTH), button_h, 1);
        MoveWindow(handle(&REMOVE_BUTTON), x.remove, scale(CABLE_BUTTONS_Y), scale(BUTTON_WIDTH), button_h, 1);

        let list_y = scale(HEADER_HEIGHT) + scale(MARGIN);
        let list_w = (w - 2 * scale(MARGIN)).max(scale(100));
        let list_h = (h - scale(MARGIN) - list_y).max(scale(60));
        let list = handle(&LIST_VIEW);
        MoveWindow(list, scale(MARGIN), list_y, list_w, list_h, 1);
        SendMessageW(list, LVM_SETCOLUMNWIDTH, 0, scale(CABLE_COLUMN_WIDTH) as LPARAM);
        SendMessageW(list, LVM_SETCOLUMNWIDTH, 1, scale(BUS_COLUMN_WIDTH) as LPARAM);
        SendMessageW(list, LVM_SETCOLUMNWIDTH, 2, LVSCW_AUTOSIZE_USEHEADER as LPARAM);
    }
}

/// The left edges of the header's buttons, which are right-aligned: Options
/// beside the title, Add and Remove beneath it.
struct HeaderButtons {
    options: i32,
    add: i32,
    remove: i32,
}

impl HeaderButtons {
    fn for_width(client_width: i32) -> Self {
        let right = client_width - scale(MARGIN);
        let remove = right - scale(BUTTON_WIDTH);
        HeaderButtons { options: right - scale(BUTTON_WIDTH), add: remove - scale(GAP) - scale(BUTTON_WIDTH), remove }
    }
}

fn header_rect(window: HWND) -> RECT {
    let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    // SAFETY: GetClientRect on a valid window.
    unsafe { GetClientRect(window, &mut client) };
    RECT { left: 0, top: 0, right: client.right, bottom: scale(HEADER_HEIGHT) }
}

/// Repaints the header; safe from any thread.
fn invalidate_header() {
    let window = main_window();
    if window.is_null() {
        return;
    }
    let header = header_rect(window);
    // SAFETY: InvalidateRect on a valid window; safe from any thread.
    unsafe { InvalidateRect(window, &header, 0) };
}

/// The USB/IP server's state and cable count, for the header.
fn server_summary(view: &Snapshot) -> (Tone, String) {
    if !view.server_error.is_empty() {
        return (Tone::Error, "USB/IP server error \u{00b7} see the diagnostic log in the settings folder".into());
    }
    let count = view.cables.len();
    let cables = match count {
        0 => "no cables yet".to_string(),
        1 => "1 cable".to_string(),
        _ => format!("{count} cables"),
    };
    (Tone::Ok, format!("USB/IP server running \u{00b7} {cables}"))
}

/// Where the cables run, for the header: in this window or in the service.
fn mode_summary(service: &ServiceView) -> (Tone, String) {
    if let Some(busy) = service.busy {
        return (Tone::Pending, busy.to_string());
    }
    let text = match (service.mode, service.state) {
        (Mode::Service, _) if service.connected && service.version != VERSION => {
            format!("Running as a Windows service \u{00b7} the service is version {}", service.version)
        }
        (Mode::Service, _) if service.connected => {
            "Running as a Windows service \u{00b7} the cables stay after this window closes".into()
        }
        (Mode::Service, ServiceState::Running) => {
            return (
                Tone::Error,
                "The Windows service is not answering \u{00b7} see its log in the settings folder".into(),
            );
        }
        (Mode::Service, _) => {
            return (Tone::Error, "The Windows service is not running \u{00b7} start it from the Options menu".into());
        }
        (Mode::OnDemand, ServiceState::NotInstalled) => {
            "Running on demand \u{00b7} the cables stop when this window closes".into()
        }
        (Mode::OnDemand, ServiceState::Running) => {
            return (
                Tone::Error,
                "Running on demand, but the Windows service runs too \u{00b7} restart Virtual Cables".into(),
            );
        }
        (Mode::OnDemand, _) => "Running on demand \u{00b7} the Windows service is installed but not running".into(),
    };
    (Tone::Ok, text)
}

/// The status line: the window's own notice, until the Windows Sound status it
/// covers changes, then that status.
fn status_line(view: &Snapshot) -> Option<(Tone, String)> {
    let mut notice = lock(&NOTICE);
    match notice.as_ref() {
        Some(n) if n.covers == view.status => return Some(n.line.clone()),
        Some(_) => *notice = None,
        None => {}
    }
    view.status.clone()
}

/// Paints the header band: the title, the server, mode and status lines.
unsafe fn paint(window: HWND) {
    // SAFETY: painting between BeginPaint and EndPaint with stock brushes and
    // this module's fonts; the original font is restored before EndPaint.
    unsafe {
        let mut paint: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(window, &mut paint);
        let header = header_rect(window);
        FillRect(dc, &header, GetSysColorBrush(COLOR_BTNFACE));
        let separator = RECT { top: header.bottom - scale(1).max(1), ..header };
        FillRect(dc, &separator, GetSysColorBrush(COLOR_3DLIGHT));

        SetBkMode(dc, TRANSPARENT as i32);
        let format = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
        // Text stops short of the buttons beside it.
        let buttons = HeaderButtons::for_width(header.right);
        let old_font = SelectObject(dc, TITLE_FONT.load(Ordering::SeqCst) as _);
        SetTextColor(dc, GetSysColor(COLOR_WINDOWTEXT));
        let title_right = buttons.options - scale(GAP);
        let title = RECT { left: scale(MARGIN), top: scale(10), right: title_right, bottom: scale(42) };
        draw_text(dc, "Virtual Cables", title, format);

        SelectObject(dc, ui_font() as _);
        let view = view();
        let (tone, summary) = server_summary(&view);
        let right = buttons.add - scale(2 * GAP);
        let line = RECT { left: scale(MARGIN), top: scale(42), right, bottom: scale(62) };
        draw_status_line(dc, line, tone, &summary, None);
        let (tone, mode) = mode_summary(&service_view());
        draw_status_line(dc, RECT { top: scale(62), bottom: scale(82), ..line }, tone, &mode, None);
        // A failed status line offers a retry.
        let retry = status_line(&view).and_then(|(tone, text)| {
            let link = matches!(tone, Tone::Error).then_some("Retry");
            draw_status_line(dc, RECT { top: scale(82), bottom: scale(102), ..line }, tone, &text, link)
        });
        *lock(&RETRY_LINK) = retry;

        SelectObject(dc, old_font);
        EndPaint(window, &paint);
    }
}

/// One header line: a dot in the tone's colour, then gray text, then `link`
/// in the link colour if given. Returns where the link was drawn.
unsafe fn draw_status_line(dc: HDC, line: RECT, tone: Tone, text: &str, link: Option<&str>) -> Option<RECT> {
    let format = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
    // SAFETY: text measurement and drawing into a valid DC.
    unsafe {
        SetTextColor(dc, tone_color(tone));
        draw_text(dc, "\u{25cf}", line, format);
        let text_left = line.left + text_width(dc, "\u{25cf}") + scale(6);
        let mut text_rect = RECT { left: text_left, ..line };
        let mut link_rect = None;
        if let Some(link) = link {
            // The link follows the text, which gives up room to it if needed.
            let (link_width, gap) = (text_width(dc, link), scale(8));
            text_rect.right = (line.right - link_width - gap).max(text_left);
            let left = text_left + text_width(dc, text).min(text_rect.right - text_left) + gap;
            let rect = RECT { left, right: left + link_width, ..line };
            SetTextColor(dc, GetSysColor(COLOR_HOTLIGHT));
            draw_text(dc, link, rect, format);
            link_rect = Some(rect);
        }
        SetTextColor(dc, GetSysColor(COLOR_GRAYTEXT));
        draw_text(dc, text, text_rect, format);
        link_rect
    }
}

unsafe fn text_width(dc: HDC, text: &str) -> i32 {
    let text: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE { cx: 0, cy: 0 };
    // SAFETY: measures `text`, whose length is passed, in a valid DC.
    unsafe { GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size) };
    size.cx
}

unsafe fn draw_text(dc: HDC, text: &str, mut rect: RECT, format: u32) {
    let text = wide(text);
    // SAFETY: a NUL-terminated string drawn into a valid DC.
    unsafe { DrawTextW(dc, text.as_ptr(), -1, &mut rect, format) };
}

unsafe fn set_item_text(list: HWND, item: i32, sub_item: i32, text: &str) {
    let mut text = wide(text);
    // SAFETY: LVITEMW and its text outlive the call.
    unsafe {
        let mut row: LVITEMW = std::mem::zeroed();
        row.mask = LVIF_TEXT;
        row.iItem = item;
        row.iSubItem = sub_item;
        row.pszText = text.as_mut_ptr();
        if sub_item == 0 {
            SendMessageW(list, LVM_INSERTITEMW, 0, &row as *const _ as LPARAM);
        } else {
            SendMessageW(list, LVM_SETITEMTEXTW, item as WPARAM, &row as *const _ as LPARAM);
        }
    }
}

/// Brings the window up to date: the list if the cables or the server
/// changed, keeping the selected cable selected, and the header.
fn refresh() {
    let view = view();
    let listed = lock(&LISTED).clone();
    if listed == (view.cables.clone(), view.server_error.is_empty()) {
        update_cable_buttons();
        invalidate_header();
        return;
    }
    let selected = selected_index().and_then(|i| listed.0.get(i)).map(|c| c.serial.clone());
    refresh_list(selected.and_then(|serial| view.cables.iter().position(|c| c.serial == serial)));
}

/// Refills the list from the cables, selecting row `select` if given.
fn refresh_list(select: Option<usize>) {
    let list = handle(&LIST_VIEW);
    let view = view();
    let running = view.server_error.is_empty();
    let row_status = if running { "Ready" } else { "Server error \u{2013} open the log" };
    let cables = view.cables;
    *lock(&LISTED) = (cables.clone(), running);
    // SAFETY: ListView messages to a valid control with structs and strings
    // that outlive each call.
    unsafe {
        SendMessageW(list, LVM_DELETEALLITEMS, 0, 0);
        for (item, cable) in cables.iter().enumerate() {
            let item = item as i32;
            set_item_text(list, item, 0, &cable.name);
            set_item_text(list, item, 1, &format!("1-{}", cable.id));
            set_item_text(list, item, 2, row_status);
        }
        if let Some(select) = select.filter(|&i| i < cables.len()) {
            let mut row: LVITEMW = std::mem::zeroed();
            row.state = LVIS_SELECTED | LVIS_FOCUSED;
            row.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
            SendMessageW(list, LVM_SETITEMSTATE, select, &row as *const _ as LPARAM);
            SendMessageW(list, LVM_ENSUREVISIBLE, select, 0);
        }
    }
    update_cable_buttons();
    invalidate_header();
}

/// The selected row of the cable list.
fn selected_index() -> Option<usize> {
    // SAFETY: LVM_GETNEXTITEM on the list view; -1 starts the search at the top.
    let index = unsafe { SendMessageW(handle(&LIST_VIEW), LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as LPARAM) };
    usize::try_from(index).ok()
}

/// Add needs room for another cable; Remove needs a selected cable. Both need
/// the cables' server to be reachable, and not to be handed over.
fn update_cable_buttons() {
    let count = lock(&LISTED).0.len();
    let service = service_view();
    let usable = service.busy.is_none() && (service.mode == Mode::OnDemand || service.connected);
    // SAFETY: EnableWindow on valid control handles.
    unsafe {
        EnableWindow(handle(&ADD_BUTTON), (usable && count < MAX_CABLES) as i32);
        EnableWindow(handle(&REMOVE_BUTTON), (usable && selected_index().is_some()) as i32);
    }
}

/// Changes the cables wherever they run: in this process, or in the service.
fn change(request: Request) -> Result<(), String> {
    if mode() == Mode::OnDemand {
        return match request {
            Request::Status => Ok(()),
            Request::Add(name) => control::add_cable(name),
            Request::Remove(serial) => control::remove_cable(serial),
            Request::Sync => control::sync_now(),
        };
    }
    match remote::call(&request) {
        Ok(reply) => {
            record_reply(&reply);
            reply.result
        }
        Err(err) => {
            lock(&SERVICE).connected = false;
            Err(format!("Could not reach the Windows service:\n{err}"))
        }
    }
}

fn add_cable() {
    let cables = view().cables;
    if cables.len() >= MAX_CABLES {
        show_error(&format!("A maximum of {MAX_CABLES} virtual cables is supported."));
        return;
    }
    let Some(cable) = prompt_new_cable(&cables) else {
        return;
    };
    let result = change(Request::Add(&cable.name));
    if let Err(err) = &result {
        show_error(err);
    }
    // Select the new cable, which comes last.
    let select = view().cables.iter().position(|c| c.name == cable.name);
    refresh_list(if result.is_ok() { select } else { selected_index() });
}

fn remove_cable() {
    let cables = view().cables;
    let Some(index) = selected_index().filter(|&i| i < cables.len()) else {
        show_error("Select the cable to remove.");
        return;
    };
    let question = format!(
        "Remove \"{}\"?\n\nIt will disappear from Windows Sound, and any app using it will lose that audio device.",
        cables[index].name
    );
    if !confirm("Remove cable", &question) {
        return;
    }
    if let Err(err) = change(Request::Remove(&cables[index].serial)) {
        show_error(&err);
    }
    // Select the cable that moved into its place, or the new last one.
    let select = view().cables.len().checked_sub(1).map(|last| index.min(last));
    refresh_list(select);
}

/// Brings the server and Windows Sound back in line with the cables, for
/// recovering from a failure: starts the USB/IP server if it is not running,
/// then detaches and attaches every cable again.
fn sync_now() {
    if service_view().busy.is_some() {
        return;
    }
    if let Err(err) = change(Request::Sync) {
        show_error(&err);
    }
    refresh();
}

/// Whether the client point is on the status line's Retry link.
fn on_retry_link(x: i32, y: i32) -> bool {
    lock(&RETRY_LINK).is_some_and(|r| (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y))
}

/// Runs the cables in this process, as when no service runs them.
fn run_on_demand(cables: Vec<Cable>) {
    control::start(cables);
    refresh_list(None);
    let error = control::snapshot().server_error;
    if !error.is_empty() {
        // The window stays open, so the user can install the driver, inspect
        // the status and open the diagnostic log.
        show_error(&format!(
            "The Virtual Cables window is now staying open, but the USB/IP server could not start:\n\n{error}\n\nDiagnostic log:\n{}",
            control::log_path()
        ));
    }
}

/// Keeps the window up to date with the service: its state, and, while it has
/// the cables, their state. Runs as long as the window is open.
fn monitor_service() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let state = service_state();
        let reply = (mode() == Mode::Service).then(|| remote::call(&Request::Status));
        let before = service_view();
        match reply {
            Some(Ok(reply)) => record_reply(&reply),
            Some(Err(_)) => lock(&SERVICE).connected = false,
            None => {}
        }
        lock(&SERVICE).state = state;
        if service_view() != before {
            post_refresh();
        }
    }
}

/// Runs `job` on its own thread, showing `busy` meanwhile, and reports its
/// failure.
fn run_switch(busy: &'static str, job: fn() -> Result<(), String>) {
    set_busy(Some(busy));
    std::thread::spawn(move || {
        let result = job();
        lock(&SERVICE).state = service_state();
        set_busy(None);
        if let Err(err) = result {
            logf!(control::logger(), "{}", single_line(&err));
            show_error(&err);
        }
    });
}

/// Asks the service for its state, trying for up to `seconds` seconds.
fn call_service_within(seconds: u64) -> Result<Reply, String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match remote::call(&Request::Status) {
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            result => return result,
        }
    }
}

fn start_service_from_gui() {
    if mode() == Mode::OnDemand {
        let question = "Start the Virtual Cables Windows service?\n\n\
            It takes over from this window with its own cables and settings, from when it was installed.";
        if !confirm("Start Windows service", question) {
            return;
        }
    }
    run_switch("Starting the Windows service\u{2026}", || switch_to_service(mode() == Mode::OnDemand));
}

/// Asks, at startup, whether to start the installed service or run the cables
/// in this window.
fn ask_to_start_service() -> bool {
    let question = "The Virtual Cables Windows service is installed but not running.\n\n\
        Yes: start the service. It runs the cables with its own settings, and keeps them in Windows Sound \
        after this window closes.\n\n\
        No: run the cables in this window until it is closed.";
    // SAFETY: a message box owned by the main window, with NUL-terminated strings.
    unsafe {
        MessageBoxW(
            main_window(),
            wide(question).as_ptr(),
            wide("Start Windows service").as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        ) == IDYES
    }
}

fn install_service_from_gui() {
    let question = "Install Virtual Cables as a Windows service?\n\n\
        The service starts with Windows and keeps the cables in Windows Sound after this window closes, \
        with no administrator prompt each time. It gets a copy of your cables and settings, and this \
        window then manages it.";
    if !confirm("Install Windows service", question) {
        return;
    }
    run_switch("Installing the Windows service\u{2026}", || {
        let parameters = format!("--install-service \"{}\"", config_file_path().display());
        run_elevated(&parameters, main_window())
            .map_err(|err| format!("Could not install the Windows service:\n{err}"))?;
        switch_to_service(mode() == Mode::OnDemand)
    });
}

/// Hands the cables over to the service: stops this window's server if
/// `running_here`, which takes its cables out of Windows, and starts the
/// service, which attaches its own. Runs the cables here if the service does
/// not start: the ones it ran, or at startup the config's.
fn switch_to_service(running_here: bool) -> Result<(), String> {
    set_busy(Some("Starting the Windows service\u{2026}"));
    let on_demand = mode() == Mode::OnDemand;
    if running_here {
        control::stop();
    }
    // The service opens its pipe before it reports that it runs.
    let started = start_service()
        .and_then(|()| call_service_within(10).map_err(|err| format!("the service does not answer: {err}")));
    match started {
        Ok(reply) => {
            record_reply(&reply);
            lock(&SERVICE).mode = Mode::Service;
            logf!(control::logger(), "Managing the Windows service, version {}", reply.version);
            Ok(())
        }
        Err(err) => {
            if on_demand {
                // SAFETY: posting to a window is safe from any thread.
                unsafe { PostMessageW(main_window(), WM_APP_RUN_ON_DEMAND, usize::from(!running_here), 0) };
            }
            Err(format!("Could not start the Windows service:\n{err}"))
        }
    }
}

fn uninstall_service_from_gui() {
    let question = if mode() == Mode::Service {
        "Uninstall the Virtual Cables Windows service?\n\n\
         Its cables leave Windows Sound. This window then runs them itself, with the service's cables and \
         settings, until it is closed, and asks for administrator approval to add them to Windows Sound again."
    } else {
        "Uninstall the Virtual Cables Windows service?"
    };
    if !confirm("Uninstall Windows service", question) {
        return;
    }
    run_switch("Uninstalling the Windows service\u{2026}", || {
        run_elevated("--uninstall-service", main_window())
            .map_err(|err| format!("Could not uninstall the Windows service:\n{err}"))?;
        if mode() == Mode::Service {
            take_back_service_config();
            lock(&SERVICE).mode = Mode::OnDemand;
            // SAFETY: posting to a window is safe from any thread.
            unsafe { PostMessageW(main_window(), WM_APP_RUN_ON_DEMAND, 1, 0) };
        }
        Ok(())
    });
}

/// Copies the service's settings and cables over this user's, so the window
/// carries on with what the service had.
fn take_back_service_config() {
    let logger = control::logger();
    let source = service_data_dir().join(CONFIG_FILE);
    let copied = std::fs::read(&source).and_then(|content| {
        std::fs::create_dir_all(application_data_dir())?;
        std::fs::write(config_file_path(), content)
    });
    match copied {
        Ok(()) => logf!(logger, "Copied the service's {} to {}", source.display(), config_file_path().display()),
        Err(err) => logf!(logger, "Keeping this user's settings: could not copy {}: {err}", source.display()),
    }
}

fn confirm(title: &str, question: &str) -> bool {
    // SAFETY: a message box owned by the main window, with NUL-terminated strings.
    unsafe {
        MessageBoxW(main_window(), wide(question).as_ptr(), wide(title).as_ptr(), MB_YESNO | MB_ICONWARNING) == IDYES
    }
}

// The "Add cable" dialog: a modal window asking for the new cable's name.

const NAME_DIALOG_CLASS: &str = "VirtualCablesNameDialog";
const NAME_DIALOG_WIDTH: i32 = 380;
const NAME_DIALOG_HEIGHT: i32 = 148;
const DIALOG_BUTTON_WIDTH: i32 = 88;
const DIALOG_BUTTON_HEIGHT: i32 = 28;

struct NameDialog {
    /// The existing cables, which the name must differ from.
    cables: Vec<Cable>,
    /// The name box and hint, as handles stored like the main window's.
    edit: isize,
    hint: isize,
    done: bool,
    /// The cable to add, once a valid name has been entered.
    cable: Option<Cable>,
}

/// The open Add cable dialog; only the GUI thread uses it.
static NAME_DIALOG: Mutex<Option<NameDialog>> = Mutex::new(None);

fn name_dialog() -> std::sync::MutexGuard<'static, Option<NameDialog>> {
    NAME_DIALOG.lock().unwrap_or_else(|p| p.into_inner())
}

/// Runs `f` on the open dialog's state, without holding the lock afterwards.
fn with_name_dialog<T>(f: impl FnOnce(&mut NameDialog) -> T) -> Option<T> {
    name_dialog().as_mut().map(f)
}

/// Asks for the new cable's name and returns the cable to add; None if
/// cancelled.
fn prompt_new_cable(cables: &[Cable]) -> Option<Cable> {
    static REGISTER: std::sync::Once = std::sync::Once::new();
    let owner = main_window();
    let class_name = wide(NAME_DIALOG_CLASS);
    // SAFETY: window creation and a modal message loop on the GUI thread,
    // with NUL-terminated strings that outlive each call.
    unsafe {
        let instance = GetModuleHandleW(null());
        REGISTER.call_once(|| {
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(name_dialog_proc),
                hInstance: instance,
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as isize as HBRUSH,
                lpszClassName: class_name.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassExW(&class);
        });

        // Size the frame around the wanted client area, centred on the owner.
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let mut frame = RECT { left: 0, top: 0, right: scale(NAME_DIALOG_WIDTH), bottom: scale(NAME_DIALOG_HEIGHT) };
        AdjustWindowRectEx(&mut frame, style, 0, WS_EX_DLGMODALFRAME);
        let (w, h) = (frame.right - frame.left, frame.bottom - frame.top);
        let mut owner_rect: RECT = std::mem::zeroed();
        GetWindowRect(owner, &mut owner_rect);
        let x = owner_rect.left + (owner_rect.right - owner_rect.left - w) / 2;
        let y = owner_rect.top + (owner_rect.bottom - owner_rect.top - h) / 3;

        *name_dialog() = Some(NameDialog { cables: cables.to_vec(), edit: 0, hint: 0, done: false, cable: None });
        let dialog = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class_name.as_ptr(),
            wide("Add cable").as_ptr(),
            style,
            x,
            y,
            w,
            h,
            owner,
            null_mut(),
            instance,
            null(),
        );
        if dialog.is_null() {
            name_dialog().take();
            show_error("Could not open the Add cable dialog.");
            return None;
        }
        let (edit, hint) = create_name_dialog_controls(dialog);
        with_name_dialog(|d| (d.edit, d.hint) = (edit as isize, hint as isize));

        EnableWindow(owner, 0);
        ShowWindow(dialog, SW_SHOW);
        SetFocus(edit);

        let mut message: MSG = std::mem::zeroed();
        while with_name_dialog(|d| !d.done).unwrap_or(false) {
            match GetMessageW(&mut message, null_mut(), 0, 0) {
                0 => {
                    // Leave WM_QUIT for the main message loop.
                    PostQuitMessage(message.wParam as i32);
                    break;
                }
                -1 => break,
                _ => {}
            }
            // Gives Tab, Enter (OK) and Escape (Cancel) their dialog meaning.
            if IsDialogMessageW(dialog, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        // Re-enable the owner first so that it, not another app, is activated.
        EnableWindow(owner, 1);
        DestroyWindow(dialog);
        let closed = name_dialog().take();
        closed.and_then(|d| d.cable)
    }
}

/// The label, name box, hint and OK/Cancel buttons; returns the name box and
/// the hint.
unsafe fn create_name_dialog_controls(dialog: HWND) -> (HWND, HWND) {
    let inner = scale(NAME_DIALOG_WIDTH) - 2 * scale(MARGIN);
    // SAFETY: child windows of the dialog on the GUI thread.
    unsafe {
        let label = create_control(0, "STATIC", "Name of the new cable:", WS_CHILD | WS_VISIBLE, dialog, 0);
        MoveWindow(label, scale(MARGIN), scale(MARGIN), inner, scale(20), 0);

        let edit_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32;
        let edit = create_control(WS_EX_CLIENTEDGE, "EDIT", "", edit_style, dialog, 100);
        MoveWindow(edit, scale(MARGIN), scale(40), inner, scale(26), 0);
        SendMessageW(edit, EM_SETLIMITTEXT, MAX_CABLE_NAME, 0);

        let hint_text = "Windows Sound shows this as the cable's device name.";
        let hint = create_control(0, "STATIC", hint_text, WS_CHILD | WS_VISIBLE, dialog, 0);
        MoveWindow(hint, scale(MARGIN), scale(72), inner, scale(18), 0);

        let button = |text: &str, id: i32, style: u32, x: i32| {
            let control = create_control(0, "BUTTON", text, WS_CHILD | WS_VISIBLE | WS_TABSTOP | style, dialog, id);
            let y = scale(NAME_DIALOG_HEIGHT) - scale(MARGIN) - scale(DIALOG_BUTTON_HEIGHT);
            MoveWindow(control, x, y, scale(DIALOG_BUTTON_WIDTH), scale(DIALOG_BUTTON_HEIGHT), 0);
        };
        let cancel_x = scale(NAME_DIALOG_WIDTH) - scale(MARGIN) - scale(DIALOG_BUTTON_WIDTH);
        let ok_x = cancel_x - scale(GAP) - scale(DIALOG_BUTTON_WIDTH);
        button("Add", IDOK, BS_DEFPUSHBUTTON as u32, ok_x);
        button("Cancel", IDCANCEL, BS_PUSHBUTTON_STYLE, cancel_x);
        (edit, hint)
    }
}

unsafe extern "system" fn name_dialog_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: called by Windows on the GUI thread with valid arguments.
    unsafe {
        match message {
            WM_COMMAND => match loword(wparam) as i32 {
                IDOK => {
                    accept_cable_name(window);
                    return 0;
                }
                IDCANCEL => {
                    with_name_dialog(|d| d.done = true);
                    return 0;
                }
                _ => {}
            },
            WM_CLOSE => {
                with_name_dialog(|d| d.done = true);
                return 0;
            }
            WM_CTLCOLORSTATIC => {
                let dc = wparam as HDC;
                let is_hint = with_name_dialog(|d| d.hint == lparam).unwrap_or(false);
                let text = if is_hint { COLOR_GRAYTEXT } else { COLOR_WINDOWTEXT };
                SetTextColor(dc, GetSysColor(text));
                SetBkColor(dc, GetSysColor(COLOR_WINDOW));
                return GetSysColorBrush(COLOR_WINDOW) as LRESULT;
            }
            WM_CTLCOLORBTN => return GetSysColorBrush(COLOR_WINDOW) as LRESULT,
            _ => {}
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

/// Closes the dialog with the cable for the typed name, or explains why the
/// name cannot be used.
unsafe fn accept_cable_name(dialog: HWND) {
    let Some((edit, cables)) = with_name_dialog(|d| (d.edit as HWND, d.cables.clone())) else {
        return;
    };
    // SAFETY: reads the edit control's text into a buffer of the reported
    // length plus the terminator.
    let typed = unsafe {
        let mut buffer = vec![0u16; GetWindowTextLengthW(edit) as usize + 1];
        let length = GetWindowTextW(edit, buffer.as_mut_ptr(), buffer.len() as i32);
        String::from_utf16_lossy(&buffer[..length.max(0) as usize])
    };
    match new_cable(&typed, &cables) {
        Ok(cable) => {
            with_name_dialog(|d| {
                d.cable = Some(cable);
                d.done = true;
            });
        }
        // SAFETY: a message box owned by the dialog, then refocusing its edit.
        Err(problem) => unsafe {
            MessageBoxW(dialog, wide(&problem).as_ptr(), wide("Add cable").as_ptr(), MB_ICONWARNING);
            SetFocus(edit);
            SendMessageW(edit, EM_SETSEL, 0, -1);
        },
    }
}

fn enable_driver_menu(enabled: bool) {
    let state = if enabled { MF_ENABLED } else { MF_GRAYED };
    // SAFETY: EnableMenuItem on the Options menu; menus are not tied to a
    // thread.
    unsafe { EnableMenuItem(OPTIONS_MENU.load(Ordering::SeqCst) as HMENU, ID_DRIVER as u32, MF_BYCOMMAND | state) };
}

fn download_and_install_driver() {
    enable_driver_menu(false);
    set_notice(Tone::Pending, "Driver: checking the official usbip-win2 release\u{2026}");

    std::thread::spawn(|| {
        let logger = control::logger();
        logf!(logger, "Driver installation requested from the GUI");
        let download = download_latest_usbip_installer(&driver_downloads_dir(), &driver_progress, &open_url, &|line| {
            logf!(logger, "{line}")
        });
        let (asset_path, asset_name) = match download {
            Ok(downloaded) => downloaded,
            Err(err) => {
                enable_driver_menu(true);
                open_url(USBIP_RELEASE_PAGE);
                set_notice(
                    Tone::Error,
                    "Driver: automatic download failed \u{00b7} the official release page was opened",
                );
                let mut message = "The automatic usbip-win2 download could not reach GitHub.\n\n\
                    The official release page has been opened in your browser. Download and run the Windows x64 installer there."
                    .to_string();
                if is_name_resolution_error(&err) {
                    message += "\n\nWindows reported a DNS name-resolution error. Check the Internet connection, DNS settings, firewall, VPN, or proxy on this PC.";
                }
                message += &format!("\n\nTechnical details:\n{err}");
                show_error(&message);
                return;
            }
        };

        set_notice(Tone::Pending, "Driver: verifying the installer signature\u{2026}");
        if let Err(signature_err) = verify_authenticode_signature(&asset_path) {
            logf!(logger, "Installer Authenticode warning; continuing with the official GitHub asset: {signature_err}");
            if let Err(folder_err) = reveal_downloaded_file(&asset_path) {
                logf!(logger, "Could not reveal downloaded installer before launch: {folder_err}");
            }
            set_notice(
                Tone::Pending,
                "Driver: publisher signature not verified \u{00b7} starting the installer from the download folder\u{2026}",
            );
        } else {
            set_notice(Tone::Pending, "Driver: signature valid \u{00b7} starting the installer\u{2026}");
        }

        if let Err(err) = start_installer_elevated(&asset_path) {
            enable_driver_menu(true);
            let folder_result = reveal_downloaded_file(&asset_path);
            set_notice(
                Tone::Error,
                "Driver: the installer could not be started \u{00b7} its download folder was opened",
            );
            let mut message = format!(
                "The installer was downloaded but Windows could not start it. The download folder has been opened and the installer selected.\n\nDownloaded file:\n{}\n\nStart error:\n{err}",
                asset_path.display()
            );
            if let Err(folder_err) = folder_result {
                message += &format!("\n\nThe folder could not be opened automatically:\n{folder_err}");
            }
            show_error(&message);
            return;
        }

        enable_driver_menu(true);
        set_notice(
            Tone::Ok,
            &format!("Driver: {asset_name} started \u{00b7} accept the administrator prompt and reboot if requested"),
        );
    });
}

fn verify_authenticode_signature(path: &Path) -> Result<(), String> {
    let system_root = std::env::var("SystemRoot").unwrap_or_default();
    let program_files = std::env::var("ProgramFiles").unwrap_or_default();
    let candidates = [
        PathBuf::from(system_root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"),
        PathBuf::from(program_files).join("PowerShell").join("7").join("pwsh.exe"),
    ];
    let Some(powershell) = candidates.into_iter().find(|candidate| candidate.is_file()) else {
        return Err("Windows PowerShell was not found, so the Authenticode signature cannot be checked".into());
    };

    let escaped = path.display().to_string().replace('\'', "''");
    let script = format!(
        "$s=Get-AuthenticodeSignature -LiteralPath '{escaped}'; Write-Output ('Status=' + $s.Status); Write-Output ('Signer=' + $s.SignerCertificate.Subject); if ($s.Status -ne 'Valid') {{ exit 23 }}"
    );
    let mut command = Command::new(powershell);
    command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script]);
    hide_command_window(&mut command);
    let (text, result) = combined_output(&mut command);
    logf!(control::logger(), "Installer Authenticode check: {}", single_line(&text));
    match result {
        Ok(()) => Ok(()),
        Err(err) => Err(format!("Authenticode status is not Valid: {}", if text.is_empty() { err } else { text })),
    }
}

/// ShellExecuteW with this window as the owner; returns its result code.
fn shell_execute(verb: &str, file: &str, parameters: Option<&str>, directory: Option<&str>, show: i32) -> isize {
    let (verb, file) = (wide(verb), wide(file));
    let parameters = parameters.map(wide);
    let directory = directory.map(wide);
    // SAFETY: all strings are NUL-terminated UTF-16 that outlive the call.
    unsafe {
        ShellExecuteW(
            main_window(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ref().map_or(null(), |p| p.as_ptr()),
            directory.as_ref().map_or(null(), |d| d.as_ptr()),
            show,
        ) as isize
    }
}

fn start_installer_elevated(path: &Path) -> Result<(), String> {
    let path = std::path::absolute(path).map_err(|err| format!("resolve installer path: {err}"))?;
    let metadata = std::fs::metadata(&path).map_err(|err| format!("installer file is unavailable: {err}"))?;
    if metadata.is_dir() {
        return Err("installer path is a directory".into());
    }
    let path_text = path.display().to_string();
    let directory = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
    let is_msi = path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("msi"));
    let (file, parameters) = if is_msi {
        ("msiexec.exe".to_string(), format!("/i \"{path_text}\""))
    } else {
        (path_text.clone(), String::new())
    };

    let result = shell_execute("runas", &file, Some(&parameters), Some(&directory), SW_SHOWNORMAL);
    let error = last_error_code();
    if result > 32 {
        return Ok(());
    }
    // If the explicit elevation verb is unavailable, ask the normal Windows
    // installer association to launch the file. An installer that requires
    // elevation will still trigger Windows UAC itself.
    let fallback = shell_execute("open", &path_text, None, Some(&directory), SW_SHOWNORMAL);
    let fallback_error = last_error_code();
    if fallback > 32 {
        return Ok(());
    }
    if fallback_error.raw_os_error().unwrap_or(0) != 0 {
        return Err(format!("elevated start returned {result}; normal start returned {fallback}: {fallback_error}"));
    }
    if error.raw_os_error().unwrap_or(0) != 0 {
        return Err(format!("elevated start returned {result}: {error}; normal start returned {fallback}"));
    }
    Err(format!("elevated start returned error code {result}; normal start returned error code {fallback}"))
}

fn last_error_code() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn reveal_downloaded_file(path: &Path) -> Result<(), String> {
    match std::fs::metadata(path) {
        Err(err) => return Err(format!("downloaded installer is unavailable: {err}")),
        Ok(metadata) if metadata.is_dir() => return Err("downloaded installer path is a directory".into()),
        Ok(_) => {}
    }
    open_folder(&driver_downloads_dir(), "Downloads")
}

fn driver_downloads_dir() -> PathBuf {
    if let Ok(downloads) = windows_downloads_dir() {
        return downloads;
    }
    if let Ok(home) = std::env::var("USERPROFILE")
        && !home.trim().is_empty()
    {
        return PathBuf::from(home).join("Downloads");
    }
    std::env::temp_dir().join("Downloads")
}

fn windows_downloads_dir() -> Result<PathBuf, String> {
    let mut raw: *mut u16 = null_mut();
    // SAFETY: SHGetKnownFolderPath allocates the path, which is freed below.
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_Downloads, 0, null_mut(), &mut raw) };
    if hr < 0 || raw.is_null() {
        return Err(format!("SHGetKnownFolderPath failed with HRESULT 0x{:08X}", hr as u32));
    }
    // SAFETY: raw points to a NUL-terminated UTF-16 string owned by COM.
    let path = unsafe {
        let mut len = 0;
        while len < 32768 && *raw.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        CoTaskMemFree(raw as *const c_void);
        text
    };
    if path.trim().is_empty() {
        return Err("Windows returned an empty Downloads path".into());
    }
    Ok(PathBuf::from(path))
}

/// Opens the folder holding config and the log: the service's while it runs
/// the cables.
fn open_settings_folder_from_gui() {
    let directory = if mode() == Mode::Service { service_data_dir() } else { application_data_dir() };
    if let Err(err) = open_folder(&directory, "settings") {
        show_error(&format!("Could not open the settings folder:\n\n{}\n\n{err}", directory.display()));
    }
}

/// Opens `directory` in Explorer, creating it first if needed. `name` labels
/// the folder in error messages.
fn open_folder(directory: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(directory).map_err(|err| format!("create {name} folder: {err}"))?;
    let result = shell_execute("open", &directory.display().to_string(), None, None, SW_SHOWNORMAL);
    let error = last_error_code();
    if result <= 32 {
        // A direct Explorer start also works on systems where the shell verb is
        // unavailable or restricted by policy.
        let explorer = PathBuf::from(std::env::var("SystemRoot").unwrap_or_default()).join("explorer.exe");
        if Command::new(explorer).arg(directory).spawn().is_ok() {
            return Ok(());
        }
        if error.raw_os_error().unwrap_or(0) != 0 {
            return Err(format!("open {name} folder returned {result}: {error}"));
        }
        return Err(format!("open {name} folder returned error code {result}"));
    }
    Ok(())
}

fn open_url(url: &str) {
    if shell_execute("open", url, None, None, SW_SHOWNORMAL) <= 32 {
        let _ = Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    }
}

/// Shows the window's own message on the status line, in place of the
/// Windows Sound status until that changes; safe from any thread.
fn set_notice(tone: Tone, text: &str) {
    *lock(&NOTICE) = Some(Notice { line: (tone, text.to_string()), covers: view().status });
    invalidate_header();
}

/// Download progress from the driver installer, for the status line.
fn driver_progress(text: &str) {
    let text = text.trim_end_matches('.');
    let mut chars = text.chars();
    let first = chars.next().map(|c| c.to_lowercase().to_string()).unwrap_or_default();
    set_notice(Tone::Pending, &format!("Driver: {first}{}\u{2026}", chars.as_str()));
}

pub fn show_fatal_dialog(message: &str) {
    // SAFETY: a message box with NUL-terminated strings.
    unsafe {
        MessageBoxW(null_mut(), wide(message).as_ptr(), wide("Virtual Cables - Startup Error").as_ptr(), MB_ICONERROR);
    }
}

pub fn show_error(s: &str) {
    // SAFETY: a message box owned by the main window, with NUL-terminated strings.
    unsafe { MessageBoxW(main_window(), wide(s).as_ptr(), wide("Virtual Cables").as_ptr(), MB_ICONERROR) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build embeds the icon group as resource 1, which the window loads
    /// before any fallback.
    #[test]
    fn embedded_icon_resource_loads() {
        // SAFETY: loads icons from this test executable's own resources.
        unsafe {
            let instance = GetModuleHandleW(null());
            assert!(!load_embedded_icon(instance, 32).is_null(), "32 px icon");
            assert!(!load_embedded_icon(instance, 16).is_null(), "16 px icon");
        }
    }
}
