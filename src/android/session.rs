//! An Android phone over adb: the same commands and result fields as the
//! iPhone's [`Session`](crate::device::session::Session).
//!
//! Reading is `uiautomator dump`, input is `input tap|swipe|motionevent|
//! keyevent|text`, apps are `am`/`pm`/`cmd package`, screenshots
//! `screencap -p`. Nothing is installed on the phone and no connection is
//! held: every call is one `adb` process, so there is nothing to reconnect.
//!
//! `uiautomator dump` waits for the app's UI to go idle before it dumps, so
//! one reading after an input already shows the screen once it has stopped
//! moving; where the iPhone path reads twice to see that a screen holds still,
//! this one reads once.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    adb::{Adb, AdbError, sh_quote},
    keys,
    uia::{self, Geometry, XmlNode},
};
use crate::{
    actions::{
        self, AFTER_DRAG, AFTER_LAUNCH, AFTER_PASTE, ButtonArgs, CHANGE_WAIT, FindArgs, KeyArgs, LaunchArgs, ReadArgs,
        ScreenshotArgs, ScrollArgs, SwipeArgs, TapArgs, Target, TypeArgs, TypeObservation, brief, parse_target,
        resolve_one, tap_point,
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
    let line = out
        .lines()
        .find(|l| l.contains("Exception") || l.starts_with("Error") || l.contains("Unknown command"))?;
    Some(line.trim().to_string())
}

/// `cmd package query-activities --brief` / `resolve-activity --brief`:
/// lines that are `package/activity`.
pub fn parse_components(out: &str) -> Vec<(String, String)> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.contains(' ') && !l.contains('=') && l.contains('/'))
        .filter_map(|l| l.split_once('/').map(|(p, a)| (p.to_string(), a.to_string())))
        .collect()
}

/// `pm list packages`: `package:<name>` lines.
pub fn parse_packages(out: &str) -> Vec<String> {
    out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(|p| p.trim().to_string()).collect()
}

