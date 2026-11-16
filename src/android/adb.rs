//! Running `adb`: finding the binary, listing devices, and one command at a
//! time against one serial. Nothing is installed on the phone; everything
//! goes through the shell tools Android ships (`uiautomator`, `input`, `am`,
//! `pm`, `screencap`, `dumpsys`, `wm`).

use std::{path::PathBuf, process::Stdio, time::Duration};

use serde::Serialize;

use crate::{i18n::t, reply::Fail};

/// Where adb is: `ADB`, then `adb` on PATH, then the Android SDK's usual
/// places (`ANDROID_HOME`, `ANDROID_SDK_ROOT`, `~/Library/Android/sdk`).
pub fn adb_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ADB").filter(|p| !p.is_empty()) {
        // An `ADB` that names nothing means "no adb", not "look elsewhere".
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let exe = if cfg!(windows) { "adb.exe" } else { "adb" };
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let mut roots: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from))
        .collect();
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let home = PathBuf::from(home);
        roots.push(home.join("Library/Android/sdk"));
        roots.push(home.join("Android/Sdk"));
        roots.push(home.join("AppData/Local/Android/Sdk"));
    }
    roots
        .into_iter()
        .map(|r| r.join("platform-tools").join(exe))
        .find(|c| c.is_file())
}

/// Why an adb command did not give its output.
#[derive(Debug, Clone, PartialEq)]
pub enum AdbError {
    /// No adb binary.
    Missing,
    /// adb could not reach the device (gone, offline, unauthorized): the
    /// command did not run on the phone.
    Unreachable(String),
    /// The command ran (or may have) and failed, or adb broke off midway.
    Failed { code: Option<i32>, message: String },
    /// No answer in time; the command may have run.
    Timeout(u64),
}

impl AdbError {
    pub fn message(&self) -> String {
        match self {
            AdbError::Missing => "adb was not found (set ADB, put adb on PATH, or install the Android SDK platform-tools)".into(),
            AdbError::Unreachable(m) => m.clone(),
            AdbError::Failed { code, message } => match code {
                Some(c) => format!("exit {c}: {message}"),
                None => message.clone(),
            },
            AdbError::Timeout(s) => format!("no answer from adb within {s} s"),
        }
    }

    /// For a command that sends nothing to the screen (reading, listing):
    /// every failure is `not_sent`.
    pub fn not_sent(&self, code: &str) -> Fail {
        let fail = match self {
            AdbError::Missing => return missing_adb(),
            AdbError::Unreachable(_) => Fail::not_sent("device_unreachable", self.message()),
            _ => Fail::not_sent(code, self.message()),
        };
        fail.hint(unreachable_hint())
    }

    /// For an input command: `not_sent` only when adb never reached the
    /// device; otherwise the input may have gone out.
    pub fn input(&self) -> Fail {
        match self {
            AdbError::Missing => missing_adb(),
            AdbError::Unreachable(_) => Fail::not_sent("device_unreachable", self.message()).hint(unreachable_hint()),
            _ => Fail::unknown("input_failed", self.message()).hint(t(
                "입력이 나갔을 수 있습니다. 다시 보내지 말고 `phone read`로 화면을 확인하세요.",
                "The input may have gone out. Do not resend; check the screen with `phone read`.",
            )),
        }
    }
}

pub fn missing_adb() -> Fail {
    Fail::not_sent("adb_not_found", AdbError::Missing.message()).hint(t(
        "Android SDK platform-tools를 설치하거나 `ADB=/path/to/adb`를 지정하세요.",
        "Install the Android SDK platform-tools, or set `ADB=/path/to/adb`.",
    ))
}

fn unreachable_hint() -> &'static str {
    t(
        "`adb devices`에 폰이 `device` 상태로 보이는지 확인하세요. `unauthorized`이면 폰에서 USB 디버깅 허용을 누르세요. Wi-Fi는 `adb connect <IP>:<port>`로 다시 연결합니다.",
        "Check that `adb devices` lists the phone as `device`. If it says `unauthorized`, allow USB debugging on the phone. For Wi-Fi, reconnect with `adb connect <IP>:<port>`.",
    )
}

/// adb's own words for "the device is not there": the command never ran.
fn unreachable(stderr: &str) -> bool {
    // adb prefixes its own complaints with "error:"; the phone's shell does not.
    let s = stderr.to_lowercase();
    let Some(first) = s.lines().next().filter(|l| l.starts_with("error:") || l.starts_with("adb: error")) else {
        return false;
    };
    (first.contains("device '") && first.contains("not found"))
        || ["no devices", "unauthorized", "device offline", "more than one device", "cannot connect", "failed to connect", "no emulators"]
            .iter()
            .any(|n| first.contains(n))
}

/// Run adb with `args` and return stdout. `serial` selects the device.
pub async fn run(serial: Option<&str>, args: &[&str], secs: u64) -> Result<Vec<u8>, AdbError> {
    let path = adb_path().ok_or(AdbError::Missing)?;
    let mut cmd = tokio::process::Command::new(path);
    if let Some(s) = serial {
        cmd.arg("-s").arg(s);
    }
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| AdbError::Failed { code: None, message: format!("cannot run adb: {e}") })?;
    let out = match tokio::time::timeout(Duration::from_secs(secs), child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(AdbError::Failed { code: None, message: format!("adb: {e}") }),
        Err(_) => return Err(AdbError::Timeout(secs)),
    };
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let message = if stderr.is_empty() { stdout.trim().chars().take(400).collect() } else { stderr.clone() };
    if unreachable(&stderr) {
        return Err(AdbError::Unreachable(message));
    }
    Err(AdbError::Failed { code: out.status.code(), message })
}

