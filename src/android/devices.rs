//! Android entries for `phone devices` and for picking the only phone.

use serde_json::{Value, json};

use super::adb::{self, Adb, AdbDevice};

/// What one adb device says about itself (read with `getprop`, no input).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props {
    pub serial_number: Option<String>,
    pub model: Option<String>,
    pub release: Option<String>,
    pub sdk: Option<String>,
}

pub fn parse_props(out: &str) -> Props {
    let mut lines = out
        .lines()
        .map(|l| l.trim())
        .map(|l| (!l.is_empty()).then(|| l.to_string()));
    Props {
        serial_number: lines.next().flatten(),
        model: lines.next().flatten(),
        release: lines.next().flatten(),
        sdk: lines.next().flatten(),
    }
}

async fn props(serial: &str) -> Option<Props> {
    let out = Adb::new(serial)
        .shell("getprop ro.serialno; getprop ro.product.model; getprop ro.build.version.release; getprop ro.build.version.sdk", 8)
        .await
        .ok()?;
    Some(parse_props(&out))
}

/// One `phone devices` entry.
pub fn entry(d: &AdbDevice, p: Option<&Props>, selected: bool) -> Value {
    let ready = d.state == "device" && p.is_some();
    let state = if !selected {
        "not selected"
    } else if d.state == "device" {
        if ready { "ready" } else { "unreachable" }
    } else if d.state == "unauthorized" {
        "unauthorized"
    } else {
        "offline"
    };
    json!({
        "udid": d.serial,
        "platform": "android",
        "connection": d.connection,
        "transport": "adb",
        "source": "adb",
        "adb_state": d.state,
        "model": p.and_then(|p| p.model.clone()).or_else(|| d.model.clone()),
        "android_version": p.and_then(|p| p.release.clone()),
        "sdk": p.and_then(|p| p.sdk.clone()),
        "serial_number": p.and_then(|p| p.serial_number.clone()),
        "ready": ready && selected,
        "state": state,
    })
}

/// Mark entries that are the same phone reached two ways (USB and Wi-Fi
/// show one phone under two serials; `ro.serialno` is the phone's own).
pub fn mark_same_device(entries: &mut [Value]) {
    let keys: Vec<Option<String>> = entries
        .iter()
        .map(|e| e["serial_number"].as_str().map(String::from))
        .collect();
    for i in 0..entries.len() {
        let Some(k) = &keys[i] else { continue };
        let others: Vec<Value> = (0..entries.len())
            .filter(|j| *j != i && keys[*j].as_ref() == Some(k))
            .map(|j| entries[j]["udid"].clone())
            .collect();
        if !others.is_empty() {
            entries[i]["same_device_as"] = Value::Array(others);
        }
    }
}

/// Entries for every adb device. Unselected devices are still asked for
/// their properties: `getprop` is read-only and tells duplicates apart.
pub async fn entries(chosen: Option<&str>) -> (Vec<Value>, Option<String>) {
    let list = match adb::list_devices().await {
        Ok(l) => l,
        Err(e) => return (Vec::new(), Some(e.message())),
    };
    let mut out = Vec::new();
    for d in &list {
        let p = if d.state == "device" {
            props(&d.serial).await
        } else {
            None
        };
        out.push(entry(d, p.as_ref(), chosen.is_none_or(|c| c == d.serial)));
    }
    mark_same_device(&mut out);
    (out, None)
}

/// Serials of ready phones, one per phone: when USB and Wi-Fi reach the
/// same phone, the USB serial.
pub async fn usable() -> Vec<AdbDevice> {
    let Ok(list) = adb::list_devices().await else {
        return Vec::new();
    };
    let mut picked: Vec<(AdbDevice, Option<String>)> = Vec::new();
    for d in list.into_iter().filter(|d| d.state == "device") {
        let sn = props(&d.serial).await.and_then(|p| p.serial_number);
        if let Some(i) = picked.iter().position(|(_, s)| sn.is_some() && *s == sn) {
            if picked[i].0.connection != "usb" && d.connection == "usb" {
                picked[i] = (d, sn);
            }
            continue;
        }
        picked.push((d, sn));
    }
    picked.into_iter().map(|(d, _)| d).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(serial: &str, state: &str, connection: &str) -> AdbDevice {
        AdbDevice {
            serial: serial.into(),
            state: state.into(),
            connection: connection.into(),
            model: Some("SM_F700N".into()),
            product: None,
            device: None,
            transport_id: None,
        }
    }

    #[test]
    fn props_come_one_per_line() {
        let p = parse_props("R39N402N5SB\nSM-F700N\n13\n33\n");
        assert_eq!(p.serial_number.as_deref(), Some("R39N402N5SB"));
        assert_eq!(p.model.as_deref(), Some("SM-F700N"));
        assert_eq!(p.release.as_deref(), Some("13"));
        assert_eq!(p.sdk.as_deref(), Some("33"));
        assert_eq!(parse_props("\n\n"), Props::default());
    }

    #[test]
    fn one_phone_over_usb_and_wifi_is_marked_as_the_same() {
        let p = parse_props("R39N\nSM-F700N\n13\n33\n");
        let mut list = vec![
            entry(&dev("R39N", "device", "usb"), Some(&p), true),
            entry(&dev("192.168.0.7:5555", "device", "wifi"), Some(&p), false),
            entry(&dev("OTHER", "unauthorized", "usb"), None, true),
        ];
        mark_same_device(&mut list);
        assert_eq!(list[0]["platform"], "android");
        assert_eq!(list[0]["state"], "ready");
        assert_eq!(list[0]["ready"], true);
        assert_eq!(list[0]["same_device_as"], json!(["192.168.0.7:5555"]));
        assert_eq!(list[1]["same_device_as"], json!(["R39N"]));
        assert_eq!(list[1]["state"], "not selected");
        assert_eq!(list[1]["connection"], "wifi");
        assert_eq!(list[2]["state"], "unauthorized");
        assert_eq!(list[2]["ready"], false);
        assert!(list[2].get("same_device_as").is_none());
    }
}
