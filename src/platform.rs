//! What the commands need from a phone, whatever the phone is.
//!
//! `Phone` is the small set of primitives (`screen`, the `cmd_*` commands,
//! drags, a few questions about the latest reading) that `batch` steps
//! ([`crate::steps::Steps`]) and scripts ([`crate::script::Scripts`]) are
//! written against. The iPhone's [`Session`] implements it by handing every
//! call to its own methods, so the iOS path runs exactly the code it ran
//! before; [`AndroidSession`] implements it over adb.
//!
//! [`Device`] is what the daemon holds: one of the two, chosen once from the
//! device id when the daemon starts ([`detect`]).

#![allow(async_fn_in_trait)]

use std::time::Duration;

use serde_json::Value;

use crate::{
    actions::{
        ButtonArgs, FindArgs, KeyArgs, LaunchArgs, ReadArgs, ScreenshotArgs, ScrollArgs, SwipeArgs,
        TapArgs, TypeArgs,
    },
    android::session::AndroidSession,
    ax::tree::{BuildOptions, Screen},
    config::Config,
    device::{
        input::Motion,
        session::{Fresh, Session},
    },
    reply::Res,
    selector::Selector,
};

/// Which kind of phone a device id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }
}

/// An iPhone UDID: 40 hex digits (older phones) or `XXXXXXXX-XXXXXXXXXXXXXXXX`
/// (8 and 16 hex digits). adb serials look different (`R39N402N5SB`,
/// `192.168.0.5:5555`, `emulator-5554`), so these never reach adb.
pub fn looks_like_ios_udid(id: &str) -> bool {
    let hex = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit());
    if id.len() == 40 && hex(id) {
        return true;
    }
    match id.split_once('-') {
        Some((a, b)) => a.len() == 8 && b.len() == 16 && hex(a) && hex(b),
        None => false,
    }
}

/// The platform of `id`. Anything shaped like an iPhone UDID is an iPhone
/// without asking adb; anything else is Android when adb lists it, and an
/// iPhone otherwise (as every id was before Android support).
pub async fn detect(id: &str) -> Platform {
    if looks_like_ios_udid(id) {
        return Platform::Ios;
    }
    match crate::android::adb::list_devices().await {
        Ok(list) if list.iter().any(|d| d.serial == id) => Platform::Android,
        _ => Platform::Ios,
    }
}

/// The primitives steps and scripts use. Every method keeps the meaning of
/// the iPhone method of the same name (see `actions.rs`).
pub trait Phone {
    fn config(&self) -> &Config;
    /// Drop cached connections that are known to be closed.
    fn refresh_connection(&mut self);
    /// Abandon every connection (after a call was cut off).
    fn reset(&mut self);
    /// End the input session now, so the next input opens a new one.
    async fn close_input(&mut self);
    async fn screen(&mut self, fresh: Fresh, full: bool, opts: BuildOptions) -> Res<(u64, Screen)>;

    async fn cmd_read(&mut self, args: ReadArgs) -> Res;
    async fn cmd_find(&mut self, args: FindArgs) -> Res;
    async fn cmd_tap(&mut self, args: TapArgs) -> Res;
    async fn cmd_swipe(&mut self, args: SwipeArgs) -> Res;
    async fn cmd_scroll(&mut self, args: ScrollArgs) -> Res;
    async fn cmd_type(&mut self, args: TypeArgs) -> Res;
    async fn cmd_key(&mut self, args: KeyArgs) -> Res;
    async fn cmd_button(&mut self, args: ButtonArgs) -> Res;
    async fn cmd_launch(&mut self, args: LaunchArgs) -> Res;
    async fn cmd_apps(&mut self) -> Res;
    async fn cmd_screenshot(&mut self, args: ScreenshotArgs) -> Res;

