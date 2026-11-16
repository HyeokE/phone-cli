//! `phone doctor` for an Android phone: adb, authorization, screen on and
//! unlocked, and a real `uiautomator dump`. Sends no input.

use serde_json::{Value, json};

use super::{adb, session::AndroidSession};
use crate::{
    ax::tree::BuildOptions,
    device::session::Fresh,
    doctor::{Check, DoctorArgs},
    fields,
    platform::Phone,
    reply::Res,
};

/// `dumpsys power`: awake unless `mWakefulness` says otherwise.
pub fn screen_awake(power: &str) -> Option<bool> {
    let v = power.lines().find_map(|l| l.trim().strip_prefix("mWakefulness="))?;
    Some(v.trim().eq_ignore_ascii_case("awake"))
}

/// `dumpsys window`: whether the keyguard (lock screen) is up. Versions name
/// it differently; any of them being true is locked.
pub fn keyguard_showing(window: &str) -> Option<bool> {
    let mut seen = None;
    for key in ["isKeyguardShowing=", "mShowingLockscreen=", "mDreamingLockscreen=", "KeyguardShowing="] {
        for l in window.lines() {
            for part in l.split_whitespace() {
                if let Some(v) = part.strip_prefix(key) {
                    let locked = v.starts_with("true");
                    seen = Some(seen.unwrap_or(false) || locked);
                }
            }
        }
    }
    seen
}

