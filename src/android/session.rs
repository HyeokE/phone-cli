//! An Android phone over adb: the same commands and result fields as the
//! iPhone's [`Session`](crate::device::session::Session).
//!
//! Two paths do the same work. The helper ([`super::helper`], on by default)
//! is one long-lived process on the phone: a reading is a walk of its open
//! UiAutomation connection, input is `injectInputEvent`, and text `input
//! text` cannot type is pasted through the clipboard. The adb path runs the
//! tools Android ships, one `adb` process per call: `uiautomator dump`,
//! `input tap|swipe|motionevent|keyevent|text`. When the helper is off, does
//! not start or breaks, the adb path takes over, and each result says which
//! one served it (`backend`). Apps are `am`/`pm`/`cmd package` either way
//! (with the helper adding their names), screenshots `screencap -p`.
//!
//! Both wait for the app's UI to go idle before reading (the helper for a
//! short quiet spell, `uiautomator dump` for a full second), so one reading
//! after an input already shows the screen once it has stopped moving; where
//! the iPhone path reads twice to see that a screen holds still, this one
//! reads once.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    adb::{Adb, AdbError, sh_quote},
    helper::{self, CallError, Caps, Helper, Next},
    keys,
    uia::{self, Geometry, XmlNode},
};
use crate::{
    actions::{
        self, AFTER_DRAG, AFTER_LAUNCH, AFTER_PASTE, ButtonArgs, CHANGE_WAIT, FindArgs, KeyArgs,
        LaunchArgs, ReadArgs, ScreenshotArgs, ScrollArgs, SwipeArgs, TapArgs, Target, TypeArgs,
        TypeObservation, brief, parse_target, resolve_one, tap_point,
    },
    ax::tree::{AppInfo, BuildOptions, Element, ElementSignature, Screen},
    config::Config,
    device::{
        input::Motion,
        session::{Fresh, reuse},
    },
    fields,
    i18n::t,
    platform::Phone,
    reply::{Fail, Outcome, Res},
    selector::{Selector, dedupe_targets},
};

/// How long after a tap to dump. The dump itself waits for the UI to idle.
const AFTER_TAP: Duration = Duration::from_millis(300);
const DUMP_SECS: u64 = 25;
const INPUT_SECS: u64 = 30;
const DUMP_FILE: &str = "/data/local/tmp/phone-cli-ui.xml";
/// How long the UI must have been quiet before the helper reads it, and how
/// long it waits for that at most (then reads anyway).
const HELPER_IDLE_MS: u64 = 250;
const HELPER_IDLE_TIMEOUT_MS: u64 = 3000;
/// How long the helper waits for pasted text to show in the field before it
/// puts the clipboard back.
const PASTE_WAIT_MS: u64 = 800;
/// A drag that comes to rest holds still this long before lifting.
const DRAG_HOLD_MS: u64 = 250;
/// Between readings while waiting for a tap to change the screen: helper
/// readings are fast enough to need a pause.
const POLL_GAP: Duration = Duration::from_millis(80);

/// One input, as the helper takes it and as `input` commands.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// `count` taps (2: a double tap); held `hold_ms` when that is not 0.
    Tap {
        x: i64,
        y: i64,
        hold_ms: u64,
        count: u32,
    },
    Taps(Vec<(i64, i64)>),
    /// A finger from `from` to `to` over `ms`. With `hold_ms` it rests at the
    /// end before lifting (a drag), without it lifts at speed (a flick).
    Swipe {
        from: (i64, i64),
        to: (i64, i64),
        ms: u64,
        hold_ms: u64,
    },
    Keys(Vec<u32>),
    Combo {
        code: u32,
        modifiers: Vec<u32>,
    },
    /// Printable ASCII as key presses.
    Text(String),
}

impl Input {
    /// The helper's op and arguments.
    pub fn helper_request(&self) -> (&'static str, Value) {
        match self {
            Input::Tap {
                x,
                y,
                hold_ms,
                count,
            } => (
                "tap",
                json!({"x": x, "y": y, "hold_ms": hold_ms, "count": count}),
            ),
            Input::Taps(points) => (
                "taps",
                json!({"points": points.iter().map(|(x, y)| json!([x, y])).collect::<Vec<_>>()}),
            ),
            Input::Swipe {
                from,
                to,
                ms,
                hold_ms,
            } => (
                "swipe",
                json!({"x1": from.0, "y1": from.1, "x2": to.0, "y2": to.1, "ms": ms, "hold_ms": hold_ms}),
            ),
            Input::Keys(codes) => ("keys", json!({"codes": codes})),
            Input::Combo { code, modifiers } => {
                ("key", json!({"code": code, "modifiers": modifiers}))
            }
            Input::Text(text) => ("text", json!({"text": text})),
        }
    }

    /// The same input as `input` commands for one shell. A drag that comes
    /// to rest is a `motionevent` sequence (Android 10+) that holds still at
    /// the end before lifting, so the list moves by the finger's distance and
    /// does not coast; a flick is `input swipe`.
    pub fn adb_command(&self, sdk: u32) -> Res<String> {
        Ok(match self {
            Input::Tap {
                x,
                y,
                hold_ms,
                count,
            } => {
                if *hold_ms > 0 {
                    format!("input swipe {x} {y} {x} {y} {hold_ms}")
                } else {
                    vec![format!("input tap {x} {y}"); (*count).max(1) as usize].join("; ")
                }
            }
            Input::Taps(points) => points
                .iter()
                .map(|(x, y)| format!("input tap {x} {y}"))
                .collect::<Vec<_>>()
                .join("; "),
            Input::Swipe {
                from: (x1, y1),
                to: (x2, y2),
                ms,
                hold_ms,
            } => {
                let ms = (*ms).max(50);
                if *hold_ms == 0 || sdk < 29 {
                    return Ok(format!("input swipe {x1} {y1} {x2} {y2} {ms}"));
                }
                let steps = ((ms / 60) as i64).clamp(4, 24);
                let mut cmd = format!("input motionevent DOWN {x1} {y1}");
                for i in 1..=steps {
                    let x = x1 + (x2 - x1) * i / steps;
                    let y = y1 + (y2 - y1) * i / steps;
                    cmd.push_str(&format!("; input motionevent MOVE {x} {y}"));
                }
                let hold = *hold_ms as f64 / 1000.0;
                cmd.push_str(&format!("; sleep {hold}; input motionevent MOVE {x2} {y2}; input motionevent UP {x2} {y2}"));
                cmd
            }
            Input::Keys(codes) => codes
                .chunks(100)
                .map(|c| {
                    format!(
                        "input keyevent {}",
                        c.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
            Input::Combo { code, modifiers } => {
                if modifiers.is_empty() {
                    format!("input keyevent {code}")
                } else if sdk >= 33 {
                    let mods: Vec<String> = modifiers.iter().map(|m| m.to_string()).collect();
                    format!("input keycombination {} {code}", mods.join(" "))
                } else {
                    return Err(Fail::not_sent(
                        "unsupported",
                        "key combinations need Android 13 (API 33) or later without the helper",
                    ));
                }
            }
            Input::Text(text) => {
                keys::typeable(text).map_err(|why| Fail::not_sent("unsupported_text", why))?;
                format!("input text {}", keys::input_text_arg(text))
            }
        })
    }
}

/// An input the helper took and may have delivered, then failed.
fn helper_input_fail(e: &CallError) -> Fail {
    Fail::unknown("input_failed", format!("the helper: {}", e.message())).hint(t(
        "입력이 나갔을 수 있습니다. 다시 보내지 말고 `phone read`로 화면을 확인하세요.",
        "The input may have gone out. Do not resend; check the screen with `phone read`.",
    ))
}

/// Whether the helper is on for this daemon: `[android] helper`, unless
/// `PHONE_ANDROID_HELPER` says otherwise.
pub fn helper_wanted(config: &Config) -> bool {
    helper::enabled(
        config.android.helper,
        crate::paths::env_var("PHONE_ANDROID_HELPER")
            .ok()
            .as_deref(),
    )
}

struct Reading {
    seq: u64,
    at: Instant,
    root: Arc<XmlNode>,
    app: AppInfo,
}

pub struct AndroidSession {
    pub serial: String,
    pub config: Config,
    adb: Adb,
    geometry: Option<Geometry>,
    sdk: Option<u32>,
    latest: Option<Reading>,
    seq: u64,
    stale_before: Option<Instant>,
    /// Pid of each package seen in front, until it is launched again.
    pids: HashMap<String, i64>,
    helper_on: bool,
    helper: Option<Helper>,
    helper_failures: u32,
    helper_failed_at: Option<Instant>,
    helper_error: Option<String>,
    /// What served the current command: the helper, adb's tools, or both.
    used_helper: bool,
    used_adb: bool,
    /// App names by package, as the helper read them (None: it had none).
    labels: HashMap<String, Option<String>>,
}

/// A pid for a package `pidof` does not know (a system window): negative,
/// so it cannot be taken for a real process, and stable per package.
fn pseudo_pid(pkg: &str) -> i64 {
    let mut h: u32 = 0x811c_9dc5;
    for b in pkg.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    -((h & 0x7fff_ffff) as i64) - 1
}

fn parse_pid(out: &str) -> Option<i64> {
    out.split_whitespace().next()?.parse().ok()
}

/// What `input` prints when it refused the event (it still exits 0).
fn input_refused(out: &str) -> Option<String> {
    let line = out.lines().find(|l| {
        l.contains("Exception") || l.starts_with("Error") || l.contains("Unknown command")
    })?;
    Some(line.trim().to_string())
}

/// `cmd package query-activities --brief` / `resolve-activity --brief`:
/// lines that are `package/activity`.
pub fn parse_components(out: &str) -> Vec<(String, String)> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.contains(' ') && !l.contains('=') && l.contains('/'))
        .filter_map(|l| {
            l.split_once('/')
                .map(|(p, a)| (p.to_string(), a.to_string()))
        })
        .collect()
}

/// `pm list packages`: `package:<name>` lines.
pub fn parse_packages(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.trim().strip_prefix("package:"))
        .map(|p| p.trim().to_string())
        .collect()
}

