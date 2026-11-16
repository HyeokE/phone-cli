//! The resident process (`phone daemon`, started on demand by the first `phone`
//! call). It keeps the tunnel, the accessibility subscription and the input
//! session open for one phone and serves requests from a Unix socket (a named
//! pipe on Windows), one at a time.

use std::{
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};

use crate::{
    actions::parse_args,
    android::session::AndroidSession,
    config::Config,
    device::session::Session,
    doctor, events, fields, ipc, paths,
    platform::{self, Device, Phone, Platform},
    reply::{Fail, Res, envelope},
    script::{self, Scripts},
    steps::Steps,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    /// Identifies the client's binary; a daemon from an older build steps down.
    pub build: String,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Stamp of the running executable: version plus the file's size and mtime.
pub fn build_stamp() -> String {
    let meta = std::env::current_exe().and_then(std::fs::metadata);
    let detail = match meta {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("{}-{}", m.len(), mtime)
        }
        Err(_) => "unknown".into(),
    };
    format!("{}-{detail}", env!("CARGO_PKG_VERSION"))
}

fn log(msg: &str) {
    eprintln!("[{}] {msg}", crate::clock::now_utc());
}

async fn dispatch(device: &mut Device, cmd: &str, args: &Value) -> Res {
    match device {
        Device::Ios(s) => Box::pin(dispatch_ios(s, cmd, args)).await,
        Device::Android(a) => Box::pin(dispatch_android(a, cmd, args)).await,
    }
}

/// The commands on an Android phone: the same names, arguments and results,
/// plus `backend`: whether the helper or adb's tools served the command.
async fn dispatch_android(a: &mut AndroidSession, cmd: &str, args: &Value) -> Res {
    a.begin_command();
    let mut result = Box::pin(dispatch_android_command(a, cmd, args)).await;
    if let Some(backend) = a.backend() {
        let out = match &mut result {
            Ok(out) => out,
            Err(fail) => &mut fail.extra,
        };
        out.insert("backend".into(), json!(backend));
    }
    result
}

async fn dispatch_android_command(a: &mut AndroidSession, cmd: &str, args: &Value) -> Res {
    match cmd {
        "read" => Box::pin(a.cmd_read(parse_args(args)?)).await,
        "find" => Box::pin(a.cmd_find(parse_args(args)?)).await,
        "tap" => Box::pin(a.cmd_tap(parse_args(args)?)).await,
        "swipe" => Box::pin(a.cmd_swipe(parse_args(args)?)).await,
        "scroll" => Box::pin(a.cmd_scroll(parse_args(args)?)).await,
        "type" => Box::pin(a.cmd_type(parse_args(args)?)).await,
        "key" => Box::pin(a.cmd_key(parse_args(args)?)).await,
        "button" => Box::pin(a.cmd_button(parse_args(args)?)).await,
        "launch" => Box::pin(a.cmd_launch(parse_args(args)?)).await,
        "apps" => Box::pin(a.cmd_apps()).await,
        "screenshot" => Box::pin(a.cmd_screenshot(parse_args(args)?)).await,
        "batch" => {
            let steps = args
                .get("steps")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Fail::invalid("batch needs a JSON array of steps"))?;
            Box::pin(a.run_steps(steps, &Map::new())).await
        }
        "script" => Box::pin(a.serve_script(args)).await,
        "doctor" => Box::pin(crate::android::doctor::run(a, parse_args(args)?)).await,
        "dnd" => a.cmd_dnd(args).await,
        "status" => Ok(fields! {
            "udid": a.serial.clone(),
            "platform": "android",
            "connection": a.connection(),
            "helper": a.helper_status(),
            "pid": std::process::id(),
            "build": build_stamp(),
        }),
        other => Err(Fail::invalid(format!("unknown command `{other}`"))),
    }
}