impl AndroidSession {
    pub fn new(serial: String, config: Config) -> Self {
        Self {
            adb: Adb::new(serial.clone()),
            serial,
            config,
            geometry: None,
            sdk: None,
            latest: None,
            seq: 0,
            stale_before: None,
            pids: HashMap::new(),
        }
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
        let sdk = self.adb.getprop("ro.build.version.sdk").await.and_then(|v| v.parse().ok());
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
            return Err(Fail::not_sent("geometry_failed", format!("could not read the screen size from `wm`: {}", out.trim())));
        };
        let g = Geometry { px, scale: density as f64 / 160.0 };
        self.geometry = Some(g);
        Ok(g)
    }

    async fn pid_of(&mut self, pkg: &str) -> i64 {
        if let Some(p) = self.pids.get(pkg) {
            return *p;
        }
        let pid = match self.adb.shell(&format!("pidof {}", sh_quote(pkg)), 8).await {
            Ok(out) => parse_pid(&out),
            Err(_) => None,
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
            match self.adb.exec_out("uiautomator dump /dev/tty", DUMP_SECS).await {
                Ok(bytes) => {
                    let out = String::from_utf8_lossy(&bytes);
                    if let Some(xml) = uia::extract_xml(&out) {
                        return Ok(xml.to_string());
                    }
                    last = out.trim().chars().take(300).collect();
                }
                Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => return Err(e.not_sent("ui_dump_failed")),
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
                Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => return Err(e.not_sent("ui_dump_failed")),
                Err(e) => last = e.message(),
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
        Err(Fail::not_sent("ui_dump_failed", format!("uiautomator gave no screen: {last}")).hint(t(
            "화면이 계속 움직이면(애니메이션·영상) uiautomator가 멈춘 화면을 기다리다 실패합니다. 화면이 켜져 있고 잠금이 풀려 있는지 확인하고 `phone doctor`를 실행하세요.",
            "uiautomator fails while the screen keeps moving (animations, video). Check that the screen is on and unlocked, and run `phone doctor`.",
        )))
    }

    async fn read_now(&mut self) -> Res<u64> {
        let geo = self.geometry().await?;
        let xml = self.dump_xml().await?;
        let root = tokio::task::spawn_blocking(move || uia::parse_xml(&xml))
            .await
            .map_err(|e| Fail::not_sent("ui_dump_failed", e.to_string()))?
            .map_err(|e| Fail::not_sent("ui_dump_unparsed", format!("the uiautomator dump was not understood: {e}")))?;
        let pkg = uia::dump_package(&root).unwrap_or_default();
        let pid = if pkg.is_empty() { 0 } else { self.pid_of(&pkg).await };
        let app = AppInfo {
            pid,
            name: (!pkg.is_empty()).then(|| pkg.clone()),
            bundle_id: (!pkg.is_empty()).then_some(pkg),
        };
        let _ = geo;
        self.seq += 1;
        self.latest = Some(Reading { seq: self.seq, at: Instant::now(), root: Arc::new(root), app });
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
        self.build(opts).ok_or_else(|| Fail::not_sent("ui_dump_failed", "no reading"))
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
        self.read_screen(Fresh::Force, BuildOptions::default()).await.ok()
    }

    /// Read after a tap until the screen differs from `before`, for up to
    /// `CHANGE_WAIT`. Each dump already waits for the UI to go idle.
    async fn observe_settled(&mut self, since: Instant, before: &[ElementSignature]) -> Option<(u64, Screen)> {
        let mut latest = self.observe(AFTER_TAP).await?;
        while latest.1.signature() == before && since.elapsed() < CHANGE_WAIT {
            match self.read_screen(Fresh::Force, BuildOptions::default()).await {
                Ok(next) => latest = next,
                Err(_) => break,
            }
        }
        Some(latest)
    }

    async fn locate(&mut self, targets: &[Target], fresh: Fresh) -> Res<(Screen, Vec<(f64, f64, Option<Element>)>)> {
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

    /// The shell commands for one drag. A flick is `input swipe` (Android
    /// computes a fling from its speed). A drag that comes to rest is a
    /// `motionevent` sequence that holds still at the end before lifting,
    /// so the list moves by the finger's distance and does not coast.
    async fn drag_cmd(&mut self, from: (f64, f64), to: (f64, f64), duration: Duration, motion: Motion) -> String {
        let (x1, y1) = self.px(from.0, from.1);
        let (x2, y2) = self.px(to.0, to.1);
        let ms = duration.as_millis().max(50);
        if motion == Motion::Flick || self.sdk().await < 29 {
            return format!("input swipe {x1} {y1} {x2} {y2} {ms}");
        }
        let steps = ((ms / 60) as i64).clamp(4, 24);
        let mut cmd = format!("input motionevent DOWN {x1} {y1}");
        for i in 1..=steps {
            let x = x1 + (x2 - x1) * i / steps;
            let y = y1 + (y2 - y1) * i / steps;
            cmd.push_str(&format!("; input motionevent MOVE {x} {y}"));
        }
        cmd.push_str(&format!("; sleep 0.25; input motionevent MOVE {x2} {y2}; input motionevent UP {x2} {y2}"));
        cmd
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
        let mut all: Vec<String> = launchable.into_iter().map(|(p, _)| p).chain(third).collect();
        all.sort();
        all.dedup();
        // No labels without installing something; match the package's own
        // words instead: "settings" finds com.android.settings.
        let hits: Vec<&String> = all
            .iter()
            .filter(|p| p.to_lowercase().rsplit('.').next() == Some(want.as_str()))
            .collect();
        match hits.len() {
            1 => Ok(hits[0].clone()),
            0 => Err(Fail::not_sent("app_not_found", format!("no installed package ends in \"{app}\"")).hint(t(
                "Android에서는 패키지 이름으로 실행하세요(예: com.android.settings). `phone apps`로 확인할 수 있습니다.",
                "On Android launch by package name (com.android.settings, for one). See them with `phone apps`.",
            ))),
            n => Err(Fail::not_sent("ambiguous_app", format!("{n} packages end in \"{app}\""))
                .with("candidates", Value::Array(hits.iter().map(|p| json!(p)).collect()))),
        }
    }

    fn not_supported(what: &str) -> Fail {
        Fail::not_sent("unsupported", format!("{what} is not supported on Android yet"))
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
    }

    async fn close_input(&mut self) {}

    async fn screen(&mut self, fresh: Fresh, _full: bool, opts: BuildOptions) -> Res<(u64, Screen)> {
        self.read_screen(fresh, opts).await
    }

    async fn cmd_read(&mut self, args: ReadArgs) -> Res {
        let fresh = match args.max_age_ms {
            Some(ms) => Fresh::MaxAge(Duration::from_millis(ms)),
            None => Fresh::Force,
        };
        let (seq, screen) = self.read_screen(fresh, BuildOptions { include_offscreen: args.all, raw: args.raw }).await?;
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
            .read_screen(Fresh::Force, BuildOptions { include_offscreen: args.all, raw: false })
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
        let (screen, mut points) = self.locate(std::slice::from_ref(&target), Fresh::MaxAge(reuse())).await?;
        let (x, y, element) = points.remove(0);
        let (px, py) = self.px(x, y);
        let cmd = if args.double {
            format!("input tap {px} {py}; input tap {px} {py}")
        } else if let Some(ms) = args.long_ms {
            format!("input swipe {px} {py} {px} {py} {ms}")
        } else {
            format!("input tap {px} {py}")
        };
        let before = screen.signature();
        let began = Instant::now();
        self.send(&cmd).await?;
        let after = if args.no_wait {
            self.observe(AFTER_TAP).await
        } else {
            self.observe_settled(began, &before).await
        };
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after.as_ref().and_then(|(_, s)| actions::app_changed(&screen, s));
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
            out.insert("note".into(), json!("the tap was sent but no reading came back to compare"));
        }
        if let Some(change) = app_change {
            out.insert("app_changed".into(), json!(true));
            out.insert("app".into(), change);
        }
        if !args.no_wait
            && let Some((seq, screen)) = after
        {
            out.insert("screen".into(), json!({"width": screen.width, "height": screen.height}));
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
        let cmd = points
            .iter()
            .map(|(x, y, _)| {
                let (px, py) = self.px(*x, *y);
                format!("input tap {px} {py}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let tapped: Vec<Value> = points.iter().map(|(_, _, el)| el.as_ref().map(brief).unwrap_or(Value::Null)).collect();
        self.send(&cmd).await.map_err(|f| {
            f.hint(t(
                "입력이 일부 나갔을 수 있습니다. 다시 보내지 말고 `phone read`로 화면을 확인하세요.",
                "Part of the input may have gone out. Do not resend; check the screen with `phone read`.",
            ))
        })?;
        let after = self.observe(AFTER_TAP).await;
        let changed = after.as_ref().map(|(_, s)| s.signature() != before);
        let app_change = after.as_ref().and_then(|(_, s)| actions::app_changed(&screen, s));
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
        let (_, screen) = self.read_screen(Fresh::MaxAge(reuse()), BuildOptions::default()).await?;
        for (x, y) in [from, to] {
            if x < 0.0 || y < 0.0 || x > screen.width || y > screen.height {
                return Err(Fail::invalid(format!(
                    "point {x},{y} is outside the {}x{} screen",
                    screen.width, screen.height
                )));
            }
        }
        let before = screen.signature();
        let cmd = self.drag_cmd(from, to, duration, motion).await;
        self.send(&cmd).await?;
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
                        return Err(Fail::invalid(format!("unknown direction `{other}` (up, down, left, right)")));
                    }
                }
            }
            (None, Some(f), Some(t)) => ((f[0], f[1]), (t[0], t[1])),
            _ => return Err(Fail::invalid("give a direction, or both from and to")),
        };
        let duration = Duration::from_millis(args.duration_ms.unwrap_or(250).clamp(50, 10_000));
        let motion = if args.drag { Motion::Place } else { Motion::Flick };
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
        let (_, mut screen) = self.read_screen(Fresh::MaxAge(reuse()), BuildOptions::default()).await?;
        loop {
            let matches = dedupe_targets(sel.find(&screen));
            if let Some(el) = matches.iter().find(|e| tap_point(e, &screen).is_ok()) {
                if matches.len() > 1 {
                    return Err(Fail::not_sent(
                        "ambiguous_selector",
                        format!("{} elements match {}", matches.len(), sel.describe()),
                    )
                    .with("candidates", Value::Array(matches.iter().map(|e| brief(e)).collect()))
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
                    format!("{} not found; the list stopped moving after {scrolls} scrolls", sel.describe()),
                )
                .with("found", false)
                .with("scrolls", scrolls)
                .with("screen_summary", screen.summary(12)));
            }
            screen = match after {
                Some(s) => s,
                None => self.read_screen(Fresh::MaxAge(reuse()), BuildOptions::default()).await?.1,
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
        if let Err(why) = keys::typeable(&args.text) {
            return Err(Fail::not_sent("unsupported_text", why).hint(t(
                "Android에서는 adb `input text`로 입력하므로 영문·숫자·기호(ASCII)만 됩니다. 한글·이모지는 아직 입력할 수 없습니다(폰에 아무것도 설치하지 않는 원칙). 줄바꿈은 `phone key return`으로 따로 보내세요.",
                "On Android text goes through adb `input text`, which types printable ASCII only. Korean and emoji cannot be typed yet (nothing is installed on the phone). Send line breaks separately with `phone key return`.",
            )));
        }
        let (_, screen) = self.read_screen(Fresh::Force, BuildOptions::default()).await?;
        let Some(field) = Self::focused_field(&screen).cloned() else {
            return Err(Fail::not_sent("no_focused_field", "no text field has the keyboard right now")
                .hint(t(
                    "먼저 입력할 칸을 `phone tap`으로 눌러 키보드를 띄우세요.",
                    "Tap the field first with `phone tap` so the keyboard comes up.",
                ))
                .with("screen_summary", screen.summary(12)));
        };
        let mut cmd = Vec::new();
        if args.clear {
            // To the end, then one Delete per character the field shows
            // (more than needed when that is a placeholder; harmless).
            let n = field.value.as_deref().map(|v| v.chars().count()).unwrap_or(0).clamp(1, 2000);
            cmd.push("input keyevent 123".to_string());
            for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
                cmd.push(format!("input keyevent{}", " 67".repeat(chunk.len())));
            }
        }
        if !args.text.is_empty() {
            cmd.push(format!("input text {}", keys::input_text_arg(&args.text)));
        }
        let began = Instant::now();
        self.send(&cmd.join("; ")).await.map_err(|f| f.with("clipboard_restored", true))?;
        tokio::time::sleep(AFTER_PASTE).await;
        let after = self.read_screen(Fresh::Force, BuildOptions::default()).await.ok().map(|(_, s)| s);
        // The original field only; focus moving elsewhere is not its value.
        let value = after.as_ref().and_then(|s| {
            s.by_id(&field.id)
                .filter(|e| e.role == field.role)
                .or_else(|| Self::focused_field(s).filter(|e| e.identifier.is_some() && e.identifier == field.identifier))
                .and_then(|e| e.value.clone())
        });
        let observation = TypeObservation::new(&args, field.value.clone(), value);
        let mut submitted = Some(false);
        let mut submit_error = None;
        let mut final_screen = after;
        if args.submit && observation.can_submit() {
            match self.send("input keyevent 66").await {
                Ok(()) => {
                    submitted = Some(true);
                    final_screen = self.observe(AFTER_TAP).await.map(|(_, s)| s);
                }
                Err(f) if f.outcome == Outcome::NotSent => submit_error = Some(format!("Return not sent: {}", f.message)),
                Err(f) => {
                    submitted = None;
                    submit_error = Some(f.message);
                }
            }
        }
        let _ = began;
        let screen_changed = final_screen.as_ref().map(|s| s.signature() != screen.signature());
        let mut result = observation.result(&args, &field, true, submitted, screen_changed, submit_error);
        let out = match &mut result {
            Ok(out) => out,
            Err(fail) => &mut fail.extra,
        };
        out.insert("input_method".into(), json!("adb_input_text"));
        result
    }

    async fn cmd_key(&mut self, args: KeyArgs) -> Res {
        let (code, modifiers) = keys::parse_combo(&args.key).map_err(Fail::invalid)?;
        let cmd = if modifiers.is_empty() {
            format!("input keyevent {code}")
        } else if self.sdk().await >= 33 {
            let mods: Vec<String> = modifiers.iter().map(|m| m.to_string()).collect();
            format!("input keycombination {} {code}", mods.join(" "))
        } else {
            return Err(Fail::not_sent("unsupported", "key combinations need Android 13 (API 33) or later"));
        };
        let before = self
            .read_screen(Fresh::MaxAge(reuse()), BuildOptions::default())
            .await
            .ok()
            .map(|(_, s)| s.signature());
        self.send(&cmd).await?;
        let changed = match &before {
            Some(sig) => self.observe(AFTER_TAP).await.map(|(_, s)| s.signature() != *sig),
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
        self.send(&format!("input keyevent {code}")).await?;
        let changed = match &before {
            Some(sig) => self.observe(AFTER_LAUNCH).await.map(|(_, s)| s.signature() != *sig),
            None => None,
        };
        let mut out = fields! {
            "sent": true,
            "verified": changed.is_some(),
            "screen_changed": changed.unwrap_or(false),
            "button": args.button,
        };
        if before.is_none() {
            out.insert("note".into(), json!("the screen could not be read before the button; not verified"));
        }
        Ok(out)
    }

    async fn cmd_apps(&mut self) -> Res {
        let (launchable, third) = self.installed().await?;
        let mut pkgs: Vec<String> = launchable.iter().map(|(p, _)| p.clone()).chain(third.iter().cloned()).collect();
        pkgs.sort();
        pkgs.dedup();
        let list: Vec<Value> = pkgs
            .iter()
            .map(|p| {
                let user = third.contains(p);
                json!({
                    "bundle_id": p,
                    "name": Value::Null,
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
                &format!("cmd package resolve-activity --brief -c android.intent.category.LAUNCHER {}", sh_quote(&pkg)),
                15,
            )
            .await
            .map_err(|e| e.not_sent("launch_failed"))?;
        let component = parse_components(&resolved).into_iter().rev().find(|(p, _)| *p == pkg);
        let Some((_, activity)) = component else {
            return Err(Fail::not_sent("app_not_found", format!("{pkg} has no launcher activity (or is not installed)"))
                .hint(t("`phone apps`로 패키지 이름을 확인하세요.", "Check the package name with `phone apps`.")));
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
                let line = o.lines().find(|l| l.contains("Error") || l.contains("Exception")).unwrap_or("").trim();
                return Err(Fail::not_sent("launch_failed", line.to_string()));
            }
            Ok(_) => {}
            Err(e @ (AdbError::Missing | AdbError::Unreachable(_))) => return Err(e.not_sent("launch_failed")),
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
                && screen.apps.iter().any(|a| a.bundle_id.as_deref() == Some(pkg.as_str()))
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
            let head: String = String::from_utf8_lossy(&png[..png.len().min(200)]).trim().to_string();
            return Err(Fail::not_sent("screenshot_failed", format!("screencap gave no PNG: {head}")));
        }
        let path = std::path::PathBuf::from(&args.path);
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", dir.display())))?;
        }
        std::fs::write(&path, &png).map_err(|e| Fail::not_sent("write_failed", format!("{}: {e}", path.display())))?;
        Ok(fields! { "path": args.path, "bytes": png.len() })
    }

    /// A dump comes only once the UI is idle, so a fresh reading's element
    /// is where it rests; an older one is read again by the caller.
    async fn held_still(&mut self, sel: &Selector, screen: &Screen) -> bool {
        self.latest_age().is_some_and(|age| age <= reuse()) && dedupe_targets(sel.find(screen)).len() == 1
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
        other => return Err(Fail::invalid(format!("unknown direction `{other}` (up, down)"))),
    };
    s.drag_points(from, to, Duration::from_millis(500), Motion::Place).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_components_and_packages_parse() {
        let resolve = "priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\ncom.android.settings/.Settings\n";
        assert_eq!(parse_components(resolve), vec![("com.android.settings".to_string(), ".Settings".to_string())]);
        let query = "2 activities found:\n  Activity #0:\n    priority=0 preferredOrder=0 match=0x108000 specificIndex=-1 isDefault=true\n    com.a/com.a.Main\n  Activity #1:\n    com.b/.Start\n";
        assert_eq!(parse_components(query).len(), 2);
        assert_eq!(parse_components("No activity found\n"), vec![]);
        assert_eq!(parse_packages("package:com.a\npackage:com.b\n\n"), vec!["com.a", "com.b"]);
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
    fn connection_follows_the_serial() {
        let s = |id: &str| AndroidSession::new(id.into(), Config::default()).connection();
        assert_eq!(s("R39N402N5SB"), "usb");
        assert_eq!(s("192.168.0.7:5555"), "wifi");
        assert_eq!(s("emulator-5554"), "emulator");
    }
}
