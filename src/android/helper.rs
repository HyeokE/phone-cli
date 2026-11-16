//! The phone-side helper: a small dex (`android-helper/`, embedded here)
//! pushed to `/data/local/tmp` and run with `app_process`, as scrcpy runs its
//! server. It is not an installed app and goes away with the adb connection
//! or a reboot. One runs per daemon and stays up, so a reading is one walk of
//! a UiAutomation connection that is already open (tens of milliseconds)
//! instead of a `uiautomator dump` (a new VM, a new connection and a fixed
//! one-second idle wait), input is `InputManager.injectInputEvent` instead of
//! a new `input` process per event, and text `input text` cannot type
//! (Korean, emoji) goes through the clipboard and a paste.
//!
//! Protocol over the helper's stdin/stdout (`adb shell`, no forward): frames
//! of `P` `H` `<u32 big-endian length>` `<UTF-8 JSON>`. The helper speaks
//! first with a hello carrying its protocol version and what works on this
//! phone; then each request `{"id","op",…}` gets one answer `{"id","ok",…}`.
//!
//! When the helper is off, does not start, or breaks, the session falls back
//! to the adb tools ([`super::session`]), and says which path it used.

use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout},
};

use super::{adb::Adb, uia::XmlNode};

/// The protocol version both sides speak; `Main.VERSION` in the Java.
pub const VERSION: u64 = 1;
/// The built helper, as committed (`scripts/build-android-helper.sh`).
pub const JAR: &[u8] = include_bytes!("../../android-helper/phone-helper.jar");
const JAR_SHA256_FILE: &str = include_str!("../../android-helper/phone-helper.jar.sha256");
pub const REMOTE_JAR: &str = "/data/local/tmp/phone-helper.jar";
pub const MAIN_CLASS: &str = "dev.phonecli.helper.Main";
/// The helper's process name on the phone (`ps -A -o NAME`).
pub const PROCESS_NAME: &str = "phone-helper";

/// A shell line that ends every helper process on the phone. Matched by the
/// process name `ps` shows, then killed by pid: toybox `pkill -x` does not
/// match the name `--nice-name` sets (observed on Android 13), and
/// `pkill -f phone-helper` would also kill the shell running it.
pub fn kill_leftovers_cmd() -> String {
    format!(
        "for p in $(ps -A -o PID=,NAME= | awk '$2==\"{PROCESS_NAME}\"{{print $1}}'); do kill $p; done; true"
    )
}

/// Who runs the helper now: `<host> <daemon pid>`, written before a helper
/// starts. A second daemon for the same phone (another state folder, a
/// second binary) must not kill a helper a live daemon is using: each kill
/// would cut the other daemon's command, and they would take turns killing.
pub const OWNER_FILE: &str = "/data/local/tmp/phone-helper.owner";

/// This daemon's owner line: host name and pid.
pub fn owner_line() -> String {
    format!("{} {}", host_name(), std::process::id())
}

fn host_name() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer outlives the call and its length is passed.
        let r = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        if r == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
            if !name.is_empty() {
                return name;
            }
        }
    }
    std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "unknown".into())
}

/// Whether a process with this pid runs on this computer.
fn local_pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// The other live daemon on this computer the recorded owner names, if the
/// helper running on the phone belongs to one. `owner` is the file's text,
/// `running` whether any helper process is up there.
pub fn live_other_owner(owner: &str, running: bool, me: &str, alive: impl Fn(u32) -> bool) -> Option<u32> {
    if !running {
        return None;
    }
    let line = owner.lines().next()?.trim();
    if line.is_empty() || line == me {
        return None;
    }
    let (host, pid) = line.rsplit_once(' ')?;
    let my_host = me.rsplit_once(' ').map(|(h, _)| h).unwrap_or("");
    let pid: u32 = pid.parse().ok()?;
    // A helper started from another computer cannot be checked from here;
    // it is treated as a leftover, as before.
    (host == my_host && alive(pid)).then_some(pid)
}

fn probe_cmd() -> String {
    format!(
        "echo owner:$(cat {OWNER_FILE} 2>/dev/null); echo running:$(ps -A -o PID=,NAME= | awk '$2==\"{PROCESS_NAME}\"{{print $1}}' | head -1)"
    )
}