async fn dispatch_ios(s: &mut Session, cmd: &str, args: &Value) -> Res {
    s.refresh_connection();
    match cmd {
        "read" => Box::pin(s.cmd_read(parse_args(args)?)).await,
        "find" => Box::pin(s.cmd_find(parse_args(args)?)).await,
        "tap" => Box::pin(s.cmd_tap(parse_args(args)?)).await,
        "swipe" => Box::pin(s.cmd_swipe(parse_args(args)?)).await,
        "scroll" => Box::pin(s.cmd_scroll(parse_args(args)?)).await,
        "type" => Box::pin(s.cmd_type(parse_args(args)?)).await,
        "key" => Box::pin(s.cmd_key(parse_args(args)?)).await,
        "button" => Box::pin(s.cmd_button(parse_args(args)?)).await,
        "launch" => Box::pin(s.cmd_launch(parse_args(args)?)).await,
        "apps" => s.cmd_apps().await,
        "screenshot" => Box::pin(s.cmd_screenshot(parse_args(args)?)).await,
        "batch" => {
            let steps = args
                .get("steps")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Fail::invalid("batch needs a JSON array of steps"))?;
            Box::pin(s.run_steps(steps, &Map::new())).await
        }
        // `phone script` and `phone run`: whoever asks sends the code along.
        "script" => Box::pin(s.serve_script(args)).await,
        "doctor" => Box::pin(doctor::run(s, parse_args(args)?)).await,
        "dnd" => Box::pin(s.cmd_dnd(parse_args(args)?)).await,
        "status" => Ok(fields! {
            "udid": s.udid.clone(),
            "connection": s.connection(),
            "tunnel": s.tunnel.is_some(),
            "accessibility": s.ax.as_ref().is_some_and(|a| !a.is_closed()),
            "input_session": s.input.is_some(),
            "connects": s.connects,
            "ax_restarts": s.ax_restarts,
            "pid": std::process::id(),
            "build": build_stamp(),
        }),
        // For testing recovery: close the tunnel under the open connections,
        // as a pulled cable or a Wi-Fi drop would.
        "debug_drop_tunnel" if crate::paths::env_var("PHONE_DEBUG").is_ok_and(|v| v == "1") => {
            let closed = match s.tunnel.as_mut() {
                Some(t) => t.adapter.close().await.is_ok(),
                None => false,
            };
            Ok(fields! { "closed": closed })
        }
        other => Err(Fail::invalid(format!("unknown command `{other}`"))),
    }
}

/// How long one request may run before the daemon gives up on it.
pub fn budget(cmd: &str) -> Duration {
    match cmd {
        // A script stops itself first, at [script] timeout_secs.
        "script" => Duration::from_secs(script::MAX_TIMEOUT_SECS + 60),
        "batch" => Duration::from_secs(600),
        "doctor" => Duration::from_secs(120),
        _ => Duration::from_secs(90),
    }
}

struct Shared {
    session: Mutex<Device>,
    last_request: StdMutex<Instant>,
    build: String,
    udid: String,
}

