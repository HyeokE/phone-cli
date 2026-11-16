//! Register a paired phone's direct address when usbmuxd discovery is empty.

use std::{future::Future, io::Write, net::IpAddr, path::Path, time::Duration};

use idevice::{IdeviceService, lockdown::LockdownClient, provider::IdeviceProvider};
use serde::Deserialize;

use crate::{
    client,
    device::conn,
    fields,
    i18n::t,
    paths,
    reply::{Fail, Res},
};

#[derive(Debug, Clone, clap::Args, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Args {
    /// The iPhone's IP address, shown in Settings > Wi-Fi > the network's info.
    #[arg(long)]
    pub ip: IpAddr,
}

pub(crate) struct Identity {
    pub udid: String,
    pub ios_version: Option<String>,
    pub model: Option<String>,
}

/// Read identity through a paired session, not an unauthenticated GetValue.
pub(crate) async fn identify(provider: &dyn IdeviceProvider) -> Res<Identity> {
    let result = tokio::time::timeout(Duration::from_secs(25), async {
        let mut phone = LockdownClient::connect(provider).await.map_err(|e| {
            Fail::not_sent(
                "wifi_connect_failed",
                format!("cannot reach the iPhone's lockdown service: {e:?}"),
            )
        })?;
        let pair = provider.get_pairing_file().await.map_err(|e| {
            Fail::not_sent(
                "not_paired",
                format!("cannot read the pairing record: {e:?}"),
            )
        })?;
        phone.start_session(&pair).await.map_err(|e| {
            Fail::not_sent(
                "wifi_session_failed",
                format!("cannot start a paired Wi-Fi session: {e:?}"),
            )
        })?;
        let identity = phone.get_value(Some("UniqueDeviceID"), None).await;
        let udid = identity
            .ok()
            .and_then(|v| v.into_string())
            .filter(|v| !v.is_empty());
        // Model/version are optional metadata; neither substitutes for identity.
        let version = phone
            .get_value(Some("ProductVersion"), None)
            .await
            .ok()
            .and_then(|v| v.into_string());
        let model = phone
            .get_value(Some("ProductType"), None)
            .await
            .ok()
            .and_then(|v| v.into_string());
        let _ = tokio::time::timeout(Duration::from_secs(2), phone.stop_session()).await;
        let udid = udid.ok_or_else(|| {
            Fail::not_sent(
                "wifi_identity_unavailable",
                "the paired session did not return a device UDID",
            )
        })?;
        Ok(Identity {
            udid,
            ios_version: version,
            model,
        })
    })
    .await;
    result.unwrap_or_else(|_| {
        Err(Fail::not_sent(
            "wifi_connect_failed",
            "direct Wi-Fi identity check timed out",
        ))
    })
}

fn validate(udid: &str, ip: IpAddr) -> Res<()> {
    if udid.is_empty()
        || udid.len() > 128
        || !udid.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        return Err(Fail::invalid(
            "UDID must contain only ASCII letters, digits and hyphens",
        ));
    }
    if ip.is_unspecified()
        || ip.is_multicast()
        || matches!(ip, IpAddr::V6(v) if v.is_unicast_link_local())
    {
        return Err(Fail::invalid(
            "use the iPhone's IPv4 address or a non-link-local IPv6 address",
        ));
    }
    Ok(())
}

/// Persist only after both the paired session and reported identity succeeded.
async fn register_at(
    state: &Path,
    udid: &str,
    ip: IpAddr,
    probe: impl Future<Output = Res<Identity>>,
) -> Res {
    validate(udid, ip)?;
    let identity = probe.await?;
    if !identity.udid.eq_ignore_ascii_case(udid) {
        return Err(Fail::not_sent(
            "wifi_device_mismatch",
            "the address belongs to a different iPhone",
        )
        .with("expected_udid", udid)
        .with("observed_udid", identity.udid));
    }
    let path = state.join(format!("{udid}.addr"));
    let temporary = state.join(format!("{udid}.addr.{}.tmp", uuid::Uuid::new_v4()));
    let save = (|| -> std::io::Result<()> {
        paths::ensure_private_dir(state)?;
        let mut file = paths::private_file()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(ip.to_string().as_bytes())?;
        drop(file);
        std::fs::rename(&temporary, &path)
    })();
    if let Err(e) = save {
        let _ = std::fs::remove_file(&temporary);
        return Err(Fail::not_sent(
            "connection_save_failed",
            format!("cannot save the verified address: {e}"),
        ));
    }
    Ok(fields! {
        "udid": udid, "connection": "wifi", "source": "direct_ip",
        "ip": ip.to_string(), "paired": true, "udid_verified": true, "address_saved": true,
        "ios_version": identity.ios_version, "model": identity.model,
        "hint": t("주소를 저장했습니다. 이후 같은 UDID로 read·doctor·MCP·스크립트를 실행할 수 있습니다. 개발자 서비스 준비 상태는 doctor로 확인하세요.",
                  "Address saved. Use the same UDID for read, doctor, MCP and scripts. Check developer-service readiness with doctor.")
    })
}

