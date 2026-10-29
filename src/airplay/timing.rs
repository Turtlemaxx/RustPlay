//! NTP-style timing requests the client expects during a session.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

/// Binds our timing port and polls the client's timing port every few seconds.
pub async fn start(local_ip: IpAddr, remote: Option<SocketAddr>) -> std::io::Result<(u16, JoinHandle<()>)> {
    let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let port = socket.local_addr()?.port();
    let task = tokio::spawn(async move {
        let Some(remote) = remote else { return };
        let mut buf = [0u8; 128];
        let mut tick = tokio::time::interval(Duration::from_secs(3));
        loop {
            tick.tick().await;
            let mut request = [0u8; 32];
            request[..4].copy_from_slice(&[0x80, 0xd2, 0x00, 0x07]);
            request[24..32].copy_from_slice(&ntp_now().to_be_bytes());
            if socket.send_to(&request, remote).await.is_err() {
                continue;
            }
            let _ = tokio::time::timeout(Duration::from_millis(500), socket.recv_from(&mut buf)).await;
        }
    });
    Ok((port, task))
}

fn ntp_now() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let fraction = ((now.subsec_nanos() as u64) << 32) / 1_000_000_000;
    ((now.as_secs() + NTP_UNIX_OFFSET) << 32) | fraction
}