async fn handle(stream: ipc::Stream, shared: Arc<Shared>) -> Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    if line.trim().is_empty() {
        return Ok(());
    }
    let started = Instant::now();
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            let out = envelope(Err(Fail::invalid(format!("bad request: {e}"))), started);
            write.write_all(format!("{out}\n").as_bytes()).await?;
            return Ok(());
        }
    };
    *shared.last_request.lock().unwrap() = Instant::now();

    let reply = match req.cmd.as_str() {
        "ping" => {
            json!({ "ok": true, "build": shared.build, "pid": std::process::id(), "udid": shared.udid })
        }
        "shutdown" => {
            log("shutdown requested");
            let mut session = shared.session.lock().await;
            let reply = json!({ "ok": true });
            write.write_all(format!("{reply}\n").as_bytes()).await?;
            write.shutdown().await.ok();
            session.shutdown().await;
            ipc::cleanup(&shared.udid);
            let _ = std::fs::remove_file(paths::daemon_pid(&shared.udid));
            std::process::exit(0);
        }
        _ if req.build != shared.build => envelope(
            Err(Fail::not_sent(
                "daemon_outdated",
                "the daemon runs an older build",
            )),
            started,
        ),
        cmd => {
            events::record(
                "daemon.command",
                "started",
                0,
                json!({ "udid": shared.udid, "command": cmd }),
            );
            let mut session = shared.session.lock().await;
            let work = Box::pin(dispatch(&mut session, cmd, &req.args));
            let result = match tokio::time::timeout(budget(cmd), work).await {
                Ok(r) => r,
                Err(_) => {
                    // Whatever was in flight is abandoned with its connections.
                    session.reset();
                    Err(Fail::unknown(
                        "timeout",
                        format!("`{cmd}` did not finish within {} s", budget(cmd).as_secs()),
                    )
                    .hint(crate::i18n::t(
                        "입력이 일부 나갔을 수 있습니다. `phone read`로 화면을 확인하세요.",
                        "Some input may have gone out. Check the screen with `phone read`.",
                    )))
                }
            };
            if let Err(f) = &result {
                log(&format!("{cmd} -> {} {}", f.outcome.as_str(), f.code));
            }
            events::record(
                "daemon.command",
                if result.is_ok() { "ok" } else { "error" },
                started.elapsed().as_millis(),
                json!({ "udid": shared.udid, "command": cmd, "error": result.as_ref().err().map(|f| f.error_json()) }),
            );
            let reply = envelope(result, started);
            session.close_input_if_unheld().await;
            session.sweep().await;
            reply
        }
    };
    *shared.last_request.lock().unwrap() = Instant::now();
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    write.shutdown().await.ok();
    Ok(())
}

/// The tunnel's TCP stack and the device library say why a connection went
/// away ("timed out after 5 retransmissions", "RST on hp=…") as `tracing`
/// warnings. Without a subscriber they are lost and all phone-cli sees later is
/// "not connected". They carry ports and byte counts, never screen contents.
/// `PHONE_LOG` takes a filter such as `phone_cli::tcp=debug` for more.
fn keep_library_warnings() {
    events::init_tracing();
}

pub async fn run(udid: String) -> Result<()> {
    keep_library_warnings();
    paths::ensure_private_dir(&paths::state_dir())?;
    if ipc::connect(&udid).await.is_ok() {
        bail!("a daemon for {udid} is already running");
    }
    let mut listener = ipc::Listener::bind(&udid)?;
    // So a client can put down a daemon that stopped answering.
    std::fs::write(paths::daemon_pid(&udid), std::process::id().to_string())?;

    let config = Config::load()?;
    let idle_exit = Duration::from_secs(config.daemon.idle_exit_secs.max(30));
    let kind = platform::detect(&udid).await;
    let device = match kind {
        Platform::Ios => Device::Ios(Box::new(Session::new(udid.clone(), config))),
        Platform::Android => Device::Android(Box::new(AndroidSession::new(udid.clone(), config))),
    };
    let shared = Arc::new(Shared {
        session: Mutex::new(device),
        last_request: StdMutex::new(Instant::now()),
        build: build_stamp(),
        udid: udid.clone(),
    });
    log(&format!(
        "daemon for {udid} ({}) listening, build {}",
        kind.as_str(),
        shared.build
    ));

    // Requests run one at a time, so everything stays on this thread: the
    // device futures need not be `Send`.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            // Housekeeping: close an idle input session, exit when unused.
            let keeper = shared.clone();
            tokio::task::spawn_local(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let idle = keeper.last_request.lock().unwrap().elapsed();
                    if let Ok(mut session) = keeper.session.try_lock() {
                        session.sweep().await;
                        session.close_idle_input().await;
                        if idle > idle_exit {
                            log("idle, exiting");
                            session.shutdown().await;
                            ipc::cleanup(&keeper.udid);
                            let _ = std::fs::remove_file(paths::daemon_pid(&keeper.udid));
                            std::process::exit(0);
                        }
                    }
                }
            });

            loop {
                let stream = listener.accept().await?;
                let shared = shared.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = handle(stream, shared).await {
                        log(&format!("connection error: {e:#}"));
                    }
                });
            }
        })
        .await
}