impl AndroidSession {
    pub fn new(serial: String, config: Config) -> Self {
        Self {
            adb: Adb::new(serial.clone()),
            serial,
            helper_on: helper_wanted(&config),
            config,
            geometry: None,
            sdk: None,
            latest: None,
            seq: 0,
            stale_before: None,
            pids: HashMap::new(),
            helper: None,
            helper_failures: 0,
            helper_failed_at: None,
            helper_error: None,
            used_helper: false,
            used_adb: false,
            labels: HashMap::new(),
        }
    }

    /// Start of a command: nothing has served it yet.
    pub fn begin_command(&mut self) {
        self.used_helper = false;
        self.used_adb = false;
    }

    /// What served the command: `adb` when any reading or input of it went
    /// through adb's tools, `helper` when all went through the helper.
    pub fn backend(&self) -> Option<&'static str> {
        if self.used_adb {
            Some("adb")
        } else if self.used_helper {
            Some("helper")
        } else {
            None
        }
    }

    fn note_helper_failure(&mut self, why: String) {
        self.helper_failures += 1;
        self.helper_failed_at = Some(Instant::now());
        eprintln!(
            "[{}] android helper ({}): {why}; using adb",
            crate::clock::now_utc(),
            self.serial
        );
        self.helper_error = Some(why);
    }

    /// Put the helper down (it is broken, or in uiautomator's way).
    fn drop_helper(&mut self, why: String) {
        if let Some(h) = self.helper.take() {
            self.note_helper_failure(why);
            // Dropping it only ends the local `adb shell`; the process on the
            // phone would keep its UiAutomation connection and block
            // `uiautomator dump`. End it there too, off the request path.
            let adb = self.adb.clone();
            tokio::spawn(async move { h.stop(&adb).await });
        }
    }

    /// The helper, started when it is due. `None` sends the caller to adb.
    async fn helper(&mut self) -> Option<&mut Helper> {
        let dead = match self.helper.as_mut() {
            Some(h) => (!h.alive()).then(|| h.stderr_tail()),
            None => None,
        };
        if let Some(tail) = dead {
            self.drop_helper(format!("the helper exited ({tail})"));
        }
        let now = Instant::now();
        match helper::next(
            self.helper_on,
            self.helper.is_some(),
            self.helper_failures,
            self.helper_failed_at,
            now,
        ) {
            Next::Use => self.helper.as_mut(),
            Next::Adb => None,
            Next::Start => match Helper::start(&self.adb).await {
                Ok(h) => {
                    eprintln!(
                        "[{}] android helper ({}) up in {} ms: sdk {}, {:?}",
                        crate::clock::now_utc(),
                        self.serial,
                        now.elapsed().as_millis(),
                        h.hello.sdk,
                        h.hello.caps
                    );
                    self.helper = Some(h);
                    self.helper.as_mut()
                }
                Err(e) => {
                    self.note_helper_failure(format!("the helper did not start: {e}"));
                    None
                }
            },
        }
    }

    /// What the helper can do now (starting it if due); nothing when it is
    /// off or down.
    async fn helper_caps(&mut self) -> Caps {
        self.helper()
            .await
            .map(|h| h.hello.caps.clone())
            .unwrap_or_default()
    }

    /// One helper call, when the helper is up and `need` says it can. `None`
    /// sends the caller to adb.
    async fn helper_call(
        &mut self,
        need: fn(&Caps) -> bool,
        op: &str,
        args: Value,
        secs: u64,
    ) -> Option<Result<Value, CallError>> {
        let h = self.helper().await?;
        if !need(&h.hello.caps) {
            return None;
        }
        let r = h.call(op, args, secs).await;
        match &r {
            Ok(_) => {
                self.helper_failures = 0;
                self.used_helper = true;
            }
            Err(e) if e.broke_helper() => {
                self.drop_helper(format!("`{op}` failed: {}", e.message()))
            }
            Err(_) => self.used_helper = true,
        }
        Some(r)
    }

    /// Start the helper if it is due, for `doctor`.
    pub async fn helper_caps_now(&mut self) -> Caps {
        self.helper_caps().await
    }

    /// The helper's state, for `status` and `doctor`.
    pub fn helper_status(&mut self) -> Value {
        let running = self.helper.as_mut().is_some_and(|h| h.alive());
        let state = if !self.helper_on {
            "off"
        } else if running {
            "running"
        } else if self.helper_failed_at.is_some() {
            "failed"
        } else {
            "not_started"
        };
        let mut v = json!({ "enabled": self.helper_on, "state": state, "jar_sha256": helper::jar_sha256_recorded() });
        if let Some(h) = self.helper.as_ref().filter(|_| running) {
            let c = &h.hello.caps;
            v["version"] = json!(h.hello.version);
            v["sdk"] = json!(h.hello.sdk);
            v["pid"] = json!(h.hello.pid);
            v["uptime_s"] = json!(h.started.elapsed().as_secs());
            v["caps"] = json!({"snapshot": c.snapshot, "inject": c.inject, "clipboard": c.clipboard, "labels": c.labels});
            if !h.hello.errors.is_empty() {
                v["unavailable"] = Value::Object(
                    h.hello
                        .errors
                        .iter()
                        .map(|(k, e)| (k.clone(), json!(e)))
                        .collect(),
                );
            }
        }
        if let Some(e) = &self.helper_error {
            v["last_error"] = json!(e);
            v["failures"] = json!(self.helper_failures);
        }
        v
    }

    /// Stop the helper (the daemon is going away).
    pub async fn shutdown(&mut self) {
        if let Some(h) = self.helper.take() {
            h.stop(&self.adb).await;
        }
    }

    /// Send one input: through the helper when it is up, else as `input`
    /// commands. Says which. Falls back to adb only when the helper took
    /// none of it.
    async fn input(&mut self, input: Input) -> Res<&'static str> {
        let (op, args) = input.helper_request();
        if let Some(r) = self.helper_call(|c| c.inject, op, args, INPUT_SECS).await {
            self.invalidate();
            match r {
                Ok(_) => return Ok("helper"),
                Err(e) if e.maybe_sent() => return Err(helper_input_fail(&e)),
                Err(_) => {}
            }
        }
        let sdk = self.sdk().await;
        let cmd = input.adb_command(sdk)?;
        self.used_adb = true;
        self.send(&cmd).await?;
        Ok("adb")
    }

    pub fn adb(&self) -> &Adb {
        &self.adb
    }

    pub fn connection(&self) -> &'static str {
        if self.serial.contains(':') || self.serial.contains("._adb-tls-connect.") {
            "wifi"
        } else if self.serial.starts_with("emulator-") {
            "emulator"
        } else {
            "usb"
        }
    }

    fn invalidate(&mut self) {
        self.stale_before = Some(Instant::now());
    }

    pub async fn sdk(&mut self) -> u32 {
        if let Some(s) = self.sdk {
            return s;
        }
        let sdk = self
            .adb
            .getprop("ro.build.version.sdk")
            .await
            .and_then(|v| v.parse().ok());
        if let Some(s) = sdk {
            self.sdk = Some(s);
        }
        sdk.unwrap_or(0)
    }

    pub async fn geometry(&mut self) -> Res<Geometry> {
        if let Some(g) = self.geometry {
            return Ok(g);
        }
        let out = self
            .adb
            .shell("wm size; wm density", 10)
            .await
            .map_err(|e| e.not_sent("geometry_failed"))?;
        let px = uia::parse_wm_size(&out);
        let density = uia::parse_wm_density(&out);
        let (Some(px), Some(density)) = (px, density.filter(|d| *d > 0)) else {
            return Err(Fail::not_sent(
                "geometry_failed",
                format!("could not read the screen size from `wm`: {}", out.trim()),
            ));
        };
        let g = Geometry {
            px,
            scale: density as f64 / 160.0,
        };
        self.geometry = Some(g);
        Ok(g)
    }

    async fn pid_of(&mut self, pkg: &str) -> i64 {
        if let Some(p) = self.pids.get(pkg) {
            return *p;
        }
        let pid = match self
            .helper_call(|_| true, "pid", json!({"package": pkg}), 8)
            .await
        {
            Some(Ok(v)) => v["pid"].as_i64(),
            _ => match self.adb.shell(&format!("pidof {}", sh_quote(pkg)), 8).await {
                Ok(out) => parse_pid(&out),
                Err(_) => None,
            },
        };
        let pid = pid.unwrap_or_else(|| pseudo_pid(pkg));
        self.pids.insert(pkg.to_string(), pid);
        pid
    }

    /// One `uiautomator dump`, straight to stdout; through a file on the
    /// phone when that gives no XML.
    async fn dump_xml(&self) -> Res<String> {
        let mut last = String::new();
        for attempt in 0..2 {
            match self
                .adb
                .exec_out("uiautomator dump /dev/tty", DUMP_SECS)
                .await
            {
                Ok(bytes) => {
                    let out = String::from_utf8_lossy(&bytes);
                    if let Some(xml) = uia::extract_xml(&out) {
                        return Ok(xml.to_string());
                    }
                    last = out.trim().chars().take(300).collect();
                }
                Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => {
                    return Err(e.not_sent("ui_dump_failed"));
                }
                Err(e) => last = e.message(),
            }
            let cmd = format!(
                "uiautomator dump {DUMP_FILE} >/dev/null 2>&1 && cat {DUMP_FILE}; r=$?; rm -f {DUMP_FILE}; exit $r"
            );
            match self.adb.exec_out(&cmd, DUMP_SECS).await {
                Ok(bytes) => {
                    let out = String::from_utf8_lossy(&bytes);
                    if let Some(xml) = uia::extract_xml(&out) {
                        return Ok(xml.to_string());
                    }
                    if !out.trim().is_empty() {
                        last = out.trim().chars().take(300).collect();
                    }
                }
                Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => {
                    return Err(e.not_sent("ui_dump_failed"));
                }
                Err(e) => last = e.message(),
            }
            if attempt == 0 {
                // A helper left on the phone (a daemon that died, a helper
                // dropped while stuck) holds the one UiAutomation connection
                // and makes uiautomator fail ("Killed"). Ours is not up when
                // this path runs, so any helper there is a leftover.
                if self.helper.is_none() {
                    helper::kill_leftovers(&self.adb).await;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        Err(Fail::not_sent("ui_dump_failed", format!("uiautomator gave no screen: {last}")).hint(t(
            "화면이 계속 움직이면(애니메이션·영상) uiautomator가 멈춘 화면을 기다리다 실패합니다. 화면이 켜져 있고 잠금이 풀려 있는지 확인하고 `phone doctor`를 실행하세요.",
            "uiautomator fails while the screen keeps moving (animations, video). Check that the screen is on and unlocked, and run `phone doctor`.",
        )))
    }

    /// The screen's tree: from the helper when it is up, else a dump.
    async fn dump_root(&mut self) -> Res<XmlNode> {
        let (w, h) = self.geometry.map(|g| g.px).unwrap_or((0, 0));
        let args = json!({"idle_ms": HELPER_IDLE_MS, "timeout_ms": HELPER_IDLE_TIMEOUT_MS, "width": w, "height": h});
        if let Some(answer) = self
            .helper_call(|c| c.snapshot, "snapshot", args, DUMP_SECS)
            .await
        {
            match answer
                .map_err(|e| e.message())
                .and_then(|v| helper::snapshot_to_tree(&v))
            {
                Ok(tree) => return Ok(tree),
                // The helper's UiAutomation connection keeps `uiautomator`
                // from registering its own, so it goes before the dump.
                Err(e) => self.drop_helper(format!("the helper could not read the screen: {e}")),
            }
        }
        self.used_adb = true;
        let xml = self.dump_xml().await?;
        tokio::task::spawn_blocking(move || uia::parse_xml(&xml))
            .await
            .map_err(|e| Fail::not_sent("ui_dump_failed", e.to_string()))?
            .map_err(|e| {
                Fail::not_sent(
                    "ui_dump_unparsed",
                    format!("the uiautomator dump was not understood: {e}"),
                )
            })
    }

    async fn read_now(&mut self) -> Res<u64> {
        let geo = self.geometry().await?;
        let root = self.dump_root().await?;
        let pkg = uia::dump_package(&root).unwrap_or_default();
        let pid = if pkg.is_empty() {
            0
        } else {
            self.pid_of(&pkg).await
        };
        let app = AppInfo {
            pid,
            name: (!pkg.is_empty()).then(|| pkg.clone()),
            bundle_id: (!pkg.is_empty()).then_some(pkg),
        };
        let _ = geo;
        self.seq += 1;
        self.latest = Some(Reading {
            seq: self.seq,
            at: Instant::now(),
            root: Arc::new(root),
            app,
        });
        Ok(self.seq)
    }

    fn build(&self, opts: BuildOptions) -> Option<(u64, Screen)> {
        let r = self.latest.as_ref()?;
        let geo = self.geometry?;
        Some((r.seq, uia::build_screen(&r.root, geo, &r.app, opts)))
    }

    fn latest_age(&self) -> Option<Duration> {
        let r = self.latest.as_ref()?;
        if self.stale_before.is_some_and(|s| r.at < s) {
            return None;
        }
        Some(r.at.elapsed())
    }

    async fn read_screen(&mut self, fresh: Fresh, opts: BuildOptions) -> Res<(u64, Screen)> {
        if let Fresh::MaxAge(max) = fresh
            && self.latest_age().is_some_and(|age| age <= max)
            && let Some(s) = self.build(opts)
        {
            return Ok(s);
        }
        self.read_now().await?;
        self.build(opts)
            .ok_or_else(|| Fail::not_sent("ui_dump_failed", "no reading"))
    }

    /// Run input commands in one shell. Fails `not_sent` when adb did not
    /// reach the phone or `input` refused the event, `unknown` otherwise.
    async fn send(&mut self, cmd: &str) -> Res<()> {
        let result = self.adb.shell(cmd, INPUT_SECS).await;
        self.invalidate();
        match result {
            Ok(out) => match input_refused(&out) {
                Some(line) => Err(Fail::not_sent("input_rejected", line).hint(t(
                    "폰이 adb 입력을 거부했습니다. 일부 제조사(Xiaomi 등)는 개발자 옵션의 'USB 디버깅(보안 설정)'을 켜야 합니다.",
                    "The phone refused adb input. Some makers (Xiaomi and others) need Developer options → USB debugging (Security settings) turned on.",
                ))),
                None => Ok(()),
            },
            Err(e) => Err(e.input()),
        }
    }

    fn px(&self, x: f64, y: f64) -> (i64, i64) {
        let g = self.geometry.expect("geometry is read before any input");
        uia::to_pixels(x, y, g.scale, g.px)
    }

    async fn size(&mut self) -> Res<(f64, f64)> {
        Ok(self.geometry().await?.dp())
    }

    /// Read after an input that began at `since`.
    async fn observe(&mut self, delay: Duration) -> Option<(u64, Screen)> {
        tokio::time::sleep(delay).await;
        self.read_screen(Fresh::Force, BuildOptions::default())
            .await
            .ok()
    }

    /// Read after a tap until the screen differs from `before`, for up to
    /// `CHANGE_WAIT`. Each dump already waits for the UI to go idle.
    async fn observe_settled(
        &mut self,
        since: Instant,
        before: &[ElementSignature],
    ) -> Option<(u64, Screen)> {
        let mut latest = self.observe(AFTER_TAP).await?;
        while latest.1.signature() == before && since.elapsed() < CHANGE_WAIT {
            if self.helper.is_some() {
                tokio::time::sleep(POLL_GAP).await;
            }
            match self
                .read_screen(Fresh::Force, BuildOptions::default())
                .await
            {
                Ok(next) => latest = next,
                Err(_) => break,
            }
        }
        Some(latest)
    }

    async fn locate(
        &mut self,
        targets: &[Target],
        fresh: Fresh,
    ) -> Res<(Screen, Vec<(f64, f64, Option<Element>)>)> {
        let (_, screen) = self.read_screen(fresh, BuildOptions::default()).await?;
        let mut points = Vec::with_capacity(targets.len());
        for target in targets {
            points.push(match target {
                Target::Selector(sel) => {
                    let el = resolve_one(sel, &screen)?;
                    let (x, y) = tap_point(el, &screen)?;
                    (x, y, Some(el.clone()))
                }
                Target::Point(x, y) => {
                    if *x < 0.0 || *y < 0.0 || *x > screen.width || *y > screen.height {
                        return Err(Fail::invalid(format!(
                            "point {x},{y} is outside the {}x{} screen",
                            screen.width, screen.height
                        )));
                    }
                    (*x, *y, screen.element_at(*x, *y).cloned())
                }
            });
        }
        Ok((screen, points))
    }

    /// One drag between points in dp, as an [`Input`].
    fn drag_input(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Input {
        let ms = duration.as_millis().max(50) as u64;
        let hold_ms = if motion == Motion::Flick {
            0
        } else {
            DRAG_HOLD_MS
        };
        Input::Swipe {
            from: self.px(from.0, from.1),
            to: self.px(to.0, to.1),
            ms,
            hold_ms,
        }
    }

    fn focused_field(screen: &Screen) -> Option<&Element> {
        let mut focused = screen.elements.iter().filter(|e| e.focused);
        let first = focused.next()?;
        focused.next().is_none().then_some(first)
    }

    async fn installed(&mut self) -> Res<(Vec<(String, String)>, Vec<String>)> {
        let launchable = self
            .adb
            .shell("cmd package query-activities --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER", 20)
            .await
            .map(|o| parse_components(&o))
            .unwrap_or_default();
        let third = self
            .adb
            .shell("pm list packages -3", 20)
            .await
            .map_err(|e| e.not_sent("apps_failed"))
            .map(|o| parse_packages(&o))?;
        Ok((launchable, third))
    }

    async fn resolve_package(&mut self, app: &str) -> Res<String> {
        if app.contains('.') && !app.contains(' ') {
            return Ok(app.to_string());
        }
        let want = actions::nfc(app).to_lowercase();
        let (launchable, third) = self.installed().await?;
        let mut all: Vec<String> = launchable
            .iter()
            .map(|(p, _)| p.clone())
            .chain(third)
            .collect();
        all.sort();
        all.dedup();
        // The name the phone shows ("설정"), when the helper can read names;
        // else the package's own last word: "settings" finds com.android.settings.
        let labels = self.labels_for(&all, &launchable).await;
        let by_label: Vec<&String> = all
            .iter()
            .filter(|p| {
                labels
                    .get(*p)
                    .is_some_and(|l| actions::nfc(l).to_lowercase() == want)
            })
            .collect();
        let by_word: Vec<&String> = all
            .iter()
            .filter(|p| p.to_lowercase().rsplit('.').next() == Some(want.as_str()))
            .collect();
        let hits: Vec<&String> = if by_label.len() == 1 {
            by_label
        } else {
            let mut h: Vec<&String> = by_label.into_iter().chain(by_word).collect();
            h.sort();
            h.dedup();
            h
        };
        match hits.len() {
            1 => Ok(hits[0].clone()),
            0 => Err(Fail::not_sent("app_not_found", format!("no installed app is named \"{app}\" and no package ends in it")).hint(t(
                "앱 이름이나 패키지 이름으로 실행하세요(예: 설정, com.android.settings). `phone apps`로 확인할 수 있습니다.",
                "Launch by the app's name or its package name (Settings, com.android.settings). See them with `phone apps`.",
            ))),
            n => Err(Fail::not_sent("ambiguous_app", format!("{n} packages end in \"{app}\""))
                .with("candidates", Value::Array(hits.iter().map(|p| json!(p)).collect()))),
        }
    }

    /// App names for `packages`, from the helper (cached for the daemon's
    /// life): the launcher activity's label for the ones in `launchable`
    /// (what the home screen shows), else the app's. Empty when the helper is
    /// not up.
    async fn labels_for(
        &mut self,
        packages: &[String],
        launchable: &[(String, String)],
    ) -> HashMap<String, String> {
        let missing: Vec<&String> = packages
            .iter()
            .filter(|p| !self.labels.contains_key(*p))
            .collect();
        if missing.len() < packages.len() {
            self.used_helper = true;
        }
        if !missing.is_empty() {
            let activities: serde_json::Map<String, Value> = launchable
                .iter()
                .filter(|(p, _)| missing.contains(&p))
                .map(|(p, a)| (p.clone(), json!(a)))
                .collect();
            let args = json!({ "packages": missing, "activities": activities });
            match self.helper_call(|c| c.labels, "labels", args, 60).await {
                Some(Ok(v)) => {
                    for p in missing.iter().map(|p| (*p).clone()).collect::<Vec<_>>() {
                        let label = v["labels"][&p].as_str().map(str::to_string);
                        self.labels.insert(p, label);
                    }
                }
                _ => self.used_adb = true,
            }
        }
        packages
            .iter()
            .filter_map(|p| Some((p.clone(), self.labels.get(p)?.clone()?)))
            .collect()
    }

    fn not_supported(what: &str) -> Fail {
        Fail::not_sent(
            "unsupported",
            format!("{what} is not supported on Android yet"),
        )
    }

    pub async fn cmd_dnd(&mut self, _args: &Value) -> Res {
        Err(Self::not_supported("`phone dnd`").hint(t(
            "Android에서는 폰의 빠른 설정에서 방해 금지를 직접 켜세요.",
            "On Android turn Do Not Disturb on from the phone's Quick Settings.",
        )))
    }
}

impl Phone for AndroidSession {
    fn config(&self) -> &Config {
        &self.config
    }

    fn refresh_connection(&mut self) {}

    fn reset(&mut self) {
        self.latest = None;
        self.geometry = None;
        // A call was cut off: the helper may be mid-answer, so it goes.
        self.helper = None;
    }

    async fn close_input(&mut self) {}

    async fn screen(
        &mut self,
        fresh: Fresh,
        _full: bool,
        opts: BuildOptions,
    ) -> Res<(u64, Screen)> {
        self.read_screen(fresh, opts).await
    }

    async fn cmd_read(&mut self, args: ReadArgs) -> Res {
        let fresh = match args.max_age_ms {
            Some(ms) => Fresh::MaxAge(Duration::from_millis(ms)),
            None => Fresh::Force,
        };
        let (seq, screen) = self
            .read_screen(
                fresh,
                BuildOptions {
                    include_offscreen: args.all,
                    raw: args.raw,
                },
            )
            .await?;
        // Only the front app is ever in a dump, so `--app` changes nothing.
        let has_ids = screen.elements.iter().any(|e| e.identifier.is_some());
        Ok(fields! {
            "screen": {"width": screen.width, "height": screen.height},
            "apps": screen.apps,
            "count": screen.elements.len(),
            "pass": if args.full || args.raw || has_ids { "full" } else { "initial" },
            "seq": seq,
            "elements": screen.elements,
        })
    }

    async fn cmd_find(&mut self, args: FindArgs) -> Res {
        let sel = Selector::from_value(&args.selector).map_err(|e| Fail::invalid(e.to_string()))?;
        let (_, screen) = self
            .read_screen(
                Fresh::Force,
                BuildOptions {
                    include_offscreen: args.all,
                    raw: false,
                },
            )
            .await?;
        let matches = sel.find(&screen);
        Ok(fields! {
            "selector": sel.describe(),
            "count": matches.len(),
            "elements": matches.iter().map(|e| (*e).clone()).collect::<Vec<_>>(),
        })
    }

    async fn cmd_tap(&mut self, args: TapArgs) -> Res {
        if args.double && args.long_ms.is_some() {
            return Err(Fail::invalid("--double and --long cannot be combined"));
        }
        if let Some(ms) = args.long_ms
            && !(1..=60_000).contains(&ms)
        {
            return Err(Fail::invalid("--long must be 1..60000 ms"));
        }
        let target = parse_target(&args.target)?;
        let (screen, mut points) = self
            .locate(std::slice::from_ref(&target), Fresh::MaxAge(reuse()))
            .await?;
        let (x, y, element) = points.remove(0);
        let (px, py) = self.px(x, y);
        let input = Input::Tap {
            x: px,
            y: py,
            hold_ms: args.long_ms.unwrap_or(0),
            count: if args.double { 2 } else { 1 },
        };
        let before = screen.signature();
        let began = Instant::now();
        self.input(input).await?;
        let after = if args.no_wait {
            self.observe(AFTER_TAP).await
        } else {
            self.observe_settled(began, &before).await
        };
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after
            .as_ref()
            .and_then(|(_, s)| actions::app_changed(&screen, s));
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "x": (x * 10.0).round() / 10.0,
            "y": (y * 10.0).round() / 10.0,
        };
        if let Some(e) = element.as_ref().map(brief) {
            out.insert("element".into(), e);
        }
        if changed.is_none() {
            out.insert(
                "note".into(),
                json!("the tap was sent but no reading came back to compare"),
            );
        }
        if let Some(change) = app_change {
            out.insert("app_changed".into(), json!(true));
            out.insert("app".into(), change);
        }
        if !args.no_wait
            && let Some((seq, screen)) = after
        {
            out.insert(
                "screen".into(),
                json!({"width": screen.width, "height": screen.height}),
            );
            out.insert("apps".into(), json!(screen.apps));
            out.insert("count".into(), json!(screen.elements.len()));
            out.insert("seq".into(), json!(seq));
            out.insert("elements".into(), json!(screen.elements));
        }
        Ok(out)
    }

    async fn tap_sequence(&mut self, targets: &[Value]) -> Res {
        if targets.is_empty() {
            return Err(Fail::invalid("the tap list is empty"));
        }
        let parsed: Vec<Target> = targets.iter().map(parse_target).collect::<Res<_>>()?;
        let (screen, points) = self.locate(&parsed, Fresh::MaxAge(reuse())).await?;
        let before = screen.signature();
        let input = Input::Taps(points.iter().map(|(x, y, _)| self.px(*x, *y)).collect());
        let tapped: Vec<Value> = points
            .iter()
            .map(|(_, _, el)| el.as_ref().map(brief).unwrap_or(Value::Null))
            .collect();
        self.input(input).await.map_err(|f| {
            f.hint(t(
                "입력이 일부 나갔을 수 있습니다. 다시 보내지 말고 `phone read`로 화면을 확인하세요.",
                "Part of the input may have gone out. Do not resend; check the screen with `phone read`.",
            ))
        })?;
        let after = self.observe(AFTER_TAP).await;
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after
            .as_ref()
            .and_then(|(_, s)| actions::app_changed(&screen, s));
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "taps": tapped.len(),
            "elements": tapped,
        };
        if let Some(change) = app_change {
            out.insert("app_changed".into(), json!(true));
            out.insert("app".into(), change);
        }
        Ok(out)
    }

    async fn drag_points(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Res<(bool, Option<Screen>)> {
        let (_, screen) = self
            .read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
            .await?;
        for (x, y) in [from, to] {
            if x < 0.0 || y < 0.0 || x > screen.width || y > screen.height {
                return Err(Fail::invalid(format!(
                    "point {x},{y} is outside the {}x{} screen",
                    screen.width, screen.height
                )));
            }
        }
        let before = screen.signature();
        let input = self.drag_input(from, to, duration, motion);
        self.input(input).await?;
        let after = self.observe(AFTER_DRAG).await.map(|(_, s)| s);
        let changed = after.as_ref().is_some_and(|s| s.signature() != before);
        Ok((changed, after))
    }

    async fn cmd_swipe(&mut self, args: SwipeArgs) -> Res {
        let (w, h) = self.size().await?;
        let (from, to) = match (&args.direction, args.from, args.to) {
            (Some(dir), None, None) => {
                let (cx, cy) = (w / 2.0, h / 2.0);
                match dir.as_str() {
                    "up" => ((cx, h * 0.70), (cx, h * 0.30)),
                    "down" => ((cx, h * 0.30), (cx, h * 0.70)),
                    "left" => ((w * 0.80, cy), (w * 0.20, cy)),
                    "right" => ((w * 0.20, cy), (w * 0.80, cy)),
                    other => {
                        return Err(Fail::invalid(format!(
                            "unknown direction `{other}` (up, down, left, right)"
                        )));
                    }
                }
            }
            (None, Some(f), Some(t)) => ((f[0], f[1]), (t[0], t[1])),
            _ => return Err(Fail::invalid("give a direction, or both from and to")),
        };
        let duration = Duration::from_millis(args.duration_ms.unwrap_or(250).clamp(50, 10_000));
        let motion = if args.drag {
            Motion::Place
        } else {
            Motion::Flick
        };
        let (changed, after) = self.drag_points(from, to, duration, motion).await?;
        Ok(fields! {
            "sent": true,
            "verified": after.is_some(),
            "screen_changed": changed,
            "from": [from.0, from.1],
            "to": [to.0, to.1],
        })
    }

    async fn cmd_scroll(&mut self, args: ScrollArgs) -> Res {
        if args.direction != "up" && args.direction != "down" {
            return Err(Fail::invalid("direction must be up or down"));
        }
        let Some(until) = &args.until else {
            let (changed, after) = scroll_step(self, &args.direction).await?;
            return Ok(fields! {
                "sent": true,
                "verified": after.is_some(),
                "screen_changed": changed,
                "scrolls": 1,
            });
        };
        let sel = Selector::from_value(until).map_err(|e| Fail::invalid(e.to_string()))?;
        let max = args.max.unwrap_or(10).clamp(1, 100);
        let mut scrolls = 0u32;
        let (_, mut screen) = self
            .read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
            .await?;
        loop {
            let matches = dedupe_targets(sel.find(&screen));
            if let Some(el) = matches.iter().find(|e| tap_point(e, &screen).is_ok()) {
                if matches.len() > 1 {
                    return Err(Fail::not_sent(
                        "ambiguous_selector",
                        format!("{} elements match {}", matches.len(), sel.describe()),
                    )
                    .with(
                        "candidates",
                        Value::Array(matches.iter().map(|e| brief(e)).collect()),
                    )
                    .with("scrolls", scrolls));
                }
                return Ok(fields! {
                    "found": true,
                    "scrolls": scrolls,
                    "screen_changed": scrolls > 0,
                    "element": brief(el),
                });
            }
            if scrolls >= max {
                return Err(Fail::new(
                    Outcome::Done,
                    "not_found",
                    format!("{} not found after {scrolls} scrolls", sel.describe()),
                )
                .with("found", false)
                .with("scrolls", scrolls)
                .with("screen_summary", screen.summary(12)));
            }
            let (mut changed, mut after) = scroll_step(self, &args.direction).await?;
            scrolls += 1;
            if !changed {
                (changed, after) = scroll_step(self, &args.direction).await?;
                scrolls += 1;
            }
            if !changed {
                return Err(Fail::new(
                    Outcome::Done,
                    "not_found",
                    format!(
                        "{} not found; the list stopped moving after {scrolls} scrolls",
                        sel.describe()
                    ),
                )
                .with("found", false)
                .with("scrolls", scrolls)
                .with("screen_summary", screen.summary(12)));
            }
            screen = match after {
                Some(s) => s,
                None => {
                    self.read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
                        .await?
                        .1
                }
            };
        }
    }

    async fn cmd_type(&mut self, args: TypeArgs) -> Res {
        if args.text.is_empty() && !args.clear {
            return Err(Fail::invalid("nothing to type"));
        }
        if args.text.len() > 64 * 1024 {
            return Err(Fail::invalid("text is larger than 64 KiB"));
        }
        // Printable ASCII goes as key presses (the helper's, or `input
        // text`); anything else only the helper can paste.
        let caps = self.helper_caps().await;
        let paste = !args.text.is_empty() && !keys::printable_ascii(&args.text);
        let typeable = keys::typeable(&args.text);
        if (paste && !(caps.clipboard && caps.inject))
            || (!paste && !caps.inject && typeable.is_err())
        {
            let why = typeable
                .err()
                .unwrap_or_else(|| "the helper cannot paste on this phone".into());
            return Err(Fail::not_sent("unsupported_text", why)
                .hint(t(
                    "한글·이모지 입력에는 phone-cli 헬퍼가 필요한데 지금은 쓸 수 없습니다(`phone status`의 helper 참고, `[android] helper = true`, `PHONE_ANDROID_HELPER`가 0이 아닌지 확인). adb `input text`는 영문·숫자·기호(ASCII)만 됩니다. 줄바꿈은 `phone key return`으로 따로 보내세요.",
                    "Korean and emoji need phone-cli's helper, which is not available now (see `helper` in `phone status`; check `[android] helper = true` and that PHONE_ANDROID_HELPER is not 0). adb `input text` types printable ASCII only. Send line breaks separately with `phone key return`.",
                ))
                .with("helper", self.helper_status()));
        }
        let (_, screen) = self
            .read_screen(Fresh::Force, BuildOptions::default())
            .await?;
        let Some(field) = Self::focused_field(&screen).cloned() else {
            return Err(Fail::not_sent(
                "no_focused_field",
                "no text field has the keyboard right now",
            )
            .hint(t(
                "먼저 입력할 칸을 `phone tap`으로 눌러 키보드를 띄우세요.",
                "Tap the field first with `phone tap` so the keyboard comes up.",
            ))
            .with("screen_summary", screen.summary(12)));
        };
        let mut cleared = false;
        let mut method = "adb_input_text";
        if args.clear {
            // To the end, then one Delete per character the field shows
            // (more than needed when that is a placeholder; harmless).
            let n = field
                .value
                .as_deref()
                .map(|v| v.chars().count())
                .unwrap_or(0)
                .clamp(1, 2000);
            let mut codes = vec![123];
            codes.extend(std::iter::repeat_n(67, n));
            let backend = self
                .input(Input::Keys(codes))
                .await
                .map_err(|f| f.with("clipboard_restored", true))?;
            cleared = true;
            if backend == "helper" {
                method = "helper_keys";
            }
        }
        // Once the clear went out, a failure after it has changed the field.
        let after_clear = |f: Fail| -> Fail {
            if cleared && f.outcome == Outcome::NotSent {
                Fail::unknown(
                    "input_failed",
                    format!("the field was cleared, then: {}", f.message),
                )
            } else {
                f
            }
        };
        let mut restored = true;
        if paste {
            let req = json!({ "text": args.text, "wait_ms": PASTE_WAIT_MS });
            match self
                .helper_call(|c| c.clipboard && c.inject, "paste", req, INPUT_SECS)
                .await
            {
                Some(Ok(v)) => {
                    restored = v["clipboard_restored"].as_bool().unwrap_or(false);
                    method = "helper_paste";
                    self.invalidate();
                }
                Some(Err(e)) => {
                    self.invalidate();
                    let restored = match &e {
                        CallError::Refused { extra, sent, .. } => {
                            extra["clipboard_restored"].as_bool().unwrap_or(!sent)
                        }
                        CallError::NotSent(_) => true,
                        CallError::Lost(_) => false,
                    };
                    let fail = if e.maybe_sent() {
                        helper_input_fail(&e)
                    } else {
                        after_clear(Fail::not_sent(
                            "input_failed",
                            format!("the helper could not paste: {}", e.message()),
                        ))
                    };
                    return Err(fail.with("clipboard_restored", restored));
                }
                None => {
                    return Err(after_clear(Fail::not_sent(
                        "unsupported_text",
                        "the helper went away before the paste",
                    ))
                    .with("clipboard_restored", true));
                }
            }
        } else if !args.text.is_empty() {
            let backend = self
                .input(Input::Text(args.text.clone()))
                .await
                .map_err(|f| after_clear(f).with("clipboard_restored", true))?;
            method = if backend == "helper" {
                "helper_keys"
            } else {
                "adb_input_text"
            };
        }
        tokio::time::sleep(AFTER_PASTE).await;
        let after = self
            .read_screen(Fresh::Force, BuildOptions::default())
            .await
            .ok()
            .map(|(_, s)| s);
        // The original field only; focus moving elsewhere is not its value.
        let value = after.as_ref().and_then(|s| {
            s.by_id(&field.id)
                .filter(|e| e.role == field.role)
                .or_else(|| {
                    Self::focused_field(s)
                        .filter(|e| e.identifier.is_some() && e.identifier == field.identifier)
                })
                .and_then(|e| e.value.clone())
        });
        let observation = TypeObservation::new(&args, field.value.clone(), value);
        let mut submitted = Some(false);
        let mut submit_error = None;
        let mut final_screen = after;
        if args.submit && observation.can_submit() {
            match self.input(Input::Keys(vec![66])).await {
                Ok(_) => {
                    submitted = Some(true);
                    final_screen = self.observe(AFTER_TAP).await.map(|(_, s)| s);
                }
                Err(f) if f.outcome == Outcome::NotSent => {
                    submit_error = Some(format!("Return not sent: {}", f.message))
                }
                Err(f) => {
                    submitted = None;
                    submit_error = Some(f.message);
                }
            }
        }
        let screen_changed = final_screen
            .as_ref()
            .map(|s| s.signature() != screen.signature());
        let mut result = observation.result(
            &args,
            &field,
            restored,
            submitted,
            screen_changed,
            submit_error,
        );
        let out = match &mut result {
            Ok(out) => out,
            Err(fail) => &mut fail.extra,
        };
        out.insert("input_method".into(), json!(method));
        result
    }

    async fn cmd_key(&mut self, args: KeyArgs) -> Res {
        let (code, modifiers) = keys::parse_combo(&args.key).map_err(Fail::invalid)?;
        let before = self
            .read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
            .await
            .ok()
            .map(|(_, s)| s.signature());
        self.input(Input::Combo { code, modifiers }).await?;
        let changed = match &before {
            Some(sig) => self
                .observe(AFTER_TAP)
                .await
                .map(|(_, s)| s.signature() != *sig),
            None => None,
        };
        Ok(fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "key": args.key,
        })
    }

    async fn cmd_button(&mut self, args: ButtonArgs) -> Res {
        let code = keys::button(&args.button).map_err(Fail::invalid)?;
        let before = self
            .read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
            .await
            .ok()
            .map(|(_, s)| s.signature());
        self.input(Input::Keys(vec![code])).await?;
        let changed = match &before {
            Some(sig) => self
                .observe(AFTER_LAUNCH)
                .await
                .map(|(_, s)| s.signature() != *sig),
            None => None,
        };
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "button": args.button,
        };
        if before.is_none() {
            out.insert(
                "note".into(),
                json!("the screen could not be read before the button; not verified"),
            );
        }
        Ok(out)
    }

    async fn cmd_apps(&mut self) -> Res {
        let (launchable, third) = self.installed().await?;
        let mut pkgs: Vec<String> = launchable
            .iter()
            .map(|(p, _)| p.clone())
            .chain(third.iter().cloned())
            .collect();
        pkgs.sort();
        pkgs.dedup();
        let labels = self.labels_for(&pkgs, &launchable).await;
        let list: Vec<Value> = pkgs
            .iter()
            .map(|p| {
                let user = third.contains(p);
                json!({
                    "bundle_id": p,
                    "name": labels.get(p),
                    "version": Value::Null,
                    "first_party": !user,
                    "removable": user,
                    "launchable": launchable.iter().any(|(lp, _)| lp == p),
                })
            })
            .collect();
        Ok(fields! { "count": list.len(), "apps": list })
    }

    async fn cmd_launch(&mut self, args: LaunchArgs) -> Res {
        let pkg = self.resolve_package(&args.app).await?;
        let resolved = self
            .adb
            .shell(
                &format!(
                    "cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {}",
                    sh_quote(&pkg)
                ),
                15,
            )
            .await
            .map_err(|e| e.not_sent("launch_failed"))?;
        let component = parse_components(&resolved)
            .into_iter()
            .rev()
            .find(|(p, _)| *p == pkg);
        let Some((_, activity)) = component else {
            return Err(Fail::not_sent(
                "app_not_found",
                format!("{pkg} has no launcher activity (or is not installed)"),
            )
            .hint(t(
                "`phone apps`로 패키지 이름을 확인하세요.",
                "Check the package name with `phone apps`.",
            )));
        };
        self.pids.remove(&pkg);
        let began = Instant::now();
        let mut cmd = String::new();
        if args.restart {
            cmd.push_str(&format!("am force-stop {}; ", sh_quote(&pkg)));
        }
        cmd.push_str(&format!(
            "am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n {}",
            sh_quote(&format!("{pkg}/{activity}"))
        ));
        let out = self.adb.shell(&cmd, INPUT_SECS).await;
        self.invalidate();
        match out {
            Ok(o) if o.contains("Error:") || o.contains("Exception") => {
                let line = o
                    .lines()
                    .find(|l| l.contains("Error") || l.contains("Exception"))
                    .unwrap_or("")
                    .trim();
                return Err(Fail::not_sent("launch_failed", line.to_string()));
            }
            Ok(_) => {}
            Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => {
                return Err(e.not_sent("launch_failed"));
            }
            Err(e) => {
                return Err(Fail::unknown("launch_failed", e.message()).hint(t(
                    "실행 요청이 나갔을 수 있습니다. `phone read`로 화면을 확인하세요.",
                    "The launch may have gone out. Check the screen with `phone read`.",
                )));
            }
        }
        let mut foreground = false;
        let mut delay = AFTER_LAUNCH;
        for _ in 0..4 {
            if let Some((_, screen)) = self.observe(delay).await
                && screen
                    .apps
                    .iter()
                    .any(|a| a.bundle_id.as_deref() == Some(pkg.as_str()))
            {
                foreground = true;
                break;
            }
            if began.elapsed() > Duration::from_secs(15) {
                break;
            }
            delay = Duration::from_millis(300);
        }
        let pid = self.pid_of(&pkg).await;
        Ok(fields! {
            "sent": true,
            "verified": foreground,
            "foreground": foreground,
            "bundle_id": pkg,
            "pid": pid,
            "activity": activity,
        })
    }

    async fn cmd_screenshot(&mut self, args: ScreenshotArgs) -> Res {
        let png = self
            .adb
            .exec_out("screencap -p", 30)
            .await
            .map_err(|e| e.not_sent("screenshot_failed"))?;
        if !png.starts_with(b"\x89PNG") {
            let head: String = String::from_utf8_lossy(&png[..png.len().min(200)])
                .trim()
                .to_string();
            return Err(Fail::not_sent(
                "screenshot_failed",
                format!("screencap gave no PNG: {head}"),
            ));
        }
        let path = std::path::PathBuf::from(&args.path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", dir.display())))?;
        }
        std::fs::write(&path, &png)
            .map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", path.display())))?;
        Ok(fields! { "path": args.path, "bytes": png.len() })
    }

    /// A dump comes only once the UI is idle, so a fresh reading's element
    /// is where it rests; an older one is read again by the caller.
    async fn held_still(&mut self, sel: &Selector, screen: &Screen) -> bool {
        self.latest_age().is_some_and(|age| age <= reuse())
            && dedupe_targets(sel.find(screen)).len() == 1
    }

    fn reading_lost(&self) -> bool {
        false
    }

    fn latest_app_pids(&self) -> Option<Vec<i64>> {
        Some(vec![self.latest.as_ref()?.app.pid])
    }

    fn latest_screen(&self) -> Option<Screen> {
        self.build(BuildOptions::default()).map(|(_, s)| s)
    }
}

