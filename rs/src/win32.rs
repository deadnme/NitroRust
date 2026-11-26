//! Win32 helpers: window subclassing (DPI size guard, full repaint, sleep/resume,
//! AC/battery status, Explorer tray recovery, live accent), clipboard, registry,
//! color dialog, monitor work area, memory trimming, and single-instance IPC.

use crate::AppWindow;
use slint::{Color, ComponentHandle};
use std::cell::Cell;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND};

pub const MUTEX: &str = "Local\\nitrorust";
pub const SHOW_EVENT: &str = "Local\\nitrorust-show";
pub const EXIT_EVENT: &str = "Local\\nitrorust-exit";

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Machine details for diagnostics, read from the registry.
#[derive(Default, Clone)]
pub struct SysInfo {
    pub maker: String,
    pub model: String,
    pub bios: String,
    pub windows: String,
}

impl SysInfo {
    pub fn read() -> SysInfo {
        let bios = "HARDWARE\\DESCRIPTION\\System\\BIOS";
        let nt = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        let get = |k: &str, v: &str| reg_string(k, v).unwrap_or_default();
        SysInfo {
            maker: get(bios, "SystemManufacturer"),
            model: get(bios, "SystemProductName"),
            bios: get(bios, "BIOSVersion"),
            windows: format!("build {} ({})", get(nt, "CurrentBuild"), get(nt, "DisplayVersion")),
        }
    }
}

/// Hands the process's pages back to Windows while it sits in the tray.
pub fn trim_memory() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

/// True when on AC power (or when Windows doesn't report battery status).
pub fn is_on_ac_power() -> bool {
    use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut st: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
    if unsafe { GetSystemPowerStatus(&mut st) } == 0 {
        return true;
    }
    // ACLineStatus: 0 = Offline (battery), 1 = Online (AC), 255 = Unknown.
    st.ACLineStatus != 0
}

/// Extracts the Win32 HWND for the Slint AppWindow, if realized.
pub fn hwnd_of(ui: &AppWindow) -> HWND {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use slint::winit_030::WinitWindowAccessor;
    ui.window()
        .with_winit_window(|ww| {
            ww.window_handle().ok().and_then(|h| match h.as_raw() {
                RawWindowHandle::Win32(w) => Some(w.hwnd.get() as HWND),
                _ => None,
            })
        })
        .flatten()
        .unwrap_or(std::ptr::null_mut())
}

thread_local! {
    static UI: Cell<Option<slint::Weak<AppWindow>>> = const { Cell::new(None) };
    static TASKBAR_CREATED_MSG: Cell<u32> = const { Cell::new(0) };
    static IN_DPI_CHANGE: Cell<bool> = const { Cell::new(false) };
    static HELD: Cell<Option<(i32, i32, u32, Instant)>> = const { Cell::new(None) };
}

fn dispatch_action(action: &'static str) {
    let _ = slint::invoke_from_event_loop(move || {
        let ui = UI.take();
        if let Some(app) = ui.as_ref().and_then(|w| w.upgrade()) {
            app.invoke_tray_action(action.into());
        }
        UI.set(ui);
    });
}

/// Hidden top-level Win32 window created at startup so system broadcasts work even in `--tray`
/// mode before the Slint AppWindow is ever shown.
/// Receives:
/// - Sleep/resume (`WM_POWERBROADCAST` -> `PBT_APMRESUMEAUTOMATIC` / `PBT_APMRESUMESUSPEND`)
/// - AC/Battery switches (`WM_POWERBROADCAST` -> `PBT_APMPOWERSTATUSCHANGE`)
/// - Explorer restart tray recovery (`TaskbarCreated`, explicitly allowed through UIPI)
/// - Live Windows accent colour updates (`WM_DWMCOLORIZATIONCOLORCHANGED` / `WM_SETTINGCHANGE`)
pub struct SystemListener(HWND);

impl SystemListener {
    pub fn start(ui: slint::Weak<AppWindow>) -> Option<Self> {
        use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, RegisterClassW,
            RegisterWindowMessageW, MSGFLT_ALLOW, PBT_APMPOWERSTATUSCHANGE,
            PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, WM_POWERBROADCAST, WM_SETTINGCHANGE,
            WNDCLASSW, WS_OVERLAPPED,
        };

        const WM_DWMCOLORIZATIONCOLORCHANGED: u32 = 0x0320;

