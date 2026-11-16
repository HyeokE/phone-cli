//! Where phone-cli keeps its files.

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment variables were `IPH_*` before the rename to phone-cli.
const LEGACY_ENV_PREFIX: &str = "IPH_";
/// The folder name under the config and state roots before the rename.
const LEGACY_DIR: &str = "iph";

/// The pre-rename name of a `PHONE_*` variable.
fn legacy_env_name(name: &str) -> Option<String> {
    name.strip_prefix("PHONE_").map(|rest| format!("{LEGACY_ENV_PREFIX}{rest}"))
}

/// `std::env::var(name)` for a `PHONE_*` variable, falling back to its old
/// `IPH_*` name when the new one is unset.
pub fn env_var(name: &str) -> Result<String, std::env::VarError> {
    match std::env::var(name) {
        Err(std::env::VarError::NotPresent) => match legacy_env_name(name) {
            Some(old) => std::env::var(old),
            None => Err(std::env::VarError::NotPresent),
        },
        found => found,
    }
}

/// `std::env::var_os(name)` with the same `IPH_*` fallback as [`env_var`].
pub fn env_var_os(name: &str) -> Option<OsString> {
    std::env::var_os(name).or_else(|| legacy_env_name(name).and_then(std::env::var_os))
}

/// `new` when it exists or `old` does not; otherwise `old`, so that files
/// kept under the pre-rename folder are still found.
fn prefer_existing(new: PathBuf, old: Option<PathBuf>) -> PathBuf {
    match old {
        Some(old) if !new.exists() && old.exists() => old,
        _ => new,
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Sockets, logs and the operation record. `PHONE_STATE_DIR` overrides it (tests).
/// `~/Library/Application Support/phone` on macOS, `%LOCALAPPDATA%\phone` on Windows.
/// `IPH_STATE_DIR` is still read when `PHONE_STATE_DIR` is unset.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = env_var_os("PHONE_STATE_DIR") {
        return PathBuf::from(dir);
    }
    state_root().join("phone")
}

fn state_root() -> PathBuf {
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Local"));
    }
    home().join("Library/Application Support")
}

/// The pre-rename state folder (`.../iph`), unless `PHONE_STATE_DIR` (or
/// `IPH_STATE_DIR`) points somewhere explicit.
fn legacy_state_dir() -> Option<PathBuf> {
    match env_var_os("PHONE_STATE_DIR") {
        Some(_) => None,
        None => Some(state_root().join(LEGACY_DIR)),
    }
}

/// A file in the state folder to read: the one under the pre-rename folder
/// when only that one exists (a saved Wi-Fi address, a Do Not Disturb record).
pub fn state_file_to_read(name: &str) -> PathBuf {
    prefer_existing(state_dir().join(name), legacy_state_dir().map(|d| d.join(name)))
}

/// The same file under the pre-rename state folder, if there is one to look at.
pub fn legacy_state_file(name: &str) -> Option<PathBuf> {
    legacy_state_dir().map(|d| d.join(name))
}

/// `config.toml` and personal scripts. `PHONE_CONFIG_DIR` overrides it (tests).
/// `~/.config/phone` on macOS, `%APPDATA%\phone` on Windows.
/// `IPH_CONFIG_DIR` is still read when `PHONE_CONFIG_DIR` is unset.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = env_var_os("PHONE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    config_root().join("phone")
}

fn config_root() -> PathBuf {
    if cfg!(windows) {
        return std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join("AppData/Roaming"));
    }
    home().join(".config")
}

/// `~/.config/iph` (`%APPDATA%\iph`), read when the new folder has nothing,
/// unless `PHONE_CONFIG_DIR` (or `IPH_CONFIG_DIR`) points somewhere explicit.
fn legacy_config_dir() -> Option<PathBuf> {
    match env_var_os("PHONE_CONFIG_DIR") {
        Some(_) => None,
        None => Some(config_root().join(LEGACY_DIR)),
    }
}

/// Developer disk images fetched when no Xcode copy is around.
pub fn ddi_cache() -> PathBuf {
    state_dir().join("ddi")
}

pub fn socket(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.sock"))
}

pub fn daemon_pid(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.pid"))
}

pub fn daemon_log(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.log"))
}

/// The Focus `phone dnd on` replaced with Do Not Disturb on this phone, for `off`.
pub fn dnd_record(udid: &str) -> PathBuf {
    state_dir().join(format!("{udid}.dnd.json"))
}

/// `config.toml`, or the pre-rename `~/.config/iph/config.toml` when only
/// that one exists.
pub fn config_file() -> PathBuf {
    prefer_existing(config_dir().join("config.toml"), legacy_config_dir().map(|d| d.join("config.toml")))
}

/// Personal scripts, or the pre-rename `~/.config/iph/scripts` when only that
/// one exists.
pub fn personal_scripts() -> PathBuf {
    prefer_existing(config_dir().join("scripts"), legacy_config_dir().map(|d| d.join("scripts")))
}

/// Create a directory only the user can enter. On Windows the folders under
/// the user's profile are private already.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Open options for a file only the user can read (0600 on macOS).
pub fn private_file() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_variables_fall_back_to_their_iph_names() {
        assert_eq!(legacy_env_name("PHONE_UDID").as_deref(), Some("IPH_UDID"));
        assert_eq!(legacy_env_name("PHONE_STATE_DIR").as_deref(), Some("IPH_STATE_DIR"));
        assert_eq!(legacy_env_name("HOME"), None);
        // Names no other test touches, so setting them here races nothing.
        let new = "PHONE_RENAME_FALLBACK_TEST";
        let old = "IPH_RENAME_FALLBACK_TEST";
        unsafe {
            std::env::remove_var(new);
            std::env::remove_var(old);
        }
        assert!(env_var(new).is_err());
        unsafe { std::env::set_var(old, "old") };
        assert_eq!(env_var(new).as_deref(), Ok("old"));
        assert_eq!(env_var_os(new).as_deref(), Some(std::ffi::OsStr::new("old")));
        unsafe { std::env::set_var(new, "new") };
        assert_eq!(env_var(new).as_deref(), Ok("new"));
        assert_eq!(env_var_os(new).as_deref(), Some(std::ffi::OsStr::new("new")));
        unsafe {
            std::env::remove_var(new);
            std::env::remove_var(old);
        }
    }

    #[test]
    fn the_old_folder_is_read_only_when_the_new_one_has_nothing() {
        let base = std::env::temp_dir().join(format!("phone-paths-{}", uuid::Uuid::new_v4()));
        let new = base.join("phone/config.toml");
        let old = base.join("iph/config.toml");
        // Neither exists: the new path, so a first save goes there.
        assert_eq!(prefer_existing(new.clone(), Some(old.clone())), new);
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, "udid = \"A\"").unwrap();
        assert_eq!(prefer_existing(new.clone(), Some(old.clone())), old);
        std::fs::create_dir_all(new.parent().unwrap()).unwrap();
        std::fs::write(&new, "udid = \"B\"").unwrap();
        assert_eq!(prefer_existing(new.clone(), Some(old.clone())), new);
        // An explicit override has no old folder to look at.
        assert_eq!(prefer_existing(base.join("x"), None), base.join("x"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
