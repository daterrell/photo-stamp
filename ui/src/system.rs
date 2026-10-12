//! Platform bits: messages, a small log file, opening the browser, starting programs detached,
//! and reading the start-at-login entry.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn init_log(data: Option<&Path>) {
    if let Some(d) = data {
        let _ = std::fs::create_dir_all(d);
        *LOG.lock().unwrap() = Some(d.join("photo-stamp-ui.log"));
    }
}

/// Append a line to `photo-stamp-ui.log` (and stderr, where there is one).
pub fn log(msg: &str) {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let line = format!("{secs} {msg}\n");
    eprint!("{line}");
    if let Some(path) = LOG.lock().unwrap().as_ref() {
        if std::fs::metadata(path).is_ok_and(|m| m.len() > 1024 * 1024) {
            let _ = std::fs::remove_file(path);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Tell the user something: a message box on Windows (there's no console), stdout elsewhere.
pub fn tell(msg: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
        let text: Vec<u16> = msg.encode_utf16().chain([0]).collect();
        let title: Vec<u16> = "photo-stamp dashboard".encode_utf16().chain([0]).collect();
        unsafe {
            MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_OK | MB_ICONINFORMATION)
        };
    }
    #[cfg(not(windows))]
    println!("{msg}");
}

pub fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let verb: Vec<u16> = "open".encode_utf16().chain([0]).collect();
        let file: Vec<u16> = url.encode_utf16().chain([0]).collect();
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
    #[cfg(not(windows))]
    if let Err(e) = Command::new("xdg-open").arg(url).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        log(&format!("open {url} in your browser ({e})"));
    }
}

/// Start `exe` in the background, independent of this program (it keeps running if we exit).
pub fn spawn_detached(exe: &Path, args: &[&std::ffi::OsStr]) -> std::io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map(drop)
}

/// Run `exe` with `args` and wait, returning what it printed.
pub fn run(exe: &Path, args: &[&std::ffi::OsStr]) -> std::io::Result<String> {
    let mut cmd = Command::new(exe);
    cmd.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if out.status.success() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(std::io::Error::other(if err.is_empty() { text } else { err }))
    }
}

/// Whether photo-stamp starts at login for the folder with this key (its HKCU Run entry).
#[cfg(windows)]
pub fn starts_at_login(key: &str) -> Option<bool> {
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegGetValueW};
    let subkey: Vec<u16> =
        r"Software\Microsoft\Windows\CurrentVersion\Run".encode_utf16().chain([0]).collect();
    let value: Vec<u16> = format!("photo-stamp {key}").encode_utf16().chain([0]).collect();
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    Some(err == 0)
}

/// Start-at-login is a Windows feature.
#[cfg(not(windows))]
pub fn starts_at_login(_key: &str) -> Option<bool> {
    None
}