pub async fn run(s: &mut AndroidSession, _args: DoctorArgs) -> Res {
    let mut checks: Vec<Check> = Vec::new();
    let skip = |name: &'static str, why: &str| Check::new(name, true).status("skipped").detail(why.to_string());

    // 1. adb itself.
    let Some(path) = adb::adb_path() else {
        checks.push(Check::new("adb", true).status("fail").action(
            "Android SDK platform-tools를 설치하거나 `ADB=/path/to/adb`를 지정하세요.",
            "Install the Android SDK platform-tools, or set `ADB=/path/to/adb`.",
        ));
        return Ok(finish(s, checks));
    };
    checks.push(Check::new("adb", true).detail(path.display().to_string()));

    // 2. The device is listed and authorized.
    let listed = adb::list_devices().await.ok().and_then(|l| l.into_iter().find(|d| d.serial == s.serial));
    let usable = match &listed {
        Some(d) if d.state == "device" => {
            checks.push(Check::new("device", true).detail(format!("{} over {}", d.model.clone().unwrap_or_default(), d.connection)));
            true
        }
        Some(d) if d.state == "unauthorized" => {
            checks.push(Check::new("device", true).status("fail").detail("unauthorized").action(
                "폰 화면의 'USB 디버깅을 허용하시겠습니까?'에서 허용을 누르세요.",
                "Tap Allow on the phone's \"Allow USB debugging?\" prompt.",
            ));
            false
        }
        Some(d) => {
            checks.push(Check::new("device", true).status("fail").detail(d.state.clone()).action(
                "USB를 다시 꽂거나 `adb connect <IP>:<port>`로 다시 연결하세요.",
                "Plug the cable in again, or reconnect with `adb connect <IP>:<port>`.",
            ));
            false
        }
        None => {
            checks.push(Check::new("device", true).status("fail").detail("not listed by `adb devices`").action(
                "개발자 옵션에서 USB 디버깅(또는 무선 디버깅)을 켜고 연결하세요.",
                "Turn on USB debugging (or Wireless debugging) in Developer options and connect.",
            ));
            false
        }
    };
    if !usable {
        for name in ["screen_on", "screen_unlocked", "accessibility_tree"] {
            checks.push(skip(name, "the device is not usable over adb"));
        }
        return Ok(finish(s, checks));
    }

    // 3. Screen on.
    let power = s.adb().shell("dumpsys power", 15).await.ok();
    let awake = power.as_deref().and_then(screen_awake);
    checks.push(match awake {
        Some(true) => Check::new("screen_on", true),
        Some(false) => Check::new("screen_on", true).status("fail").action(
            "폰 화면을 켜 주세요. 오래 쓸 때는 개발자 옵션의 '화면 켜짐 상태 유지'를 권합니다.",
            "Wake the phone. For long sessions turn on Developer options → Stay awake.",
        ),
        None => Check::new("screen_on", true).status("warn").detail("dumpsys power did not say"),
    });

    // 4. Unlocked.
    let window = s.adb().shell("dumpsys window", 15).await.ok();
    let locked = window.as_deref().and_then(keyguard_showing);
    checks.push(match locked {
        Some(false) => Check::new("screen_unlocked", true),
        Some(true) => Check::new("screen_unlocked", true).status("fail").action(
            "폰 잠금을 풀어 주세요. 비밀번호·지문 입력은 대신 하지 않습니다.",
            "Unlock the phone. Passwords and fingerprints are never entered for you.",
        ),
        None => Check::new("screen_unlocked", true).status("warn").detail("dumpsys window did not say"),
    });

    // 5. A real dump.
    checks.push(match s.screen(Fresh::Force, false, BuildOptions::default()).await {
        Ok((_, screen)) if screen.elements.len() > 1 => Check::new("accessibility_tree", true).detail(format!(
            "{} elements, {} in front",
            screen.elements.len(),
            screen.apps.first().and_then(|a| a.bundle_id.clone()).unwrap_or_default()
        )),
        Ok(_) => Check::new("accessibility_tree", true).status("fail").detail("empty dump").action(
            "화면을 켜고 잠금을 푼 뒤 다시 시도하세요.",
            "Wake and unlock the phone, then try again.",
        ),
        Err(f) => Check::new("accessibility_tree", true).status("fail").detail(f.message).action(
            "애니메이션이나 영상이 계속 움직이는 화면에서는 uiautomator가 실패합니다. 홈 화면에서 다시 시도하세요.",
            "uiautomator fails on screens that never stop moving (animations, video). Try again from the home screen.",
        ),
    });

    // 6. Stay awake while charging (advice).
    let stay = s.adb().shell("settings get global stay_on_while_plugged_in", 8).await.ok();
    if stay.as_deref().map(str::trim).is_some_and(|v| v == "0") {
        checks.push(Check::new("stay_awake", false).status("info").action(
            "오래 쓸 때는 개발자 옵션 → '화면 켜짐 상태 유지'를 켜 두세요. 화면이 꺼지면 읽기와 입력이 멈춥니다.",
            "For long sessions turn on Developer options → Stay awake. Reading and input stop when the screen goes off.",
        ));
    }
    checks.push(Check::new("do_not_disturb", false).status("info").detail("`phone dnd` is not supported on Android"));
    Ok(finish(s, checks))
}

fn finish(s: &AndroidSession, checks: Vec<Check>) -> serde_json::Map<String, Value> {
    let ok = checks.iter().all(|c| !c.required || c.status == "ok");
    let failing: Vec<Value> = checks.iter().filter(|c| c.required && c.status != "ok").map(|c| json!(c.check)).collect();
    fields! {
        "ready": ok,
        "udid": s.serial.clone(),
        "platform": "android",
        "connection": s.connection(),
        "failing": failing,
        "checks": checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_and_keyguard_states_are_read() {
        assert_eq!(screen_awake("  mWakefulness=Awake\n"), Some(true));
        assert_eq!(screen_awake("  mWakefulness=Asleep\n"), Some(false));
        assert_eq!(screen_awake("nothing"), None);
        assert_eq!(keyguard_showing("    mShowingDream=false mDreamingLockscreen=false\n    isKeyguardShowing=false\n"), Some(false));
        assert_eq!(keyguard_showing("    isKeyguardShowing=true\n"), Some(true));
        assert_eq!(keyguard_showing("  mShowingLockscreen=true mShowingDream=false\n"), Some(true));
        assert_eq!(keyguard_showing(""), None);
    }
}