pub async fn run(udid: &str, args: Args) -> Res {
    let probe = async {
        let provider = conn::direct_provider(udid, args.ip).await.map_err(|e| {
            Fail::not_sent(
                "not_paired",
                format!("cannot get this phone's pairing record: {e:#}"),
            )
        })?;
        identify(&provider).await.map_err(|f| f.hint(t(
            "아이폰을 깨워 잠금을 풀고 IP 주소를 확인하세요. 처음 연결한다면 USB로 연결해 이 컴퓨터를 신뢰해야 합니다.",
            "Wake and unlock the iPhone, confirm its IP address, and pair by USB first if needed.",
        )))
    };
    let mut fields = register_at(&paths::state_dir(), udid, args.ip, probe).await?;
    // An existing daemon may still hold a tunnel to the previous address.
    fields.insert(
        "daemon_stopped".into(),
        client::stop_daemon(udid).await.is_some().into(),
    );
    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(udid: &str) -> Identity {
        Identity {
            udid: udid.into(),
            ios_version: Some("27.2.1".into()),
            model: Some("iPhone14,2".into()),
        }
    }

    #[tokio::test]
    async fn only_a_matching_paired_probe_replaces_the_saved_address() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        let udid = "TEST-PHONE";
        let old: IpAddr = "192.0.2.1".parse().unwrap();
        let new: IpAddr = "192.0.2.2".parse().unwrap();
        let saved = dir.join(format!("{udid}.addr"));
        let v = register_at(&dir, udid, old, async { Ok(identity(udid)) })
            .await
            .unwrap();
        assert_eq!(v["address_saved"], true);
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), old.to_string());

        for result in [
            Err(Fail::not_sent("wifi_session_failed", "rejected")),
            Err(Fail::not_sent("wifi_identity_unavailable", "unreadable")),
            Ok(identity("OTHER-PHONE")),
        ] {
            assert!(
                register_at(&dir, udid, new, async { result })
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&saved).unwrap(), old.to_string());
        }
        register_at(&dir, udid, new, async { Ok(identity(udid)) })
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), new.to_string());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn invalid_target_is_rejected_before_probing_or_creating_files() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        for (udid, ip) in [
            ("../escape", "192.0.2.1"),
            ("PHONE", "0.0.0.0"),
            ("PHONE", "224.0.0.251"),
            ("PHONE", "fe80::1"),
        ] {
            let result = register_at(&dir, udid, ip.parse().unwrap(), async {
                panic!("must not probe")
            })
            .await;
            assert_eq!(result.unwrap_err().code, "invalid_argument");
            assert!(!dir.exists());
        }
    }

    #[tokio::test]
    async fn failed_cache_commit_preserves_existing_data_and_removes_the_temporary_file() {
        let dir = std::env::temp_dir().join(format!("iph-direct-{}", uuid::Uuid::new_v4()));
        let destination = dir.join("TEST-PHONE.addr");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::write(destination.join("keep"), "previous data").unwrap();
        let result = register_at(&dir, "TEST-PHONE", "192.0.2.1".parse().unwrap(), async {
            Ok(identity("TEST-PHONE"))
        })
        .await;
        assert_eq!(result.unwrap_err().code, "connection_save_failed");
        assert_eq!(
            std::fs::read_to_string(destination.join("keep")).unwrap(),
            "previous data"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