/// One device as `adb devices -l` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdbDevice {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, `authorizing`, …
    pub state: String,
    /// `usb`, `wifi` or `emulator`.
    pub connection: String,
    pub model: Option<String>,
    pub product: Option<String>,
    pub device: Option<String>,
    pub transport_id: Option<String>,
}

/// Parse `adb devices -l`.
pub fn parse_devices(text: &str) -> Vec<AdbDevice> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("List of devices") || line.starts_with('*') || line.starts_with("adb ") {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else { continue };
        // "no permissions (…)" and the like take several words.
        let mut state = state.to_string();
        let mut usb = None;
        let mut model = None;
        let mut product = None;
        let mut device = None;
        let mut transport_id = None;
        for p in parts {
            match p.split_once(':') {
                Some(("usb", v)) => usb = Some(v.to_string()),
                Some(("model", v)) => model = Some(v.to_string()),
                Some(("product", v)) => product = Some(v.to_string()),
                Some(("device", v)) => device = Some(v.to_string()),
                Some(("transport_id", v)) => transport_id = Some(v.to_string()),
                _ if model.is_none() && product.is_none() && usb.is_none() => {
                    state.push(' ');
                    state.push_str(p);
                }
                _ => {}
            }
        }
        let connection = if serial.starts_with("emulator-") {
            "emulator"
        } else if usb.is_some() {
            "usb"
        } else if serial.contains(':') || serial.contains("._adb-tls-connect.") {
            "wifi"
        } else {
            "usb"
        };
        out.push(AdbDevice {
            serial: serial.to_string(),
            state,
            connection: connection.into(),
            model,
            product,
            device,
            transport_id,
        });
    }
    out
}

/// Devices adb knows. An empty list when adb is not installed.
pub async fn list_devices() -> Result<Vec<AdbDevice>, AdbError> {
    if adb_path().is_none() {
        return Ok(Vec::new());
    }
    let out = run(None, &["devices", "-l"], 10).await?;
    Ok(parse_devices(&String::from_utf8_lossy(&out)))
}

/// Quote one word for the phone's `sh`.
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-/:=@%+,".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// One serial's adb.
#[derive(Debug, Clone)]
pub struct Adb {
    pub serial: String,
}

impl Adb {
    pub fn new(serial: impl Into<String>) -> Self {
        Self { serial: serial.into() }
    }

    /// `adb exec-out <cmd>`: raw bytes, no terminal translation.
    pub async fn exec_out(&self, cmd: &str, secs: u64) -> Result<Vec<u8>, AdbError> {
        run(Some(&self.serial), &["exec-out", cmd], secs).await
    }

    /// `adb shell <cmd>` as text. The exit status of `cmd` is adb's own.
    pub async fn shell(&self, cmd: &str, secs: u64) -> Result<String, AdbError> {
        let out = run(Some(&self.serial), &["shell", cmd], secs).await?;
        Ok(String::from_utf8_lossy(&out).replace("\r\n", "\n"))
    }

    pub async fn getprop(&self, key: &str) -> Option<String> {
        let v = self.shell(&format!("getprop {}", sh_quote(key)), 8).await.ok()?;
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_list_parses_usb_wifi_emulator_and_unauthorized() {
        let text = "List of devices attached\n\
            R39N402N5SB            device usb:2-1 product:bloomqks model:SM_F700N device:bloomq transport_id:4\n\
            192.168.0.7:5555       device product:bloomqks model:SM_F700N device:bloomq transport_id:5\n\
            emulator-5554          device product:sdk_gphone64_arm64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n\
            0123456789ABCDEF       unauthorized usb:1-1 transport_id:6\n\
            ZY22                   no permissions (user in plugdev group); see [http://developer.android.com/tools/device.html] usb:3-2 transport_id:7\n\n";
        let list = parse_devices(text);
        assert_eq!(list.len(), 5);
        assert_eq!(list[0].serial, "R39N402N5SB");
        assert_eq!(list[0].state, "device");
        assert_eq!(list[0].connection, "usb");
        assert_eq!(list[0].model.as_deref(), Some("SM_F700N"));
        assert_eq!(list[1].connection, "wifi");
        assert_eq!(list[1].transport_id.as_deref(), Some("5"));
        assert_eq!(list[2].connection, "emulator");
        assert_eq!(list[3].state, "unauthorized");
        assert!(list[4].state.starts_with("no permissions"), "{}", list[4].state);
        assert_eq!(list[4].connection, "usb");
        assert!(parse_devices("* daemon not running; starting now at tcp:5037\n* daemon started successfully\nList of devices attached\n\n").is_empty());
    }

    #[test]
    fn shell_words_are_quoted() {
        assert_eq!(sh_quote("com.android.settings"), "com.android.settings");
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("$(reboot)"), "'$(reboot)'");
    }

    #[test]
    fn adb_reports_a_missing_device_as_unreachable() {
        assert!(unreachable("error: device 'XYZ' not found"));
        assert!(unreachable("error: device unauthorized.\nThis adb server's $ADB_VENDOR_KEYS is not set"));
        assert!(unreachable("error: device offline"));
        assert!(!unreachable("Error: could not get idle state."));
    }
}