    /// Tap several targets in a row on the screen as it is.
    async fn tap_sequence(&mut self, targets: &[Value]) -> Res;
    /// Drag between two points (logical units) and report whether the screen
    /// changed, with the reading taken afterwards.
    async fn drag_points(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Res<(bool, Option<Screen>)>;
    /// Whether the one element `sel` finds on `screen` is still where that
    /// reading put it, asked cheaply. False sends the caller to full readings.
    async fn held_still(&mut self, sel: &Selector, screen: &Screen) -> bool;
    /// The connection that delivered the latest reading is gone, so that
    /// reading may not describe the screen any more.
    fn reading_lost(&self) -> bool;
    /// Pids of the applications in the latest reading.
    fn latest_app_pids(&self) -> Option<Vec<i64>>;
    /// The latest reading, built with default options.
    fn latest_screen(&self) -> Option<Screen>;
}

impl Phone for Session {
    fn config(&self) -> &Config {
        &self.config
    }
    fn refresh_connection(&mut self) {
        Session::refresh_connection(self)
    }
    fn reset(&mut self) {
        Session::reset(self)
    }
    async fn close_input(&mut self) {
        Session::close_input(self).await
    }
    async fn screen(&mut self, fresh: Fresh, full: bool, opts: BuildOptions) -> Res<(u64, Screen)> {
        Session::screen(self, fresh, full, opts).await
    }
    async fn cmd_read(&mut self, args: ReadArgs) -> Res {
        Session::cmd_read(self, args).await
    }
    async fn cmd_find(&mut self, args: FindArgs) -> Res {
        Session::cmd_find(self, args).await
    }
    async fn cmd_tap(&mut self, args: TapArgs) -> Res {
        Box::pin(Session::cmd_tap(self, args)).await
    }
    async fn cmd_swipe(&mut self, args: SwipeArgs) -> Res {
        Box::pin(Session::cmd_swipe(self, args)).await
    }
    async fn cmd_scroll(&mut self, args: ScrollArgs) -> Res {
        Box::pin(Session::cmd_scroll(self, args)).await
    }
    async fn cmd_type(&mut self, args: TypeArgs) -> Res {
        Box::pin(Session::cmd_type(self, args)).await
    }
    async fn cmd_key(&mut self, args: KeyArgs) -> Res {
        Session::cmd_key(self, args).await
    }
    async fn cmd_button(&mut self, args: ButtonArgs) -> Res {
        Session::cmd_button(self, args).await
    }
    async fn cmd_launch(&mut self, args: LaunchArgs) -> Res {
        Box::pin(Session::cmd_launch(self, args)).await
    }
    async fn cmd_apps(&mut self) -> Res {
        Session::cmd_apps(self).await
    }
    async fn cmd_screenshot(&mut self, args: ScreenshotArgs) -> Res {
        Session::cmd_screenshot(self, args).await
    }
    async fn tap_sequence(&mut self, targets: &[Value]) -> Res {
        Box::pin(Session::tap_sequence(self, targets)).await
    }
    async fn drag_points(
        &mut self,
        from: (f64, f64),
        to: (f64, f64),
        duration: Duration,
        motion: Motion,
    ) -> Res<(bool, Option<Screen>)> {
        Box::pin(Session::drag_points(self, from, to, duration, motion)).await
    }
    async fn held_still(&mut self, sel: &Selector, screen: &Screen) -> bool {
        Session::held_still(self, sel, screen).await
    }
    fn reading_lost(&self) -> bool {
        self.ax.as_ref().is_none_or(|ax| ax.is_closed())
    }
    fn latest_app_pids(&self) -> Option<Vec<i64>> {
        let (_, dump, _) = self.ax.as_ref()?.latest()?;
        Some(dump.applications.iter().map(|a| a.pid).collect())
    }
    fn latest_screen(&self) -> Option<Screen> {
        let (_, dump, _) = self.ax.as_ref()?.latest()?;
        Some(Screen::build(&dump, BuildOptions::default()))
    }
}

/// What the daemon holds for its one phone.
pub enum Device {
    Ios(Box<Session>),
    Android(Box<AndroidSession>),
}

impl Device {
    pub fn platform(&self) -> Platform {
        match self {
            Device::Ios(_) => Platform::Ios,
            Device::Android(_) => Platform::Android,
        }
    }

    pub fn reset(&mut self) {
        match self {
            Device::Ios(s) => s.reset(),
            Device::Android(a) => Phone::reset(a.as_mut()),
        }
    }

    pub async fn shutdown(&mut self) {
        match self {
            Device::Ios(s) => s.shutdown().await,
            Device::Android(a) => a.shutdown().await,
        }
    }

    pub async fn sweep(&mut self) {
        if let Device::Ios(s) = self {
            s.sweep().await;
        }
    }

    pub async fn close_idle_input(&mut self) {
        if let Device::Ios(s) = self {
            s.close_idle_input().await;
        }
    }

    pub async fn close_input_if_unheld(&mut self) {
        if let Device::Ios(s) = self {
            s.close_input_if_unheld().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_ios_udid;

    #[test]
    fn iphone_udids_are_told_from_adb_serials() {
        assert!(looks_like_ios_udid("00008130-001A2B3C4D5E6F70"));
        assert!(looks_like_ios_udid(
            "0123456789abcdef0123456789abcdef01234567"
        ));
        for serial in [
            "R39N402N5SB",
            "emulator-5554",
            "192.168.0.5:5555",
            "adb-R39N402N5SB-abc._adb-tls-connect._tcp",
        ] {
            assert!(!looks_like_ios_udid(serial), "{serial}");
        }
    }
}