/// The pid of the other live daemon whose helper is on the phone, if any.
pub async fn other_owner(adb: &Adb) -> Option<u32> {
    let out = adb.shell(&probe_cmd(), 10).await.ok()?;
    let field = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(k))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    live_other_owner(&field("owner:"), !field("running:").is_empty(), &owner_line(), local_pid_alive)
}

/// End helpers left on the phone (a daemon that died, an earlier build),
/// unless a live daemon on this computer owns the one running (then its pid).
/// Only one UiAutomation connection can be registered at a time, so a
/// leftover one blocks both a new helper's snapshots and `uiautomator dump`.
pub async fn kill_leftovers(adb: &Adb) -> Result<(), u32> {
    if let Some(pid) = other_owner(adb).await {
        return Err(pid);
    }
    let _ = adb.shell(&kill_leftovers_cmd(), 10).await;
    Ok(())
}

/// What to say when another daemon on this computer is using the phone.
pub fn in_use_message(pid: u32) -> String {
    format!("another phone daemon (pid {pid}) on this computer is using this phone's helper")
}
const MAGIC: &[u8; 2] = b"PH";
const MAX_FRAME: usize = 32 << 20;
const HELLO_SECS: u64 = 12;

/// The sha256 the committed `.sha256` file records for [`JAR`].
pub fn jar_sha256_recorded() -> &'static str {
    JAR_SHA256_FILE.split_whitespace().next().unwrap_or("")
}

/// The sha256 of the embedded jar, computed.
pub fn jar_sha256() -> String {
    hex(&Sha256::digest(JAR))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether to use the helper: `[android] helper` in the config, unless
/// `PHONE_ANDROID_HELPER` says otherwise (`0`/`false`/`off`/`no` to turn it
/// off, `1`/`true`/`on`/`yes` to turn it on).
pub fn enabled(config: bool, env: Option<&str>) -> bool {
    match env.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if ["0", "false", "off", "no"].contains(&v.as_str()) => false,
        Some(v) if ["1", "true", "on", "yes"].contains(&v.as_str()) => true,
        _ => config,
    }
}

/// One frame: magic, length, JSON.
pub fn encode_frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).expect("a JSON value serializes");
    let mut out = Vec::with_capacity(body.len() + 6);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// A frame header's body length.
pub fn decode_header(h: &[u8; 6]) -> Result<usize, String> {
    if &h[..2] != MAGIC {
        return Err(format!(
            "bad frame magic {:02x}{:02x} (something else wrote to the helper's stdout)",
            h[0], h[1]
        ));
    }
    let n = u32::from_be_bytes([h[2], h[3], h[4], h[5]]) as usize;
    if n > MAX_FRAME {
        return Err(format!("frame of {n} bytes is too large"));
    }
    Ok(n)
}

/// Decode every whole frame at the start of `buf`; the rest is left.
pub fn decode_frames(buf: &[u8]) -> Result<(Vec<Value>, usize), String> {
    let mut out = Vec::new();
    let mut at = 0;
    while buf.len() - at >= 6 {
        let h: [u8; 6] = buf[at..at + 6].try_into().unwrap();
        let n = decode_header(&h)?;
        if buf.len() - at - 6 < n {
            break;
        }
        let v = serde_json::from_slice(&buf[at + 6..at + 6 + n])
            .map_err(|e| format!("bad frame JSON: {e}"))?;
        out.push(v);
        at += 6 + n;
    }
    Ok((out, at))
}

/// What works in this helper on this phone.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caps {
    pub snapshot: bool,
    pub inject: bool,
    pub clipboard: bool,
    pub labels: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub version: u64,
    pub sdk: u64,
    pub pid: Option<i64>,
    pub caps: Caps,
    /// Why a capability is missing, by capability.
    pub errors: Vec<(String, String)>,
    pub start_ms: Option<u64>,
}

