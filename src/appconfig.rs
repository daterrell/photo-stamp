//! This PC's photo-stamp setup, written by the setup program (`photo-stamp --configure`): which
//! folder to watch. Kept in `app.json` in the data folder.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    /// The drop folder.
    pub root: PathBuf,
    /// Set for a second, separate install (tests): names its start-at-login entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

impl AppConfig {
    pub fn path(data: &Path) -> PathBuf {
        data.join("app.json")
    }

    pub fn load(data: &Path) -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path(data)).ok()?).ok()
    }

    pub fn save(&self, data: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(data)?;
        let path = Self::path(data);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)
    }

    /// The name of the start-at-login entry (under HKCU\…\Run) for this install.
    pub fn startup_name(instance: Option<&str>) -> String {
        match instance {
            Some(i) => format!("photo-stamp-{i}"),
            None => "photo-stamp".to_owned(),
        }
    }
}

/// Which folder to watch: `--root`, else the one set up for this PC, else this program's own folder.
pub fn choose_root(
    arg: Option<&Path>,
    config: Option<&AppConfig>,
    exe_dir: Option<&Path>,
) -> Option<PathBuf> {
    arg.map(Path::to_owned).or_else(|| config.map(|c| c.root.clone())).or_else(|| exe_dir.map(Path::to_owned))
}

/// The command that starts photo-stamp at login: quietly, without opening the dashboard.
pub fn startup_command(exe: &Path, root_arg: Option<&Path>) -> String {
    let mut command = format!("\"{}\" --background", exe.display());
    if let Some(r) = root_arg {
        command.push_str(&format!(" --root \"{}\"", r.display()));
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_and_loaded() {
        let dir = std::env::temp_dir().join(format!("ps-appconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(AppConfig::load(&dir), None);
        let c = AppConfig { root: PathBuf::from(r"C:\Users\x\Pictures\Photo Stamp"), instance: None };
        c.save(&dir).unwrap();
        assert_eq!(AppConfig::load(&dir), Some(c));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn root_comes_from_the_command_line_then_setup_then_the_program_folder() {
        let c = AppConfig { root: "/setup".into(), instance: None };
        let (arg, exe) = (Path::new("/arg"), Path::new("/exe"));
        assert_eq!(choose_root(Some(arg), Some(&c), Some(exe)).unwrap(), arg);
        assert_eq!(choose_root(None, Some(&c), Some(exe)).unwrap(), Path::new("/setup"));
        assert_eq!(choose_root(None, None, Some(exe)).unwrap(), exe);
    }

    #[test]
    fn startup_entry() {
        assert_eq!(
            startup_command(Path::new(r"C:\P\photo-stamp.exe"), None),
            r#""C:\P\photo-stamp.exe" --background"#
        );
        assert_eq!(
            startup_command(Path::new("/p"), Some(Path::new("/d"))),
            r#""/p" --background --root "/d""#
        );
        assert_eq!(AppConfig::startup_name(None), "photo-stamp");
        assert_eq!(AppConfig::startup_name(Some("e2e1")), "photo-stamp-e2e1");
    }
}
