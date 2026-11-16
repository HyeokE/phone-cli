//! The real CLI with an empty Apple discovery list and no pairing record.
//! No phone, host service, or developer tunnel is contacted.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};

struct Fixture {
    dir: PathBuf,
    address: String,
    done: Arc<AtomicBool>,
    server: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("iph-connect-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("state")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let shutdown = done.clone();
        let server = thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                if let Ok((stream, _)) = listener.accept() {
                    let _ = serve(stream);
                } else {
                    thread::sleep(Duration::from_millis(2));
                }
            }
        });
        Self {
            dir,
            address,
            done,
            server: Some(server),
        }
    }

    fn run(&self, args: &[&str]) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_iph"))
            .args(args)
            .env("IPH_STATE_DIR", self.dir.join("state"))
            .env("IPH_CONFIG_DIR", self.dir.join("config"))
            .env("USBMUXD_SOCKET_ADDRESS", &self.address)
            .env_remove("IPH_UDID")
            .env_remove("IPH_TRACE")
            .env_remove("IPH_LOG")
            .output()
            .unwrap();
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap(), value)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.server.take().unwrap().join().unwrap();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn serve(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    loop {
        let mut header = [0; 16];
        stream.read_exact(&mut header)?;
        let n = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        assert!((16..1024 * 1024).contains(&n));
        let mut body = vec![0; n - 16];
        stream.read_exact(&mut body)?;
        let request = plist::Value::from_reader(std::io::Cursor::new(body)).unwrap();
        let kind = request
            .as_dictionary()
            .unwrap()
            .get("MessageType")
            .unwrap()
            .as_string()
            .unwrap();
        // A cached address is never enough to authenticate a target: no
        // pairing record is returned, and no Connect request is allowed.
        let response = match kind {
            "ListDevices" => json!({"DeviceList": []}),
            "ReadPairRecord" => json!({"MessageType": "Result", "Number": 2}),
            other => panic!("unexpected mux request: {other}"),
        };
        let mut payload = Vec::new();
        plist::to_value(&response)
            .unwrap()
            .to_writer_xml(&mut payload)
            .unwrap();
        stream.write_all(&((payload.len() + 16) as u32).to_le_bytes())?;
        stream.write_all(&header[4..])?;
        stream.write_all(&payload)?;
    }
}

#[test]
fn selected_cached_candidate_is_reported_without_claiming_apple_discovery_or_readiness() {
    let f = Fixture::new();
    let (_, empty) = f.run(&["--udid", "TEST-PHONE", "devices"]);
    assert_eq!(empty["count"], 0);
    std::fs::write(f.dir.join("state/TEST-PHONE.addr"), "192.0.2.1").unwrap();
    std::fs::write(f.dir.join("state/OTHER-PHONE.addr"), "192.0.2.2").unwrap();
    let (code, listed) = f.run(&["--udid", "TEST-PHONE", "devices"]);
    assert_eq!(code, 0);
    assert_eq!(listed["count"], 1);
    let d = &listed["devices"][0];
    assert_eq!(d["udid"], "TEST-PHONE");
    assert_eq!(d["source"], "cached_wifi_address");
    assert_eq!(d["listed_by_usbmuxd"], false);
    assert_eq!(d["ready"], false);
    assert_eq!(d["paired"], false);
    // No target selected: unrelated/stale caches are not contacted or
    // silently selected as the user's phone.
    let (_, unselected) = f.run(&["devices"]);
    assert_eq!(unselected["count"], 0);
}

#[test]
fn connect_requires_pairing_and_keeps_the_previous_address_on_failure() {
    let f = Fixture::new();
    let saved = f.dir.join("state/TEST-PHONE.addr");
    std::fs::write(&saved, "192.0.2.1").unwrap();
    let (code, result) = f.run(&["--udid", "TEST-PHONE", "connect", "--ip", "192.0.2.2"]);
    assert_eq!(code, 1);
    assert_eq!(result["outcome"], "not_sent");
    assert_eq!(result["error"]["code"], "not_paired");
    assert_eq!(std::fs::read_to_string(saved).unwrap(), "192.0.2.1");
}
