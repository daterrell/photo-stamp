//! Windows plumbing for running as a quiet background app: console attachment, one instance per
//! folder, a stop signal, start-at-login, and message boxes.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE,
};
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW};

use crate::outbox::Wake;
use crate::paths::folder_key;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Show output in the terminal we were started from, if any. Returns whether there is one.
pub fn attach_parent_console() -> bool {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) != 0 }
}

pub fn message_box(text: &str, error: bool) {
    let icon = if error { MB_ICONERROR } else { MB_ICONINFORMATION };
    let (text, title) = (wide(text), wide("photo-stamp"));
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | icon);
    }
}

/// Held while this process is the watcher for a folder.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// The handle is only ever closed, from whichever thread drops the guard.
unsafe impl Send for InstanceGuard {}

/// `None` if another photo-stamp is already watching `root`.
pub fn single_instance(root: &Path) -> std::io::Result<Option<InstanceGuard>> {
    let name = wide(format!("Local\\photo-stamp-{}", folder_key(root)));
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    Ok(Some(InstanceGuard(handle)))
}

fn stop_event_name(root: &Path) -> Vec<u16> {
    wide(format!("Local\\photo-stamp-stop-{}", folder_key(root)))
}

/// Wake `waker` with [`Wake::Stop`] when another process runs `photo-stamp --stop` for `root`.
pub fn on_stop_signal(root: &Path, waker: Sender<Wake>) -> std::io::Result<()> {
    let name = stop_event_name(root);
    let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    if event.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let event = event as usize; // HANDLE isn't Send; it stays valid for the life of the process.
    std::thread::spawn(move || {
        unsafe { WaitForSingleObject(event as HANDLE, INFINITE) };
        let _ = waker.send(Wake::Stop);
    });
    Ok(())
}

/// Ask the watcher for `root` to stop. Returns false if none is running.
pub fn signal_stop(root: &Path) -> std::io::Result<bool> {
    let name = stop_event_name(root);
    let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if event.is_null() {
        return match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => Ok(false),
            _ => Err(std::io::Error::last_os_error()),
        };
    }
    let ok = unsafe { SetEvent(event) } != 0;
    unsafe { CloseHandle(event) };
    if ok { Ok(true) } else { Err(std::io::Error::last_os_error()) }
}

/// The name of this folder's entry under HKCU\…\Run.
pub fn startup_entry_name(root: &Path) -> String {
    format!("photo-stamp {}", folder_key(root))
}

/// Start `command` at login for this user (no admin needed).
pub fn install_startup(root: &Path, command: &str) -> std::io::Result<()> {
    let (key, name, value) = (wide(RUN_KEY), wide(startup_entry_name(root)), wide(command));
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if err == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(err as i32)) }
}

/// Remove this folder's start-at-login entry. Returns false if there wasn't one.
pub fn uninstall_startup(root: &Path) -> std::io::Result<bool> {
    let (key, name) = (wide(RUN_KEY), wide(startup_entry_name(root)));
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
        0 => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        err => Err(std::io::Error::from_raw_os_error(err as i32)),
    }
}

/// The name Windows shows on our notifications.
const APP_ID: &str = "photo-stamp";

/// Register photo-stamp as a notification sender for this user, so toasts show its name
/// (unpackaged apps need this instead of a Start menu shortcut).
fn register_app_id() -> std::io::Result<()> {
    let key = wide(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    let (name, value) = (wide("DisplayName"), wide("photo-stamp"));
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if err == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(err as i32)) }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Show a Windows notification; clicking it opens `link` (e.g. the dashboard).
pub fn toast(title: &str, body: &str, link: &str) -> anyhow::Result<()> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
    use windows::core::HSTRING;

    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        if let Err(e) = register_app_id() {
            log::warn!("couldn't register for notifications: {e}");
        }
    });
    // WinRT needs COM on this thread; harmless if it's already set up.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let xml = format!(
        r#"<toast activationType="protocol" launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        xml_escape(link),
        xml_escape(title),
        xml_escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(())
}
