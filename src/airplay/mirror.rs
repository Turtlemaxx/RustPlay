//! The screen-mirroring data connection (stream type 110): a TCP stream of
//! 128-byte headers, each followed by an H.264 payload.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use sha2::{Digest, Sha512};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::hub::Hub;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

const HEADER_LEN: usize = 128;
const MAX_PAYLOAD: usize = 32 * 1024 * 1024;

/// Payload types in header byte 4.
const PAYLOAD_VIDEO: u8 = 0x00;
const PAYLOAD_CODEC: u8 = 0x01;

/// Opens the data port and returns it; the stream is handled in the background.
pub async fn listen(
    local_ip: IpAddr,
    aeskey: [u8; 16],
    stream_id: u64,
    hub: Arc<Hub>,
    session: u64,
) -> std::io::Result<(u16, JoinHandle<()>)> {
    let listener = TcpListener::bind(SocketAddr::new(local_ip, 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move {
        let mut state = StreamState::new(stream_cipher(&aeskey, stream_id));
        // The client can drop the data connection (e.g. while its screen is locked)
        // and reconnect to the same port later, so keep accepting for the session.
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    log::info!("mirror stream connected from {peer}");
                    if let Err(err) = state.receive(stream, &hub, session).await {
                        log::info!("mirror stream disconnected: {err}");
                    }
                    state.reconnected = true;
                }
                Err(err) => {
                    log::warn!("mirror accept failed: {err}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    });
    Ok((port, task))
}

struct StreamState {
    cipher: Aes128Ctr,
    /// The keystream as it was at SETUP, for clients that restart it on reconnect.
    initial: Aes128Ctr,
    reconnected: bool,
    paused: bool,
}

fn stream_cipher(aeskey: &[u8; 16], stream_id: u64) -> Aes128Ctr {
    let hash = |label: &str| -> [u8; 16] {
        let digest =
            Sha512::new().chain_update(format!("{label}{stream_id}").as_bytes()).chain_update(aeskey).finalize();
        digest[..16].try_into().unwrap()
    };
    Aes128Ctr::new(&hash("AirPlayStreamKey").into(), &hash("AirPlayStreamIV").into())
}

impl StreamState {
    fn new(cipher: Aes128Ctr) -> Self {
        StreamState { initial: cipher.clone(), cipher, reconnected: false, paused: false }
    }

    async fn receive(&mut self, mut stream: TcpStream, hub: &Hub, session: u64) -> std::io::Result<()> {
        stream.set_nodelay(true)?;
        let mut header = [0u8; HEADER_LEN];
        loop {
            stream.read_exact(&mut header).await?;
            let size = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
            if size > MAX_PAYLOAD {
                return Err(std::io::Error::other(format!("payload of {size} bytes is too large")));
            }
            // Two leading bytes are reserved for the UI message tag (see Hub::frame).
            let mut frame = vec![0u8; 2 + size];
            stream.read_exact(&mut frame[2..]).await?;

            match header[4] {
                PAYLOAD_VIDEO => {
                    // Decrypt every packet, even ones we drop, to stay in step with the keystream.
                    self.decrypt(&mut frame[2..]);
                    match scan_access_unit(&frame[2..]) {
                        Some(keyframe) => {
                            // Video flowing again means the client woke up, even if it
                            // didn't send a fresh codec packet first.
                            if self.paused {
                                self.set_paused(false, hub, session);
                            }
                            hub.frame(session, keyframe, frame);
                        }
                        None => log::debug!("dropping malformed video packet ({size} bytes)"),
                    }
                }
                PAYLOAD_CODEC => {
                    // 0x56 marks the client going to sleep (screen locked), 0x16 waking up.
                    let paused = header[6] == 0x56 || header[6] == 0x5e;
                    let width = f32::from_le_bytes(header[56..60].try_into().unwrap()) as u16;
                    let height = f32::from_le_bytes(header[60..64].try_into().unwrap()) as u16;
                    match avc_config(&frame[2..]) {
                        Some(avcc) => {
                            log::info!("video format {width}x{height}");
                            hub.codec(session, width, height, &avcc, paused);
                        }
                        None => log::warn!("unreadable codec packet ({size} bytes)"),
                    }
                    if paused != self.paused {
                        self.set_paused(paused, hub, session);
                    }
                }
                other => log::trace!("ignoring mirror packet type {other:#04x}"),
            }
        }
    }

    fn decrypt(&mut self, payload: &mut [u8]) {
        if std::mem::take(&mut self.reconnected) {
            // After a reconnect the client either continues the keystream or
            // restarts it; keep whichever one yields a well-formed access unit.
            let mut restarted = self.initial.clone();
            let mut trial = payload.to_vec();
            restarted.apply_keystream(&mut trial);
            if scan_access_unit(&trial).is_some() {
                log::info!("mirror keystream restarted after reconnect");
                self.cipher = restarted;
                payload.copy_from_slice(&trial);
                return;
            }
        }
        self.cipher.apply_keystream(payload);
    }

    fn set_paused(&mut self, paused: bool, hub: &Hub, session: u64) {
        log::info!("mirror stream {}", if paused { "paused" } else { "resumed" });
        self.paused = paused;
        hub.set_paused(session, paused);
    }
}

/// Walks the length-prefixed NAL units; returns whether the unit holds an IDR
/// slice, or None if the lengths don't add up (a decryption failure).
fn scan_access_unit(data: &[u8]) -> Option<bool> {
    let mut pos = 0;
    let mut keyframe = false;
    while pos < data.len() {
        let len = u32::from_be_bytes(data.get(pos..pos + 4)?.try_into().ok()?) as usize;
        let nal = data.get(pos + 4..pos + 4 + len)?;
        let first = *nal.first()?;
        if first & 0x80 != 0 {
            return None;
        }
        keyframe |= first & 0x1f == 5;
        pos += 4 + len;
    }
    Some(keyframe)
}

/// The codec packet carries one SPS and one PPS; rebuild a clean
/// AVCDecoderConfigurationRecord from them for the decoder.
fn avc_config(payload: &[u8]) -> Option<Vec<u8>> {
    let sps_len = u16::from_be_bytes(payload.get(6..8)?.try_into().ok()?) as usize;
    let sps = payload.get(8..8 + sps_len)?;
    let pps_len = u16::from_be_bytes(payload.get(8 + sps_len + 1..8 + sps_len + 3)?.try_into().ok()?) as usize;
    let pps = payload.get(8 + sps_len + 3..8 + sps_len + 3 + pps_len)?;
    if sps.len() < 4 {
        return None;
    }
    let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
    avcc.extend_from_slice(&(sps_len as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1);
    avcc.extend_from_slice(&(pps_len as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    Some(avcc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_idr_access_unit() {
        let data = [0, 0, 0, 2, 0x06, 0xaa, 0, 0, 0, 3, 0x65, 0x88, 0x80];
        assert_eq!(scan_access_unit(&data), Some(true));
        assert_eq!(scan_access_unit(&[0, 0, 0, 9, 0x41]), None);
    }

    #[test]
    fn builds_avcc_from_codec_packet() {
        let sps = [0x67, 0x64, 0x00, 0x28, 0xac];
        let pps = [0x68, 0xee, 0x3c];
        let mut payload = vec![1, 0x64, 0x00, 0x28, 0xff, 0xe1, 0, sps.len() as u8];
        payload.extend_from_slice(&sps);
        payload.extend_from_slice(&[1, 0, pps.len() as u8]);
        payload.extend_from_slice(&pps);
        let avcc = avc_config(&payload).unwrap();
        assert_eq!(&avcc[..8], &[1, 0x64, 0x00, 0x28, 0xff, 0xe1, 0, 5]);
        assert_eq!(&avcc[avcc.len() - 6..], &[1, 0, 3, 0x68, 0xee, 0x3c]);
    }
}