/// The helper's first frame. Fails on anything but this protocol version.
pub fn parse_hello(v: &Value) -> Result<Hello, String> {
    if v["hello"] != "phone-helper" {
        return Err(format!(
            "not a phone-helper hello: {}",
            truncate(&v.to_string(), 200)
        ));
    }
    let version = v["version"].as_u64().ok_or("the hello has no version")?;
    if version != VERSION {
        return Err(format!(
            "the helper speaks protocol {version}, phone-cli speaks {VERSION}"
        ));
    }
    let caps = &v["caps"];
    let flag = |k: &str| caps[k].as_bool().unwrap_or(false);
    let errors = ["uia_error", "inject_error", "clipboard_error"]
        .iter()
        .filter_map(|k| {
            v[*k]
                .as_str()
                .map(|e| (k.trim_end_matches("_error").to_string(), e.to_string()))
        })
        .collect();
    Ok(Hello {
        version,
        sdk: v["sdk"].as_u64().unwrap_or(0),
        pid: v["pid"].as_i64(),
        caps: Caps {
            snapshot: flag("snapshot"),
            inject: flag("inject"),
            clipboard: flag("clipboard"),
            labels: flag("labels"),
        },
        errors,
        start_ms: v["start_ms"].as_u64(),
    })
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Why a call did not give an answer.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The request never reached the helper: nothing happened on the phone.
    NotSent(String),
    /// The request went out and no (sane) answer came back: it may have run.
    /// The helper is not usable any more.
    Lost(String),
    /// The helper answered that it could not. `sent` is whether anything may
    /// have reached the screen before it stopped.
    Refused {
        code: String,
        message: String,
        sent: bool,
        extra: Value,
    },
}

impl CallError {
    pub fn message(&self) -> String {
        match self {
            CallError::NotSent(m) | CallError::Lost(m) => m.clone(),
            CallError::Refused { code, message, .. } => format!("{code}: {message}"),
        }
    }

    /// Whether the helper process is gone or unusable after this.
    pub fn broke_helper(&self) -> bool {
        !matches!(self, CallError::Refused { .. })
    }

    /// Whether the phone may have received input.
    pub fn maybe_sent(&self) -> bool {
        match self {
            CallError::NotSent(_) => false,
            CallError::Lost(_) => true,
            CallError::Refused { sent, .. } => *sent,
        }
    }
}

/// An answer frame to the result of the call: the answer itself when `ok`.
pub fn parse_answer(id: u64, v: Value) -> Result<Value, CallError> {
    if v["id"].as_u64() != Some(id) {
        return Err(CallError::Lost(format!(
            "answer for request {} came for {id}",
            v["id"]
        )));
    }
    match v["ok"].as_bool() {
        Some(true) => Ok(v),
        Some(false) => Err(CallError::Refused {
            code: v["code"].as_str().unwrap_or("helper_error").to_string(),
            message: v["error"].as_str().unwrap_or("").to_string(),
            sent: v["sent"].as_bool().unwrap_or(true),
            extra: v,
        }),
        None => Err(CallError::Lost("answer without `ok`".into())),
    }
}

/// One running helper.
pub struct Helper {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
    next_id: u64,
    pub hello: Hello,
    stderr: Arc<Mutex<String>>,
    pub started: Instant,
}

