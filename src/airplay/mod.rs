//! AirPlay screen-mirroring receiver (legacy AirPlay protocol, as UxPlay speaks it).

pub mod audio;
mod fairplay;
mod fairplay_tables;
pub mod hub;
mod mdns;
mod mirror;
mod pairing;
mod rtsp;
mod timing;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, RwLock};

use sha2::{Digest, Sha512};
use tokio::net::TcpListener;

use hub::Hub;
use pairing::Identity;

/// Who we are on the network; the display name lives separately so it can change.
pub struct Receiver {
    pub device_id: [u8; 6],
    pub public_key: [u8; 32],
    hostname: String,
}

impl Receiver {
    pub fn device_id_string(&self) -> String {
        self.device_id.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
    }

    fn host_label(&self) -> String {
        let label: String = self.hostname.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        format!("{}-rustplay", label.trim_matches('-'))
    }
}

/// The running receiver; dropping it withdraws the Bonjour advertisement.
pub struct Running {
    mdns: mdns_sd::ServiceDaemon,
    ctx: Arc<rtsp::Context>,
    address: Option<IpAddr>,
    port: u16,
    services: Mutex<Vec<String>>,
}

impl Running {
    /// Re-advertises under a new name; the iPhone's list picks it up within seconds.
    pub fn rename(&self, name: &str) -> anyhow::Result<()> {
        let mut services = self.services.lock().unwrap();
        for service in services.drain(..) {
            let _ = self.mdns.unregister(&service);
        }
        *self.ctx.name.write().unwrap() = name.to_string();
        *services = mdns::register(&self.mdns, &self.ctx.receiver, name, self.address, self.port)?;
        log::info!("now advertising as \"{name}\"");
        Ok(())
    }
}

pub fn random_token() -> [u8; 16] {
    pairing::random_bytes()
}

pub fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

/// The LAN address of the default route (no packet is sent).
pub fn primary_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(8, 8, 8, 8), 80)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() && !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

/// `instance` keeps the device ID stable across renames; only a second copy of
/// the app (see RUSTPLAY_NAME in main.rs) uses a different one.
pub async fn start(
    name: String,
    instance: &str,
    address: Option<Ipv4Addr>,
    hub: Arc<Hub>,
    output: Arc<audio::Output>,
) -> anyhow::Result<Running> {
    let hostname = computer_name();
    // A stable, locally administered MAC-style ID so the iPhone recognizes us across launches.
    let digest = Sha512::digest(format!("rustplay:{hostname}:{instance}"));
    let mut device_id: [u8; 6] = digest[..6].try_into().unwrap();
    device_id[0] = (device_id[0] | 0x02) & 0xfe;

    let identity = Identity::generate();
    let receiver = Receiver { device_id, public_key: identity.public_key(), hostname };

    let listener = bind_control_port().await?;
    let port = listener.local_addr()?.port();
    let address = address.map(IpAddr::V4);
    let mdns = mdns::daemon()?;
    let services = mdns::register(&mdns, &receiver, &name, address, port)?;
    log::info!("advertising \"{name}\" on port {port}");

    let ctx = Arc::new(rtsp::Context { identity, receiver, name: RwLock::new(name), hub, output });
    tokio::spawn(rtsp::serve(listener, ctx.clone()));
    Ok(Running { mdns, ctx, address, port, services: Mutex::new(services) })
}

async fn bind_control_port() -> std::io::Result<TcpListener> {
    for port in [7000, 7100, 7200] {
        if let Ok(listener) = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).await {
            return Ok(listener);
        }
    }
    TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))).await
}