        unsafe extern "system" fn wnd_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let taskbar_msg = TASKBAR_CREATED_MSG.get();
                if taskbar_msg != 0 && msg == taskbar_msg {
                    dispatch_action("taskbar-created");
                    return 0;
                }
                match msg {
                    WM_POWERBROADCAST => {
                        let event = w as u32;
                        if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
                            dispatch_action("power-resume");
                        } else if event == PBT_APMPOWERSTATUSCHANGE {
                            dispatch_action("power-status");
                        }
                        1
                    }
                    WM_DWMCOLORIZATIONCOLORCHANGED | WM_SETTINGCHANGE => {
                        dispatch_action("accent-changed");
                        unsafe { DefWindowProcW(h, msg, w, l) }
                    }
                    _ => unsafe { DefWindowProcW(h, msg, w, l) },
                }
            }));
            res.unwrap_or_else(|_| unsafe { DefWindowProcW(h, msg, w, l) })
        }

        UI.set(Some(ui));
        let taskbar_msg = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
        TASKBAR_CREATED_MSG.set(taskbar_msg);

        let class_name = wide("NitroRustSystemListener");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: class_name.as_ptr(),
            ..unsafe { std::mem::zeroed() }
        };
        unsafe { RegisterClassW(&wc) };

        // Must be a top-level window (hwndParent = null, NOT HWND_MESSAGE) to receive broadcasts.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                wide("NitroRust").as_ptr(),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        // NitroRust runs elevated (High integrity), while explorer.exe broadcasts TaskbarCreated
        // from Medium integrity. Allow it explicitly through UIPI.
        unsafe {
            if taskbar_msg != 0 {
                ChangeWindowMessageFilterEx(hwnd, taskbar_msg, MSGFLT_ALLOW, std::ptr::null_mut());
            }
            ChangeWindowMessageFilterEx(hwnd, WM_DWMCOLORIZATIONCOLORCHANGED, MSGFLT_ALLOW, std::ptr::null_mut());
            ChangeWindowMessageFilterEx(hwnd, WM_SETTINGCHANGE, MSGFLT_ALLOW, std::ptr::null_mut());
        }
        Some(Self(hwnd))
    }
}

impl Drop for SystemListener {
    fn drop(&mut self) {
        if !self.0.is_null() {
            use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
            unsafe { DestroyWindow(self.0) };
        }
    }
}

/// Subclasses the visible AppWindow for:
/// - DPI-change size lock (prevents Slint resizing back at the old scale during monitor drag)
/// - Full software repaint on non-empty WM_PAINT regions
/// - Clean subclass removal on WM_NCDESTROY
pub fn hook_window(ui: &AppWindow) {
    use slint::winit_030::WinitWindowAccessor;
    UI.set(Some(ui.as_weak()));
    ui.window().with_winit_window(install_subclass);
}

fn install_subclass(ww: &slint::winit_030::winit::window::Window) {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::GetUpdateRect;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, IsZoomed, SWP_NOSIZE, WINDOWPOS, WM_DPICHANGED, WM_NCDESTROY, WM_PAINT,
        WM_WINDOWPOSCHANGING,
    };

    const HOLD: Duration = Duration::from_millis(500);
    const SUBCLASS_ID: usize = 1;

    unsafe extern "system" fn proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM, _: usize, _: usize) -> LRESULT {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
            WM_DPICHANGED => {
                HELD.set(None);
                IN_DPI_CHANGE.set(true);
                let result = unsafe { DefSubclassProc(h, msg, w, l) };
                IN_DPI_CHANGE.set(false);
                result
            }
            WM_WINDOWPOSCHANGING => {
                let pos = unsafe { &mut *(l as *mut WINDOWPOS) };
                if pos.flags & SWP_NOSIZE == 0 {
                    let dpi = unsafe { GetDpiForWindow(h) };
                    if IN_DPI_CHANGE.get() {
                        HELD.set(Some((pos.cx, pos.cy, dpi, Instant::now())));
                    } else if let Some((cx, cy, ..)) = HELD.get().filter(|s| s.2 == dpi && s.3.elapsed() < HOLD) {
                        if (pos.cx, pos.cy) != (cx, cy) && unsafe { IsZoomed(h) == 0 && IsIconic(h) == 0 } {
                            pos.cx = cx;
                            pos.cy = cy;
                        }
                    }
                }
                unsafe { DefSubclassProc(h, msg, w, l) }
            }
            // Slint's own redraw requests come with an empty update region.
            WM_PAINT if unsafe { GetUpdateRect(h, std::ptr::null_mut(), 0) } != 0 => {
                let _ = slint::invoke_from_event_loop(|| {
                    let ui = UI.take();
                    if let Some(app) = ui.as_ref().and_then(|w| w.upgrade()) {
                        app.set_repaint_all(!app.get_repaint_all());
                    }
                    UI.set(ui);
                });
                unsafe { DefSubclassProc(h, msg, w, l) }
            }
            WM_NCDESTROY => {
                let result = unsafe { DefSubclassProc(h, msg, w, l) };
                unsafe { RemoveWindowSubclass(h, Some(proc), SUBCLASS_ID) };
                result
            }
            _ => unsafe { DefSubclassProc(h, msg, w, l) },
        }));
        res.unwrap_or_else(|_| unsafe { DefSubclassProc(h, msg, w, l) })
    }

    if let Ok(handle) = ww.window_handle() {
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            // Installed once, on the first showing; Slint hides the window without destroying it.
            unsafe { SetWindowSubclass(h.hwnd.get() as HWND, Some(proc), SUBCLASS_ID, 0) };
        }
    }
}

