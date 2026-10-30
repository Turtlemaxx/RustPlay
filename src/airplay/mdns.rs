//! Bonjour advertisement: `_airplay._tcp` and `_raop._tcp` make the receiver
//! show up under Screen Mirroring on the iPhone.

use std::net::IpAddr;

use mdns_sd::{IfKind, ServiceDaemon, ServiceInfo};

use super::Receiver;

pub const MODEL: &str = "AppleTV3,2";
pub const SOURCE_VERSION: &str = "220.68";
pub const FEATURES: u64 = 0x5A7F_FEE6;
pub const PI: &str = "2e388006-13ba-4041-9a67-25dd4a43d536";

/// Asking for a password goes together with turning off legacy pairing (bit 27).
pub fn features(code_required: bool) -> u64 {
    if code_required { FEATURES & !(1 << 27) } else { FEATURES }
}

fn features_txt(code_required: bool) -> String {
    format!("0x{:X},0x0", features(code_required))
}

pub fn airplay_txt(receiver: &Receiver, code_required: bool) -> Vec<(&'static str, String)> {
    vec![
        ("deviceid", receiver.device_id_string()),
        ("features", features_txt(code_required)),
        ("pw", code_required.to_string()),
        ("flags", "0x4".into()),
        ("model", MODEL.into()),
        ("pk", hex::encode(receiver.public_key)),
        ("pi", PI.into()),
        ("srcvers", SOURCE_VERSION.into()),
        ("vv", "2".into()),
    ]
}

fn raop_txt(receiver: &Receiver, code_required: bool) -> Vec<(&'static str, String)> {
    vec![
        ("ch", "2".into()),
        ("cn", "0,1,2,3".into()),
        ("da", "true".into()),
        ("et", "0,3,5".into()),
        ("vv", "2".into()),
        ("ft", features_txt(code_required)),
        ("am", MODEL.into()),
        ("md", "0,1,2".into()),
        ("rhd", "5.6.0.0".into()),
        ("pw", code_required.to_string()),
        ("sr", "44100".into()),
        ("ss", "16".into()),
        ("sv", "false".into()),
        ("tp", "UDP".into()),
        ("txtvers", "1".into()),
        ("sf", if code_required { "0x84" } else { "0x4" }.into()),
        ("vs", SOURCE_VERSION.into()),
        ("vn", "65537".into()),
        ("pk", hex::encode(receiver.public_key)),
    ]
}

/// TXT records in DNS wire format, which /info echoes back to the client.
pub fn txt_wire(entries: &[(&'static str, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (key, value) in entries {
        let entry = format!("{key}={value}");
        out.push(entry.len() as u8);
        out.extend_from_slice(entry.as_bytes());
    }
    out
}

pub fn daemon() -> anyhow::Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new()?;
    // The servers listen on IPv4; keep the iPhone from trying IPv6 addresses first.
    daemon.disable_interface(IfKind::IPv6)?;
    Ok(daemon)
}

/// Registers both services under `name`; returns their full names for unregistering.
pub fn register(
    daemon: &ServiceDaemon,
    receiver: &Receiver,
    name: &str,
    address: Option<IpAddr>,
    port: u16,
    code_required: bool,
) -> anyhow::Result<Vec<String>> {
    let host = format!("{}.local.", receiver.host_label());
    let raop_name = format!("{}@{name}", hex::encode_upper(receiver.device_id));
    let mut registered = Vec::new();
    for (service, instance, txt) in [
        ("_airplay._tcp.local.", name.to_string(), airplay_txt(receiver, code_required)),
        ("_raop._tcp.local.", raop_name, raop_txt(receiver, code_required)),
    ] {
        let props: Vec<(&str, &str)> = txt.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let info = match address {
            Some(ip) => ServiceInfo::new(service, &instance, &host, ip, port, &props[..])?,
            None => ServiceInfo::new(service, &instance, &host, "", port, &props[..])?.enable_addr_auto(),
        };
        registered.push(info.get_fullname().to_string());
        daemon.register(info)?;
    }
    Ok(registered)
}