/// One scroll step: a 500 ms drag over the middle 36 % of the screen that
/// comes to rest before lifting. "down" shows what is further down.
async fn scroll_step(s: &mut AndroidSession, direction: &str) -> Res<(bool, Option<Screen>)> {
    let (w, h) = s.size().await?;
    let cx = w / 2.0;
    let (from, to) = match direction {
        "down" => ((cx, h * 0.68), (cx, h * 0.32)),
        "up" => ((cx, h * 0.32), (cx, h * 0.68)),
        other => {
            return Err(Fail::invalid(format!(
                "unknown direction `{other}` (up, down)"
            )));
        }
    };
    s.drag_points(from, to, Duration::from_millis(500), Motion::Place)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_components_and_packages_parse() {
        let resolve = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\ncom.android.settings/.Settings\n";
        assert_eq!(
            parse_components(resolve),
            vec![("com.android.settings".to_string(), ".Settings".to_string())]
        );
        let query = "2 activities found:\n  Activity #0:\n    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\n    com.a/com.a.Main\n  Activity #1:\n    com.b/.Start\n";
        assert_eq!(parse_components(query).len(), 2);
        assert_eq!(parse_components("No activity found\n"), vec![]);
        assert_eq!(
            parse_packages("package:com.a\npackage:com.b\n\n"),
            vec!["com.a", "com.b"]
        );
    }

    #[test]
    fn pids_and_refusals() {
        assert_eq!(parse_pid("18506\n"), Some(18506));
        assert_eq!(parse_pid("18506 18507\n"), Some(18506));
        assert_eq!(parse_pid(""), None);
        assert!(pseudo_pid("com.android.systemui") < 0);
        assert_eq!(pseudo_pid("x"), pseudo_pid("x"));
        assert!(input_refused("").is_none());
        assert!(
            input_refused("java.lang.SecurityException: Injecting to another application requires INJECT_EVENTS permission\n")
                .is_some()
        );
        assert!(input_refused("Error: Unknown command: fly\n").is_some());
    }

    #[test]
    fn inputs_become_the_input_commands_they_were() {
        let tap = |hold_ms, count| {
            Input::Tap {
                x: 540,
                y: 1200,
                hold_ms,
                count,
            }
            .adb_command(33)
            .unwrap()
        };
        assert_eq!(tap(0, 1), "input tap 540 1200");
        assert_eq!(tap(0, 2), "input tap 540 1200; input tap 540 1200");
        assert_eq!(tap(800, 1), "input swipe 540 1200 540 1200 800");
        assert_eq!(
            Input::Taps(vec![(1, 2), (3, 4)]).adb_command(33).unwrap(),
            "input tap 1 2; input tap 3 4"
        );
        let flick = Input::Swipe {
            from: (10, 900),
            to: (10, 300),
            ms: 250,
            hold_ms: 0,
        };
        assert_eq!(
            flick.adb_command(33).unwrap(),
            "input swipe 10 900 10 300 250"
        );
        let drag = Input::Swipe {
            from: (0, 1000),
            to: (0, 400),
            ms: 500,
            hold_ms: 250,
        }
        .adb_command(33)
        .unwrap();
        assert!(
            drag.starts_with("input motionevent DOWN 0 1000; input motionevent MOVE 0 925;"),
            "{drag}"
        );
        assert!(
            drag.ends_with(
                "; sleep 0.25; input motionevent MOVE 0 400; input motionevent UP 0 400"
            ),
            "{drag}"
        );
        // Before Android 10 there is no motionevent.
        let old = Input::Swipe {
            from: (0, 1000),
            to: (0, 400),
            ms: 500,
            hold_ms: 250,
        }
        .adb_command(28)
        .unwrap();
        assert_eq!(old, "input swipe 0 1000 0 400 500");
        let mut codes = vec![123];
        codes.extend(std::iter::repeat_n(67, 150));
        let clear = Input::Keys(codes).adb_command(33).unwrap();
        assert_eq!(clear.matches("input keyevent").count(), 2);
        assert!(clear.starts_with("input keyevent 123 67 67"));
        assert_eq!(
            Input::Combo {
                code: 29,
                modifiers: vec![113]
            }
            .adb_command(33)
            .unwrap(),
            "input keycombination 113 29"
        );
        assert_eq!(
            Input::Combo {
                code: 66,
                modifiers: vec![]
            }
            .adb_command(26)
            .unwrap(),
            "input keyevent 66"
        );
        assert_eq!(
            Input::Combo {
                code: 29,
                modifiers: vec![113]
            }
            .adb_command(32)
            .unwrap_err()
            .outcome,
            Outcome::NotSent
        );
        assert_eq!(
            Input::Text("hello world".into()).adb_command(33).unwrap(),
            "input text hello%sworld"
        );
        assert_eq!(
            Input::Text("화면".into()).adb_command(33).unwrap_err().code,
            "unsupported_text"
        );
    }

    #[test]
    fn inputs_become_helper_requests() {
        let (op, args) = Input::Tap {
            x: 1,
            y: 2,
            hold_ms: 0,
            count: 2,
        }
        .helper_request();
        assert_eq!((op, args["count"].as_u64()), ("tap", Some(2)));
        let (op, args) = Input::Swipe {
            from: (1, 2),
            to: (3, 4),
            ms: 500,
            hold_ms: 250,
        }
        .helper_request();
        assert_eq!(op, "swipe");
        assert_eq!(
            (args["x2"].as_i64(), args["hold_ms"].as_u64()),
            (Some(3), Some(250))
        );
        let (op, args) = Input::Taps(vec![(5, 6)]).helper_request();
        assert_eq!((op, args["points"][0][1].as_i64()), ("taps", Some(6)));
        let (op, args) = Input::Combo {
            code: 29,
            modifiers: vec![113],
        }
        .helper_request();
        assert_eq!((op, args["modifiers"][0].as_u64()), ("key", Some(113)));
    }

    #[test]
    fn the_backend_is_adb_when_any_step_fell_back() {
        let mut s = AndroidSession::new("R39N402N5SB".into(), Config::default());
        assert_eq!(s.backend(), None);
        s.used_helper = true;
        assert_eq!(s.backend(), Some("helper"));
        s.used_adb = true;
        assert_eq!(s.backend(), Some("adb"));
        s.begin_command();
        assert_eq!(s.backend(), None);
        let mut off = Config::default();
        off.android.helper = false;
        let mut s = AndroidSession::new("R39N402N5SB".into(), off);
        assert_eq!(s.helper_status()["state"], "off");
    }

    #[test]
    fn connection_follows_the_serial() {
        let s = |id: &str| AndroidSession::new(id.into(), Config::default()).connection();
        assert_eq!(s("R39N402N5SB"), "usb");
        assert_eq!(s("192.168.0.7:5555"), "wifi");
        assert_eq!(s("emulator-5554"), "emulator");
    }
}