/// Left, top, right and bottom of the work area (screen without the taskbar) of the monitor at x, y, in physical pixels.
pub fn work_area(x: i32, y: i32) -> (i32, i32, i32, i32) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let ok = unsafe { GetMonitorInfoW(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST), &mut mi) };
    let r = mi.rcWork;
    if ok != 0 { (r.left, r.top, r.right, r.bottom) } else { (0, 0, 1600, 1000) }
}

/// The Windows accent colour (DWM stores it as 0xAABBGGRR).
pub fn windows_accent() -> Option<Color> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (key, value) = (wide("Software\\Microsoft\\Windows\\DWM"), wide("AccentColor"));
    let (mut data, mut size) = (0u32, 4u32);
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _,
            &mut size,
        )
    };
    (err == 0).then(|| Color::from_rgb_u8(data as u8, (data >> 8) as u8, (data >> 16) as u8))
}

/// The Windows colour dialog, properly parented to `owner` and preserving custom colour slots.
pub fn choose_color(owner: HWND, initial: [u8; 3], custom: &mut [u32; 16]) -> Option<[u8; 3]> {
    use windows_sys::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let mut cc: CHOOSECOLORW = unsafe { std::mem::zeroed() };
    cc.lStructSize = std::mem::size_of::<CHOOSECOLORW>() as u32;
    cc.hwndOwner = if owner.is_null() { unsafe { GetForegroundWindow() } } else { owner };
    cc.rgbResult = initial[0] as u32 | (initial[1] as u32) << 8 | (initial[2] as u32) << 16;
    cc.lpCustColors = custom.as_mut_ptr();
    cc.Flags = CC_FULLOPEN | CC_RGBINIT;
    if unsafe { ChooseColorW(&mut cc) } == 0 {
        return None;
    }
    let v = cc.rgbResult;
    Some([v as u8, (v >> 8) as u8, (v >> 16) as u8])
}

/// A REG_SZ value under HKEY_LOCAL_MACHINE.
pub fn reg_string(key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let (k, v) = (wide(key), wide(value));
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as *mut _,
            &mut size,
        )
    };
    if err != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim().to_string())
}

/// Puts text on the Windows clipboard.
pub fn set_clipboard(text: &str) -> bool {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let data = wide(&text.replace('\n', "\r\n"));
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
                GlobalUnlock(mem);
                // On success the clipboard owns the memory.
                ok = !SetClipboardData(CF_UNICODETEXT, mem as _).is_null();
            }
            if !ok {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
        ok
    }
}

/// Opens a web address in the default browser.
pub fn open_url(url: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, target) = (wide("open"), wide(url));
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Holds the single-instance mutex handle for the lifetime of the process.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    /// Returns `Some` if this process is the sole instance; `None` if another copy already holds it.
    pub fn acquire() -> Option<Self> {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name = wide(MUTEX);
        unsafe {
            let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            let err = GetLastError();
            if h.is_null() {
                // ERROR_ACCESS_DENIED means another instance owns it; treat as already running.
                return if err == ERROR_ACCESS_DENIED { None } else { Some(Self(std::ptr::null_mut())) };
            }
            if err == ERROR_ALREADY_EXISTS {
                CloseHandle(h);
                return None;
            }
            Some(Self(h))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// Asks the running copy to show its window (SHOW_EVENT) or to exit (EXIT_EVENT).
/// Returns true if the event existed and was signalled.
pub fn signal_running_copy(event: &str) -> bool {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    let name = wide(event);
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if !h.is_null() {
            SetEvent(h);
            CloseHandle(h);
            return true;
        }
    }
    false
}

/// Signals SHOW_EVENT with a short retry window in case the first instance is still initializing.
pub fn signal_show_with_retry() {
    for _ in 0..20 {
        if signal_running_copy(SHOW_EVENT) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// True while a copy holds the single-instance mutex.
pub fn copy_running() -> bool {
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = wide(MUTEX);
    unsafe {
        let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// Waits for another copy's signal on `event` and runs the tray action `action` ("open" or "exit").
pub fn listen_for_second_copy(ui: slint::Weak<AppWindow>, event: &str, action: &'static str) {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    let name = wide(event);
    let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if h.is_null() {
        return;
    }
    let h = h as usize; // HANDLE is a raw pointer; the thread only passes it back to Windows.
    std::thread::spawn(move || loop {
        if unsafe { WaitForSingleObject(h as _, INFINITE) } != 0 {
            unsafe { CloseHandle(h as _) };
            return;
        }
        let _ = ui.upgrade_in_event_loop(move |ui| ui.invoke_tray_action(action.into()));
    });
}
