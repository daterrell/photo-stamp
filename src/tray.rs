//! The icon by the clock (Windows): click it to open the dashboard; right-click for a menu with
//! Open dashboard, Open drop folder, Pause/Resume and Quit. It shows when photo-stamp is paused.
//!
//! It lives on its own thread with a hidden window that receives the icon's clicks. The window
//! class is named `PhotoStampTray-<key>`; tests find it by that name and send it `WM_COMMAND` with
//! the menu commands below.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HICON, IDI_APPLICATION, IMAGE_ICON,
    KillTimer, LR_DEFAULTCOLOR, LoadIconW, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SetForegroundWindow,
    SetMenuDefaultItem, SetTimer, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WM_APP, WM_CLOSE, WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WM_TIMER, WNDCLASSW, WS_POPUP,
};

use crate::api::Api;
use crate::desktop;

/// Menu commands (also accepted as `WM_COMMAND` from tests).
pub const CMD_OPEN: u16 = 1;
pub const CMD_FOLDER: u16 = 2;
pub const CMD_PAUSE: u16 = 3;
pub const CMD_QUIT: u16 = 4;

/// The icon's messages to our window.
const WM_TRAY: u32 = WM_APP + 1;
/// Enter or space on the focused icon (NIN_SELECT | NINF_KEY; not in windows-sys).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const ICON_ID: u32 = 1;
/// Refresh the icon and tooltip (paused or not) this often.
const TIMER_ID: usize = 1;
const TIMER_MS: u32 = 2000;
/// Icon resources in the exe (see `assets/photo-stamp.rc`).
const RES_ICON: usize = 1;
const RES_ICON_PAUSED: usize = 2;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The tray icon; removed when dropped.
pub struct Tray {
    hwnd: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Ctx {
    api: Arc<Api>,
    url: String,
    hwnd: HWND,
    /// Explorer broadcasts this when the taskbar is (re)created: the icon must be added again.
    taskbar_created: u32,
    icon: HICON,
    icon_paused: HICON,
    /// What the icon shows now: (paused, tooltip).
    shown: Option<(bool, String)>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

impl Tray {
    /// Show the icon. `None` (and a warning in the log) if it can't be shown.
    pub fn start(key: &str, api: Arc<Api>, url: &str) -> Option<Tray> {
        let (tx, rx) = mpsc::channel();
        let class = format!("PhotoStampTray-{key}");
        let url = url.to_owned();
        let thread =
            std::thread::Builder::new().name("tray".into()).spawn(move || run(&class, api, url, tx)).ok()?;
        match rx.recv() {
            Ok(Ok(hwnd)) => Some(Tray { hwnd, thread: Some(thread) }),
            Ok(Err(e)) => {
                log::warn!("no tray icon: {e}");
                let _ = thread.join();
                None
            }
            Err(_) => None,
        }
    }
}

fn load_icon(id: usize) -> HICON {
    unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let icon = LoadImageW(
            module,
            id as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        if icon.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { icon }
    }
}

fn run(class: &str, api: Arc<Api>, url: String, ready: mpsc::Sender<Result<usize, String>>) {
    let class_w = wide(class);
    let hwnd = unsafe {
        let module = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: module,
            lpszClassName: class_w.as_ptr(),
            // SAFETY: an all-zero WNDCLASSW is valid for the remaining fields.
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        // A hidden top-level window (a message-only one wouldn't hear TaskbarCreated).
        CreateWindowExW(
            0,
            class_w.as_ptr(),
            class_w.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            module,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        let _ = ready.send(Err(format!("can't create its window: {}", std::io::Error::last_os_error())));
        return;
    }
    let taskbar_created = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            api,
            url,
            hwnd,
            taskbar_created,
            icon: load_icon(RES_ICON),
            icon_paused: load_icon(RES_ICON_PAUSED),
            shown: None,
        })
    });
    with_ctx(|c| c.add());
    unsafe { SetTimer(hwnd, TIMER_ID, TIMER_MS, None) };
    let _ = ready.send(Ok(hwnd as usize));

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    CTX.with(|c| c.borrow_mut().take());
}

/// Run `f` on the context. `None` if there's none, or it's in use further up this thread's stack
/// (a menu's message loop can call back in).
fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> Option<R> {
    CTX.with(|c| c.try_borrow_mut().ok()?.as_mut().map(f))
}

impl Ctx {
    fn data(&self, flags: u32) -> NOTIFYICONDATAW {
        // SAFETY: an all-zero NOTIFYICONDATAW is valid; the fields used are set below.
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd;
        nid.uID = ICON_ID;
        nid.uFlags = flags;
        nid
    }