impl Helper {
    /// Make sure the phone has this build of the jar, put down helpers left
    /// over on it, start one and wait for its hello.
    pub async fn start(adb: &Adb) -> Result<Helper, String> {
        let want = jar_sha256();
        // One shell: the hash of what is there, and old helpers put down so
        // their UiAutomation connection does not block ours (only one can be
        // registered at a time).
        if let Some(pid) = other_owner(adb).await {
            return Err(in_use_message(pid));
        }
        let probe = format!(
            "sha256sum {REMOTE_JAR} 2>/dev/null; {}; echo '{}' > {OWNER_FILE}",
            kill_leftovers_cmd(),
            owner_line()
        );
        let have = adb.shell(&probe, 15).await.map_err(|e| e.message())?;
        if have.split_whitespace().next() != Some(want.as_str()) {
            push_jar(adb, &want).await?;
        }
        let adb_path = super::adb::adb_path().ok_or("adb was not found")?;
        let cmd = format!(
            "CLASSPATH={REMOTE_JAR} exec app_process / --nice-name={PROCESS_NAME} {MAIN_CLASS}"
        );
        let mut child = tokio::process::Command::new(adb_path)
            .arg("-s")
            .arg(&adb.serial)
            .arg("shell")
            .arg(&cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot run adb: {e}"))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let mut stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut err) = child.stderr.take() {
            let tail = stderr.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                while let Ok(n) = err.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut t = tail.lock().unwrap();
                    t.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if t.len() > 4096 {
                        let cut = t.len() - 4096;
                        let cut = (cut..t.len())
                            .find(|i| t.is_char_boundary(*i))
                            .unwrap_or(t.len());
                        t.drain(..cut);
                    }
                }
            });
        }
        let started = Instant::now();
        let first =
            tokio::time::timeout(Duration::from_secs(HELLO_SECS), read_frame(&mut stdout)).await;
        let hello = match first {
            Ok(Ok(v)) => parse_hello(&v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("no hello within {HELLO_SECS} s")),
        };
        let hello = match hello {
            Ok(h) => h,
            Err(e) => {
                let _ = child.start_kill();
                tokio::time::sleep(Duration::from_millis(50)).await;
                let tail = last_line(&stderr.lock().unwrap());
                return Err(if tail.is_empty() {
                    e
                } else {
                    format!("{e}; helper said: {tail}")
                });
            }
        };
        Ok(Helper {
            child,
            stdin,
            stdout,
            next_id: 1,
            hello,
            stderr,
            started,
        })
    }

    /// Whether the helper process is still there.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// The last line the helper wrote to stderr.
    pub fn stderr_tail(&self) -> String {
        last_line(&self.stderr.lock().unwrap())
    }

    /// One request and its answer, within `secs`.
    pub async fn call(&mut self, op: &str, mut args: Value, secs: u64) -> Result<Value, CallError> {
        if !self.alive() {
            return Err(CallError::NotSent(format!(
                "the helper exited ({})",
                self.stderr_tail()
            )));
        }
        let id = self.next_id;
        self.next_id += 1;
        if !args.is_object() {
            args = json!({});
        }
        args["id"] = json!(id);
        args["op"] = json!(op);
        let frame = encode_frame(&args);
        // A frame that is not written whole is not acted on: the helper
        // reads whole frames only.
        if let Err(e) = async {
            self.stdin.write_all(&frame).await?;
            self.stdin.flush().await
        }
        .await
        {
            return Err(CallError::NotSent(format!(
                "could not write to the helper: {e}"
            )));
        }
        match tokio::time::timeout(Duration::from_secs(secs), read_frame(&mut self.stdout)).await {
            Ok(Ok(v)) => parse_answer(id, v),
            Ok(Err(e)) => Err(CallError::Lost(format!("{e} ({})", self.stderr_tail()))),
            Err(_) => Err(CallError::Lost(format!(
                "no answer from the helper within {secs} s"
            ))),
        }
    }

    /// Ask it to quit, then make sure. Killing the local `adb shell` does not
    /// end the process on the phone, so it is killed there by pid as well.
    pub async fn stop(mut self, adb: &Adb) {
        let _ =
            tokio::time::timeout(Duration::from_millis(800), self.call("quit", json!({}), 1)).await;
        let _ = self.child.start_kill();
        if let Some(pid) = self.hello.pid {
            let _ = adb.shell(&format!("kill {pid} 2>/dev/null; true"), 5).await;
        }
    }
}

fn last_line(s: &str) -> String {
    let l = s
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    truncate(l, 300)
}

async fn read_frame(r: &mut ChildStdout) -> Result<Value, String> {
    let mut h = [0u8; 6];
    r.read_exact(&mut h)
        .await
        .map_err(|e| format!("the helper closed its output ({e})"))?;
    let n = decode_header(&h)?;
    let mut body = vec![0u8; n];
    r.read_exact(&mut body)
        .await
        .map_err(|e| format!("the helper's answer broke off ({e})"))?;
    serde_json::from_slice(&body).map_err(|e| format!("bad answer from the helper: {e}"))
}

