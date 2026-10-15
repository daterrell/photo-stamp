//! Windows plumbing for running as a quiet background app: console attachment, one running copy
//! per user (per data folder), a stop signal, start-at-login, opening things, and message boxes.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE,
};
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenMutexW, SetEvent,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW};

use crate::outbox::Wake;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// Don't let Windows show error dialogs for this process (e.g. "not a valid Win32 application"
/// when starting a damaged program, which would otherwise wait for someone to click OK): the
/// call fails instead, and we deal with it.
pub fn no_error_dialogs() {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
    };
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX) };
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

/// Held while this process is the running photo-stamp.
pub struct InstanceGuard(HANDLE);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

// The handle is only ever closed, from whichever thread drops the guard.
unsafe impl Send for InstanceGuard {}

fn mutex_name(key: &str) -> Vec<u16> {
    wide(format!("Local\\photo-stamp-{key}"))
}

/// `None` if another photo-stamp with this `key` (see `instance_key` in main) is already running.
pub fn single_instance(key: &str) -> std::io::Result<Option<InstanceGuard>> {
    let name = mutex_name(key);
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

fn stop_event_name(key: &str) -> Vec<u16> {
    wide(format!("Local\\photo-stamp-stop-{key}"))
}

/// Wake `waker` with [`Wake::Stop`] when another process runs `photo-stamp --quit`.
pub fn on_stop_signal(key: &str, waker: Sender<Wake>) -> std::io::Result<()> {
    let name = stop_event_name(key);
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

/// Ask the running photo-stamp to stop. Returns false if none is running.
pub fn signal_stop(key: &str) -> std::io::Result<bool> {
    let name = stop_event_name(key);
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

/// Whether the running photo-stamp with this `key` is gone (its single-instance lock is free).
pub fn wait_until_gone(key: &str, timeout: Duration) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let name = mutex_name(key);
    let deadline = Instant::now() + timeout;
    loop {
        let handle = unsafe { OpenMutexW(SYNCHRONIZE, 0, name.as_ptr()) };
        if handle.is_null() {
            return true;
        }
        unsafe { CloseHandle(handle) };
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Start `command` at login for this user (no admin needed), as the Run entry `name`.
pub fn install_startup(name: &str, command: &str) -> std::io::Result<()> {
    let (key, name, value) = (wide(RUN_KEY), wide(name), wide(command));
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

/// Remove the start-at-login entry `name`. Returns false if there wasn't one.
pub fn uninstall_startup(name: &str) -> std::io::Result<bool> {
    let (key, name) = (wide(RUN_KEY), wide(name));
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
        0 => Ok(true),
        ERROR_FILE_NOT_FOUND => Ok(false),
        err => Err(std::io::Error::from_raw_os_error(err as i32)),
    }
}

/// Whether the start-at-login entry `name` exists.
pub fn starts_at_login(name: &str) -> bool {
    let (key, name) = (wide(RUN_KEY), wide(name));
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    err == 0
}

/// A text value from the registry under HKEY_CURRENT_USER.
fn read_string(subkey: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(subkey), wide(value));
    let mut len: u32 = 0;
    let query = |buf: *mut std::ffi::c_void, len: &mut u32| unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf,
            len,
        )
    };
    if query(std::ptr::null_mut(), &mut len) != 0 {
        return None;
    }
    let mut buf = vec![0u16; (len as usize).div_ceil(2)];
    if query(buf.as_mut_ptr().cast(), &mut len) != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Keep the version shown in Settings > Apps current after an update: if this program is the one
/// the setup program installed (its uninstall entry points at `program_dir`), set the entry's
/// DisplayVersion to `version`.
pub fn sync_installed_version(instance: Option<&str>, program_dir: &std::path::Path, version: &str) {
    let name = match instance {
        Some(i) => format!("PhotoStamp-{i}"),
        None => "PhotoStamp".to_owned(),
    };
    let subkey = format!(r"Software\Microsoft\Windows\CurrentVersion\Uninstall\{name}");
    let Some(location) = read_string(&subkey, "InstallLocation") else { return };
    let norm = |p: &str| p.trim_end_matches(['\\', '/']).to_lowercase();
    if norm(&location) != norm(&program_dir.to_string_lossy()) {
        return;
    }
    if read_string(&subkey, "DisplayVersion").as_deref() == Some(version) {
        return;
    }
    let (key, value_name, value) = (wide(&subkey), wide("DisplayVersion"), wide(version));
    let err = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value_name.as_ptr(),
            REG_SZ,
            value.as_ptr().cast(),
            (value.len() * 2) as u32,
        )
    };
    if err == 0 {
        log::info!("Settings > Apps now shows version {version}");
    }
}

/// Open a web address or folder as double-clicking it would.
pub fn shell_open(target: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, file) = (wide("open"), wide(target));
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
}

/// The name Windows shows on our notifications.
const APP_ID: &str = "photo-stamp";

/// Register photo-stamp as a notification sender for this user, so toasts show its name
/// (unpackaged apps need this instead of a Start menu shortcut).
pub fn register_app_id() -> std::io::Result<()> {
    let key = wide(format!(r"Software\Classes\AppUserModelId\{APP_ID}"));
    let (name, value) = (wide("DisplayName"), wide("Photo Stamp"));
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