    /// Whether photo-stamp is paused, and the tooltip to show.
    fn state(&self) -> (bool, String) {
        let paused = self.api.status.is_paused();
        let folder = self.api.root.file_name().map_or_else(
            || self.api.root.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        let tip = if paused {
            format!("Photo Stamp: paused ({folder})")
        } else {
            format!("Photo Stamp: watching {folder}")
        };
        (paused, tip)
    }

    fn fill(&self, nid: &mut NOTIFYICONDATAW, paused: bool, tip: &str) {
        nid.hIcon = if paused { self.icon_paused } else { self.icon };
        let tip: Vec<u16> = tip.encode_utf16().take(nid.szTip.len() - 1).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
    }

    fn add(&mut self) {
        let (paused, tip) = self.state();
        let mut nid = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        nid.uCallbackMessage = WM_TRAY;
        self.fill(&mut nid, paused, &tip);
        unsafe {
            if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
                log::warn!("couldn't add the tray icon");
            }
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
        self.shown = Some((paused, tip));
    }

    /// Show the current state, if it changed.
    fn refresh(&mut self) {
        let (paused, tip) = self.state();
        if self.shown.as_ref() == Some(&(paused, tip.clone())) {
            return;
        }
        let mut nid = self.data(NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        self.fill(&mut nid, paused, &tip);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
        self.shown = Some((paused, tip));
    }

    fn remove(&self) {
        let nid = self.data(0);
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }
}

/// Show the menu at the pointer and carry out the choice. Runs without holding the context: the
/// menu has its own message loop, which delivers our other messages meanwhile.
fn show_menu(hwnd: HWND) {
    let Some(paused) = with_ctx(|c| c.api.status.is_paused()) else { return };
    let chosen = {
        unsafe {
            let menu = CreatePopupMenu();
            AppendMenuW(menu, MF_STRING, CMD_OPEN as usize, wide("Open dashboard").as_ptr());
            AppendMenuW(menu, MF_STRING, CMD_FOLDER as usize, wide("Open drop folder").as_ptr());
            let pause = if paused { "Resume" } else { "Pause" };
            AppendMenuW(menu, MF_STRING, CMD_PAUSE as usize, wide(pause).as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, CMD_QUIT as usize, wide("Quit Photo Stamp").as_ptr());
            SetMenuDefaultItem(menu, u32::from(CMD_OPEN), 0);
            let mut at = POINT { x: 0, y: 0 };
            GetCursorPos(&mut at);
            // Without this the menu doesn't close when clicking elsewhere.
            SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                at.x,
                at.y,
                0,
                hwnd,
                std::ptr::null(),
            );
            PostMessageW(hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            chosen
        }
    };
    if chosen > 0 {
        with_ctx(|c| c.command(chosen as u16));
    }
}

impl Ctx {
    fn command(&mut self, cmd: u16) {
        match cmd {
            CMD_OPEN => desktop::open(&self.url),
            CMD_FOLDER => desktop::open(&self.api.root.to_string_lossy()),
            CMD_PAUSE => {
                let paused = self.api.status.is_paused();
                self.api.set_paused(!paused, "the tray icon");
                self.refresh();
            }
            CMD_QUIT => self.api.quit("the tray icon"),
            _ => {}
        }
    }
}

#[derive(PartialEq)]
enum Handled {
    Yes,
    No,
    ShowMenu,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let handled = with_ctx(|c| match msg {
        WM_TRAY => {
            // With NOTIFYICON_VERSION_4 the event is in the low word of lparam.
            match (lparam & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP => {
                    desktop::open(&c.url);
                    Handled::Yes
                }
                WM_CONTEXTMENU | WM_RBUTTONUP => Handled::ShowMenu,
                _ => Handled::Yes,
            }
        }
        WM_COMMAND => {
            c.command((wparam & 0xffff) as u16);
            Handled::Yes
        }
        WM_TIMER => {
            c.refresh();
            Handled::Yes
        }
        WM_CLOSE => {
            unsafe { KillTimer(hwnd, TIMER_ID) };
            c.remove();
            Handled::Yes
        }
        m if m == c.taskbar_created && m != 0 => {
            c.add();
            Handled::Yes
        }
        _ => Handled::No,
    });
    match (msg, handled) {
        (WM_CLOSE, _) => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        (WM_DESTROY, _) => {
            unsafe { PostQuitMessage(0) };
            0
        }
        (_, Some(Handled::ShowMenu)) => {
            show_menu(hwnd);
            0
        }
        (_, Some(Handled::Yes)) => 0,
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