/// Push the embedded jar and check it arrived whole. Read-only on the phone,
/// as newer Android wants for code it loads.
async fn push_jar(adb: &Adb, want: &str) -> Result<(), String> {
    let dir = std::env::temp_dir().join(format!("phone-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let local = dir.join(format!("phone-helper-{}.jar", &want[..12]));
    std::fs::write(&local, JAR).map_err(|e| format!("{}: {e}", local.display()))?;
    let _ = adb.shell(&format!("rm -f {REMOTE_JAR}"), 10).await;
    let local_s = local.to_string_lossy().to_string();
    let pushed = super::adb::run(Some(&adb.serial), &["push", &local_s, REMOTE_JAR], 60).await;
    let _ = std::fs::remove_file(&local);
    let _ = std::fs::remove_dir(&dir);
    pushed.map_err(|e| format!("adb push: {}", e.message()))?;
    let check = adb
        .shell(
            &format!("chmod 0444 {REMOTE_JAR}; sha256sum {REMOTE_JAR}"),
            15,
        )
        .await
        .map_err(|e| e.message())?;
    if check.split_whitespace().next() != Some(want) {
        return Err(format!(
            "the pushed jar does not hash to {want}: {}",
            check.trim()
        ));
    }
    Ok(())
}

/// The helper's flag bits for one node (`Uia.F_*` in the Java).
pub mod flag {
    pub const CHECKABLE: u64 = 1;
    pub const CHECKED: u64 = 1 << 1;
    pub const CLICKABLE: u64 = 1 << 2;
    pub const ENABLED: u64 = 1 << 3;
    pub const FOCUSABLE: u64 = 1 << 4;
    pub const FOCUSED: u64 = 1 << 5;
    pub const SCROLLABLE: u64 = 1 << 6;
    pub const LONG_CLICKABLE: u64 = 1 << 7;
    pub const PASSWORD: u64 = 1 << 8;
    pub const SELECTED: u64 = 1 << 9;
    pub const EDITABLE: u64 = 1 << 10;
    pub const SHOWING_HINT: u64 = 1 << 11;
}

/// A helper snapshot to the same tree a `uiautomator dump` parses into: a
/// `hierarchy` element whose `node`s carry uiautomator's attribute names, in
/// its order, so [`super::uia::build_screen`] treats both alike. Two extra
/// attributes come only from the helper: `editable` and `showing-hint`.
pub fn snapshot_to_tree(answer: &Value) -> Result<XmlNode, String> {
    let root = answer
        .get("root")
        .filter(|r| r.is_object())
        .ok_or("the snapshot has no root")?;
    let mut hierarchy = XmlNode {
        name: "hierarchy".into(),
        attrs: vec![("rotation".into(), "0".into())],
        children: vec![],
    };
    hierarchy.children.push(node_from(root, 0)?);
    Ok(hierarchy)
}

fn node_from(v: &Value, depth: usize) -> Result<XmlNode, String> {
    if depth > 512 {
        return Err("the snapshot is nested too deep".into());
    }
    let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
    let f = v["f"].as_u64().unwrap_or(flag::ENABLED);
    let b = |k: &str| {
        if f & match k {
            "checkable" => flag::CHECKABLE,
            "checked" => flag::CHECKED,
            "clickable" => flag::CLICKABLE,
            "enabled" => flag::ENABLED,
            "focusable" => flag::FOCUSABLE,
            "focused" => flag::FOCUSED,
            "scrollable" => flag::SCROLLABLE,
            "long-clickable" => flag::LONG_CLICKABLE,
            "password" => flag::PASSWORD,
            "selected" => flag::SELECTED,
            "editable" => flag::EDITABLE,
            "showing-hint" => flag::SHOWING_HINT,
            _ => 0,
        } != 0
        {
            "true".to_string()
        } else {
            "false".to_string()
        }
    };
    let bounds = match v["b"]
        .as_array()
        .map(|a| a.iter().map(|n| n.as_i64()).collect::<Option<Vec<_>>>())
    {
        Some(Some(n)) if n.len() == 4 => format!("[{},{}][{},{}]", n[0], n[1], n[2], n[3]),
        _ => "[0,0][0,0]".to_string(),
    };
    let mut attrs: Vec<(String, String)> = vec![
        ("index".into(), v["i"].as_u64().unwrap_or(0).to_string()),
        ("text".into(), s("t")),
        ("resource-id".into(), s("r")),
        ("class".into(), s("k")),
        ("package".into(), s("p")),
        ("content-desc".into(), s("d")),
    ];
    for k in [
        "checkable",
        "checked",
        "clickable",
        "enabled",
        "focusable",
        "focused",
        "scrollable",
        "long-clickable",
        "password",
        "selected",
    ] {
        attrs.push((k.into(), b(k)));
    }
    attrs.push(("bounds".into(), bounds));
    for k in ["editable", "showing-hint"] {
        attrs.push((k.into(), b(k)));
    }
    let children = match v["c"].as_array() {
        Some(c) => c
            .iter()
            .map(|c| node_from(c, depth + 1))
            .collect::<Result<Vec<_>, _>>()?,
        None => vec![],
    };
    Ok(XmlNode {
        name: "node".into(),
        attrs,
        children,
    })
}

/// What the session does with its helper between commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next {
    /// Use the one that is running.
    Use,
    /// Start one now.
    Start,
    /// Go through adb: the helper is off, or broke recently.
    Adb,
}

/// When to try again after `failures` failures in a row: at once after the
/// first (a crash or a phone that went away and came back), then after 30 s,
/// then every 5 minutes, so a phone the helper cannot run on costs one try now
/// and then, not one per command.
pub fn backoff(failures: u32) -> Duration {
    match failures {
        0 | 1 => Duration::ZERO,
        2 => Duration::from_secs(30),
        _ => Duration::from_secs(300),
    }
}

/// The choice for the next helper-capable step.
pub fn next(
    enabled: bool,
    running: bool,
    failures: u32,
    last_failure: Option<Instant>,
    now: Instant,
) -> Next {
    if !enabled {
        return Next::Adb;
    }
    if running {
        return Next::Use;
    }
    match last_failure {
        Some(at) if now.duration_since(at) < backoff(failures) => Next::Adb,
        _ => Next::Start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        android::uia::{self, Geometry},
        ax::tree::{AppInfo, BuildOptions},
    };

    #[test]
    fn the_embedded_jar_is_the_recorded_build() {
        assert_eq!(
            jar_sha256(),
            jar_sha256_recorded(),
            "run scripts/build-android-helper.sh"
        );
        assert!(JAR.starts_with(b"PK"), "a zip");
        assert!(JAR.len() < 200_000, "small");
    }

    #[test]
    fn both_sides_speak_the_same_version() {
        let java = include_str!("../../android-helper/src/dev/phonecli/helper/Main.java");
        assert!(java.contains(&format!("public static final int VERSION = {VERSION};")));
        assert!(java.contains(&format!(
            "package {};",
            MAIN_CLASS.rsplit_once('.').unwrap().0
        )));
        let uia = include_str!("../../android-helper/src/dev/phonecli/helper/Uia.java");
        for (name, bit) in [
            ("F_CHECKABLE = 1;", flag::CHECKABLE),
            ("F_EDITABLE = 1 << 10;", flag::EDITABLE),
            ("F_SHOWING_HINT = 1 << 11;", flag::SHOWING_HINT),
        ] {
            assert!(uia.contains(name), "{name}");
            assert!(bit > 0);
        }
    }

    #[test]
    fn frames_round_trip_and_split_anywhere() {
        let a = json!({"id": 1, "op": "text", "text": "화면 😀"});
        let b = json!({"id": 2, "op": "ping"});
        let mut buf = encode_frame(&a);
        assert_eq!(&buf[..2], b"PH");
        assert_eq!(
            u32::from_be_bytes(buf[2..6].try_into().unwrap()) as usize,
            buf.len() - 6
        );
        buf.extend(encode_frame(&b));
        let (frames, used) = decode_frames(&buf).unwrap();
        assert_eq!(frames, vec![a.clone(), b]);
        assert_eq!(used, buf.len());
        // A partial second frame waits for more.
        let cut = buf.len() - 3;
        let (frames, used) = decode_frames(&buf[..cut]).unwrap();
        assert_eq!(frames, vec![a]);
        assert!(used < cut);
        assert!(
            decode_header(b"XX\0\0\0\x01")
                .unwrap_err()
                .contains("magic")
        );
        assert!(
            decode_header(&[b'P', b'H', 0xff, 0xff, 0xff, 0xff])
                .unwrap_err()
                .contains("too large")
        );
        assert!(decode_frames(&[b'P', b'H', 0, 0, 0, 1, b'{']).is_err());
    }

    #[test]
    fn the_hello_is_checked() {
        let h = parse_hello(&json!({
            "hello": "phone-helper", "version": VERSION, "sdk": 33, "pid": 123,
            "caps": {"snapshot": false, "inject": true, "clipboard": true, "labels": true},
            "uia_error": "java.lang.IllegalStateException: already registered", "start_ms": 25
        }))
        .unwrap();
        assert_eq!(h.sdk, 33);
        assert_eq!(
            h.caps,
            Caps {
                snapshot: false,
                inject: true,
                clipboard: true,
                labels: true
            }
        );
        assert_eq!(
            h.errors,
            vec![(
                "uia".to_string(),
                "java.lang.IllegalStateException: already registered".to_string()
            )]
        );
        assert!(
            parse_hello(&json!({"hello": "phone-helper", "version": VERSION + 1}))
                .unwrap_err()
                .contains("protocol")
        );
        assert!(parse_hello(&json!({"hello": "other", "version": VERSION})).is_err());
        assert!(parse_hello(&json!({"hello": "phone-helper"})).is_err());
    }

    #[test]
    fn answers_say_whether_anything_was_sent() {
        assert_eq!(
            parse_answer(3, json!({"id": 3, "ok": true, "pid": 9})).unwrap()["pid"],
            9
        );
        match parse_answer(
            4,
            json!({"id": 4, "ok": false, "code": "input_rejected", "error": "no", "sent": false}),
        ) {
            Err(e @ CallError::Refused { .. }) => {
                assert!(!e.maybe_sent());
                assert!(!e.broke_helper());
            }
            other => panic!("{other:?}"),
        }
        // No `sent` is taken as maybe sent.
        assert!(
            parse_answer(5, json!({"id": 5, "ok": false}))
                .unwrap_err()
                .maybe_sent()
        );
        let lost = parse_answer(6, json!({"id": 7, "ok": true})).unwrap_err();
        assert!(lost.maybe_sent() && lost.broke_helper());
        assert!(!CallError::NotSent("x".into()).maybe_sent());
    }

    #[test]
    fn a_live_daemon_here_keeps_its_helper() {
        let me = "mac 100";
        let alive = |p: u32| p == 200;
        // Another live daemon on this computer: hands off.
        assert_eq!(live_other_owner("mac 200\n", true, me, alive), Some(200));
        // Ours, a dead one, nothing running, no record, another computer: a leftover.
        assert_eq!(live_other_owner("mac 100", true, me, alive), None);
        assert_eq!(live_other_owner("mac 300", true, me, alive), None);
        assert_eq!(live_other_owner("mac 200", false, me, alive), None);
        assert_eq!(live_other_owner("", true, me, alive), None);
        assert_eq!(live_other_owner("other-host 200", true, me, alive), None);
        assert_eq!(live_other_owner("garbage", true, me, alive), None);
    }

    #[test]
        fn leftovers_are_killed_by_pid_not_by_pattern() {
        let cmd = kill_leftovers_cmd();
        assert!(cmd.contains("$2==\"phone-helper\""), "{cmd}");
        assert!(!cmd.contains("pkill"), "{cmd}");
    }

    #[test]
    fn the_switch_follows_config_and_env() {
        assert!(enabled(true, None));
        assert!(!enabled(false, None));
        assert!(!enabled(true, Some("0")));
        assert!(!enabled(true, Some("OFF")));
        assert!(enabled(false, Some("1")));
        assert!(enabled(true, Some("")));
        assert!(enabled(true, Some("maybe")));
    }

    #[test]
    fn fallback_waits_longer_after_each_failure() {
        let now = Instant::now();
        assert_eq!(next(false, true, 0, None, now), Next::Adb);
        assert_eq!(next(true, true, 5, Some(now), now), Next::Use);
        assert_eq!(next(true, false, 0, None, now), Next::Start);
        // Right after a first failure: try again at once.
        assert_eq!(next(true, false, 1, Some(now), now), Next::Start);
        // A second: adb for 30 s.
        assert_eq!(
            next(true, false, 2, Some(now), now + Duration::from_secs(5)),
            Next::Adb
        );
        assert_eq!(
            next(true, false, 2, Some(now), now + Duration::from_secs(31)),
            Next::Start
        );
        assert_eq!(
            next(true, false, 3, Some(now), now + Duration::from_secs(120)),
            Next::Adb
        );
        assert_eq!(
            next(true, false, 3, Some(now), now + Duration::from_secs(301)),
            Next::Start
        );
    }

    /// A synthetic Settings-like snapshot: no real screen is stored.
    fn snapshot() -> Value {
        use flag::*;
        json!({
            "package": "com.android.settings",
            "root": {"i": 0, "k": "android.widget.FrameLayout", "p": "com.android.settings", "b": [0, 0, 1080, 2400], "f": ENABLED, "c": [
                {"i": 0, "k": "android.widget.ImageButton", "d": "Navigate up", "p": "com.android.settings",
                 "b": [0, 84, 147, 231], "f": ENABLED | CLICKABLE | FOCUSABLE},
                {"i": 1, "k": "android.widget.EditText", "t": "화면", "d": "Search settings", "r": "com.android.settings:id/search_src_text",
                 "p": "com.android.settings", "b": [147, 84, 900, 231], "f": ENABLED | CLICKABLE | FOCUSABLE | FOCUSED | EDITABLE},
                {"i": 2, "k": "android.widget.Switch", "t": "Wi-Fi", "p": "com.android.settings",
                 "b": [900, 300, 1038, 461], "f": ENABLED | CLICKABLE | CHECKABLE | CHECKED},
                {"i": 3, "k": "android.widget.TextView", "t": "Unavailable", "p": "com.android.settings", "b": [42, 500, 600, 560], "f": 0},
                {"i": 4, "k": "androidx.compose.ui.platform.ComposeView", "t": "Your name", "d": "Name", "p": "com.android.settings",
                 "b": [42, 600, 1038, 700], "f": ENABLED | EDITABLE | SHOWING_HINT},
                {"i": 5, "k": "android.widget.TextView", "t": "Below the fold", "p": "com.android.settings", "b": [0, 0, 0, 0], "f": ENABLED}
            ]}
        })
    }

    #[test]
    fn a_snapshot_maps_to_the_elements_a_dump_gives() {
        let tree = snapshot_to_tree(&snapshot()).unwrap();
        assert_eq!(tree.name, "hierarchy");
        assert_eq!(
            uia::dump_package(&tree).as_deref(),
            Some("com.android.settings")
        );
        let root = &tree.children[0];
        let keys: Vec<&str> = root.attrs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            &keys[..6],
            [
                "index",
                "text",
                "resource-id",
                "class",
                "package",
                "content-desc"
            ]
        );
        assert_eq!(root.attr("bounds"), Some("[0,0][1080,2400]"));
        assert_eq!(root.attr("clickable"), Some("false"));

        let geo = Geometry {
            px: (1080, 2400),
            scale: 3.0,
        };
        let app = AppInfo {
            pid: 7,
            name: Some("Settings".into()),
            bundle_id: Some("com.android.settings".into()),
        };
        let s = uia::build_screen(&tree, geo, &app, BuildOptions::default());
        let back = s
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Navigate up"))
            .unwrap();
        assert_eq!(back.role, "button");
        assert_eq!(back.frame.map(|f| (f.x, f.y)), Some((0.0, 28.0)));
        let search = s
            .elements
            .iter()
            .find(|e| e.role == "search_field")
            .unwrap();
        assert!(search.focused);
        assert_eq!(search.value.as_deref(), Some("화면"));
        assert_eq!(
            search.identifier.as_deref(),
            Some("com.android.settings:id/search_src_text")
        );
        let wifi = s.elements.iter().find(|e| e.role == "switch").unwrap();
        assert_eq!(wifi.value.as_deref(), Some("1"));
        assert!(
            !s.elements
                .iter()
                .find(|e| e.label.as_deref() == Some("Unavailable"))
                .unwrap()
                .enabled
        );
        // Editable by the flag even when the class is not an EditText.
        let compose = s
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Name"))
            .unwrap();
        assert_eq!(compose.role, "text_field");
        assert_eq!(
            compose.value.as_deref(),
            Some(""),
            "a placeholder is not the field's value"
        );
        assert_eq!(compose.hint.as_deref(), Some("Your name"));
        assert!(
            !s.elements
                .iter()
                .any(|e| e.label.as_deref() == Some("Below the fold"))
        );
        let all = uia::build_screen(
            &tree,
            geo,
            &app,
            BuildOptions {
                include_offscreen: true,
                raw: true,
            },
        );
        let raw = all
            .elements
            .iter()
            .find(|e| e.label.as_deref() == Some("Below the fold"))
            .unwrap()
            .raw
            .clone()
            .unwrap();
        assert_eq!(raw["class"], "android.widget.TextView");
        assert_eq!(raw["editable"], "false");
    }

    #[test]
    fn bad_snapshots_are_refused() {
        assert!(snapshot_to_tree(&json!({"package": "x"})).is_err());
        let mut deep = json!({"i": 0});
        for _ in 0..600 {
            deep = json!({"i": 0, "c": [deep]});
        }
        assert!(
            snapshot_to_tree(&json!({"root": deep}))
                .unwrap_err()
                .contains("deep")
        );
    }
}
