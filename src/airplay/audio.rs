//! Mirroring audio (stream type 96): RTP over UDP, each payload AES-128-CBC
//! encrypted (whole blocks only, IV reset per packet), carrying AAC-ELD frames
//! that fdk-aac decodes and cpal plays on the default output device.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::Aes128;
use cbc::cipher::block_padding::NoPadding;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use fdk_aac::dec::{Decoder, Transport};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

const SAMPLE_RATE: u32 = 44_100;
const CHANNELS: usize = 2;
const SAMPLES_PER_SECOND: usize = SAMPLE_RATE as usize * CHANNELS;
/// Audio buffered before playback starts, absorbing network jitter.
const PRIME: usize = SAMPLES_PER_SECOND * 70 / 1000;
/// Above this the queue is trimmed back, so clock drift can't build up delay.
const MAX_QUEUE: usize = SAMPLES_PER_SECOND * 300 / 1000;

/// AudioSpecificConfig for the `ct` codes the client may announce.
fn audio_specific_config(ct: u64) -> Option<&'static [u8]> {
    match ct {
        8 => Some(&[0xf8, 0xe8, 0x50, 0x00]), // AAC-ELD, 44.1 kHz stereo, 480 samples per frame
        4 => Some(&[0x12, 0x10]),             // AAC-LC, 44.1 kHz stereo
        _ => None,
    }
}

/// The app's audio output: a queue of decoded samples and the two volume
/// controls (the slider in the window and the client's own volume).
pub struct Output {
    queue: Mutex<Queue>,
    volume: AtomicU32,
    client_gain: AtomicU32,
}

struct Queue {
    samples: VecDeque<f32>,
    playing: bool,
    gain: f32,
}

impl Output {
    /// Starts the playback thread; it reopens the device if it goes away.
    pub fn start(volume: f32) -> Arc<Output> {
        let output = Arc::new(Output {
            queue: Mutex::new(Queue { samples: VecDeque::new(), playing: false, gain: 0.0 }),
            volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
            client_gain: AtomicU32::new(1.0f32.to_bits()),
        });
        let shared = output.clone();
        std::thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || shared.run_device())
            .expect("audio thread");
        output
    }

    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// The client reports volume in dB: -30..0, or -144 for muted.
    pub fn set_client_volume_db(&self, db: f32) {
        let gain = if db <= -30.0 { 0.0 } else { 10f32.powf(db.min(0.0) / 20.0) };
        self.client_gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    pub fn flush(&self) {
        let mut queue = self.queue.lock().unwrap();
        queue.samples.clear();
        queue.playing = false;
    }

    fn push(&self, samples: &[f32]) {
        let mut queue = self.queue.lock().unwrap();
        queue.samples.extend(samples);
        if queue.samples.len() > MAX_QUEUE {
            let excess = queue.samples.len() - PRIME;
            queue.samples.drain(..excess - excess % CHANNELS);
        }
    }

    fn target_gain(&self) -> f32 {
        // A squared curve makes the slider feel even across its range.
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        volume * volume * f32::from_bits(self.client_gain.load(Ordering::Relaxed))
    }

    fn fill(&self, out: &mut [f32]) {
        let target = self.target_gain();
        let mut queue = self.queue.lock().unwrap();
        if !queue.playing && queue.samples.len() >= PRIME {
            queue.playing = true;
        }
        if !queue.playing {
            out.fill(0.0);
            return;
        }
        for sample in out.iter_mut() {
            // Glide toward the target gain so slider moves don't click.
            queue.gain += (target - queue.gain) * 0.002;
            match queue.samples.pop_front() {
                Some(value) => *sample = value * queue.gain,
                None => {
                    *sample = 0.0;
                    queue.playing = false;
                }
            }
        }
    }

    fn run_device(self: Arc<Self>) {
        loop {
            let (failed_tx, failed_rx) = std::sync::mpsc::channel();
            match self.open_stream(failed_tx) {
                Ok(stream) => {
                    log::info!("audio output ready");
                    let _ = failed_rx.recv();
                    drop(stream);
                    log::warn!("audio output lost, reopening");
                }
                Err(err) => log::warn!("no audio output: {err}"),
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn open_stream(self: &Arc<Self>, failed: std::sync::mpsc::Sender<()>) -> anyhow::Result<cpal::Stream> {
        let device = cpal::default_host().default_output_device().ok_or_else(|| anyhow::anyhow!("no output device"))?;
        // WASAPI converts 44.1 kHz stereo to whatever the device runs at.
        let config = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: SAMPLE_RATE,
            buffer_size: cpal::BufferSize::Default,
        };
        let output = self.clone();
        let stream = device.build_output_stream(
            config,
            move |data: &mut [f32], _| output.fill(data),
            move |err| {
                log::warn!("audio stream error: {err}");
                let _ = failed.send(());
            },
            None,
        )?;
        stream.play()?;
        Ok(stream)
    }
}

/// Opens the audio data and control ports for one session.
pub async fn listen(
    local_ip: IpAddr,
    aeskey: [u8; 16],
    aesiv: [u8; 16],
    ct: u64,
    output: Arc<Output>,
) -> std::io::Result<(u16, u16, JoinHandle<()>)> {
    let data = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let control = UdpSocket::bind(SocketAddr::new(local_ip, 0)).await?;
    let ports = (data.local_addr()?.port(), control.local_addr()?.port());
    let task = tokio::spawn(async move {
        let mut stream = match AudioStream::new(aeskey, aesiv, ct) {
            Some(stream) => Some(stream),
            None => {
                log::warn!("audio format ct={ct} isn't supported; mirroring without sound");
                None
            }
        };
        let mut packet = [0u8; 2048];
        let mut ignored = [0u8; 2048];
        loop {
            tokio::select! {
                received = data.recv_from(&mut packet) => {
                    let Ok((len, _)) = received else { break };
                    if let Some(samples) = stream.as_mut().and_then(|s| s.handle(&packet[..len])) {
                        output.push(&samples);
                    }
                }
                // Sync and retransmit packets; playback runs on arrival order.
                received = control.recv_from(&mut ignored) => if received.is_err() { break },
            }
        }
    });
    Ok((ports.0, ports.1, task))
}

struct AudioStream {
    key: [u8; 16],
    iv: [u8; 16],
    decoder: Decoder,
    pcm: Vec<i16>,
    last_seq: Option<u16>,
    errors: u32,
}

impl AudioStream {
    fn new(key: [u8; 16], iv: [u8; 16], ct: u64) -> Option<Self> {
        Self::with_config(key, iv, audio_specific_config(ct)?)
    }

    fn with_config(key: [u8; 16], iv: [u8; 16], asc: &[u8]) -> Option<Self> {
        let mut decoder = Decoder::new(Transport::Raw);
        decoder.config_raw(asc).ok()?;
        Some(AudioStream { key, iv, decoder, pcm: vec![0; 8192], last_seq: None, errors: 0 })
    }

    /// One RTP packet in, interleaved stereo samples out.
    fn handle(&mut self, packet: &[u8]) -> Option<Vec<f32>> {
        if packet.len() <= 12 || packet[1] & 0x7f != 0x60 {
            return None;
        }
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        if let Some(last) = self.last_seq {
            // Drop duplicates and stragglers that arrive after newer packets.
            let ahead = seq.wrapping_sub(last);
            if ahead == 0 || ahead > 0x8000 {
                return None;
            }
        }
        self.last_seq = Some(seq);

        let mut payload = packet[12..].to_vec();
        let whole = payload.len() / 16 * 16;
        Aes128CbcDec::new(&self.key.into(), &self.iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut payload[..whole])
            .ok()?;

        let decoded = self.decoder.fill(&payload).and_then(|_| self.decoder.decode_frame(&mut self.pcm));
        if let Err(err) = decoded {
            self.errors += 1;
            if self.errors % 100 == 1 {
                log::warn!("audio decode error: {err}");
            }
            return None;
        }
        // decoded_frame_size() already counts every channel's samples.
        let channels = self.decoder.stream_info().numChannels.max(1) as usize;
        let samples = &self.pcm[..self.decoder.decoded_frame_size()];
        Some(if channels == CHANNELS {
            samples.iter().map(|&s| s as f32 / 32768.0).collect()
        } else {
            // Mono (or anything else): spread the first channel over both speakers.
            samples.chunks(channels).flat_map(|c| [c[0] as f32 / 32768.0; CHANNELS]).collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;
    use std::ffi::c_void;
    use std::mem::MaybeUninit;

    /// An AAC-ELD encoder configured like the iPhone's: 480 samples per frame.
    struct IphoneEncoder(fdk_aac_sys::HANDLE_AACENCODER);

    impl IphoneEncoder {
        fn new() -> (Self, Vec<u8>) {
            use fdk_aac_sys as sys;
            unsafe {
                let mut handle = std::ptr::null_mut();
                assert_eq!(sys::aacEncOpen(&mut handle, 0, 2), 0);
                for (param, value) in [
                    (sys::AACENC_PARAM_AACENC_AOT, sys::AUDIO_OBJECT_TYPE_AOT_ER_AAC_ELD as u32),
                    (sys::AACENC_PARAM_AACENC_SAMPLERATE, SAMPLE_RATE),
                    (sys::AACENC_PARAM_AACENC_CHANNELMODE, sys::CHANNEL_MODE_MODE_2 as u32),
                    (sys::AACENC_PARAM_AACENC_GRANULE_LENGTH, 480),
                    (sys::AACENC_PARAM_AACENC_SBR_MODE, 0),
                    (sys::AACENC_PARAM_AACENC_BITRATE, 96_000),
                    (sys::AACENC_PARAM_AACENC_TRANSMUX, sys::TRANSPORT_TYPE_TT_MP4_RAW as u32),
                ] {
                    assert_eq!(sys::aacEncoder_SetParam(handle, param, value), 0);
                }
                // Encoding with no buffers applies the parameters.
                use std::ptr::{null, null_mut};
                assert_eq!(sys::aacEncEncode(handle, null(), null(), null(), null_mut()), 0);
                let mut info = MaybeUninit::<sys::AACENC_InfoStruct>::zeroed();
                assert_eq!(sys::aacEncInfo(handle, info.as_mut_ptr()), 0);
                let info = info.assume_init();
                (IphoneEncoder(handle), info.confBuf[..info.confSize as usize].to_vec())
            }
        }

        fn encode(&self, pcm: &[i16]) -> Vec<u8> {
            use fdk_aac_sys as sys;
            let mut out = vec![0u8; 2048];
            unsafe {
                let mut in_ptr = pcm.as_ptr() as *mut c_void;
                let mut in_id = sys::AACENC_BufferIdentifier_IN_AUDIO_DATA as i32;
                let mut in_size = (pcm.len() * 2) as i32;
                let mut in_el = 2i32;
                let in_desc = sys::AACENC_BufDesc {
                    numBufs: 1,
                    bufs: &mut in_ptr,
                    bufferIdentifiers: &mut in_id,
                    bufSizes: &mut in_size,
                    bufElSizes: &mut in_el,
                };
                let mut out_ptr = out.as_mut_ptr() as *mut c_void;
                let mut out_id = sys::AACENC_BufferIdentifier_OUT_BITSTREAM_DATA as i32;
                let mut out_size = out.len() as i32;
                let mut out_el = 1i32;
                let out_desc = sys::AACENC_BufDesc {
                    numBufs: 1,
                    bufs: &mut out_ptr,
                    bufferIdentifiers: &mut out_id,
                    bufSizes: &mut out_size,
                    bufElSizes: &mut out_el,
                };
                let in_args = sys::AACENC_InArgs { numInSamples: pcm.len() as i32, numAncBytes: 0 };
                let mut out_args = MaybeUninit::<sys::AACENC_OutArgs>::zeroed();
                assert_eq!(sys::aacEncEncode(self.0, &in_desc, &out_desc, &in_args, out_args.as_mut_ptr()), 0);
                out.truncate(out_args.assume_init().numOutBytes as usize);
            }
            out
        }
    }

    impl Drop for IphoneEncoder {
        fn drop(&mut self) {
            unsafe { fdk_aac_sys::aacEncClose(&mut self.0) };
        }
    }

    fn silent_output() -> Arc<Output> {
        Arc::new(Output {
            queue: Mutex::new(Queue { samples: VecDeque::new(), playing: false, gain: 0.0 }),
            volume: AtomicU32::new(1.0f32.to_bits()),
            client_gain: AtomicU32::new(1.0f32.to_bits()),
        })
    }

    /// iPhone-style audio end to end: a 440 Hz tone encoded as AAC-ELD/480,
    /// AES-CBC encrypted into RTP packets, sent over UDP to the audio port, and
    /// checked for pitch in the playback queue.
    #[tokio::test]
    async fn plays_iphone_audio_from_udp() {
        let (encoder, asc) = IphoneEncoder::new();
        assert_eq!(asc, audio_specific_config(8).unwrap(), "our AAC-ELD config must match a 480-sample encoder");

        let key = [7u8; 16];
        let iv = [9u8; 16];
        let output = silent_output();
        let local = IpAddr::from([127, 0, 0, 1]);
        let (data_port, _, task) = listen(local, key, iv, 8, output.clone()).await.unwrap();
        let sender = UdpSocket::bind((local, 0)).await.unwrap();

        for frame in 0..26u16 {
            let pcm: Vec<i16> = (0..480)
                .flat_map(|i| {
                    let t = (frame as usize * 480 + i) as f32 / SAMPLE_RATE as f32;
                    let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 12000.0) as i16;
                    [v, v]
                })
                .collect();
            let mut payload = encoder.encode(&pcm);
            if payload.is_empty() {
                continue;
            }
            let whole = payload.len() / 16 * 16;
            cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into())
                .encrypt_padded_mut::<NoPadding>(&mut payload[..whole], whole)
                .unwrap();
            let mut packet = vec![0x80, 0x60];
            packet.extend_from_slice(&frame.to_be_bytes());
            packet.extend_from_slice(&[0; 8]);
            packet.extend_from_slice(&payload);
            sender.send_to(&packet, (local, data_port)).await.unwrap();
            // Duplicates (e.g. retransmits that arrive anyway) must not play twice.
            sender.send_to(&packet, (local, data_port)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        task.abort();

        let queued: Vec<f32> = output.queue.lock().unwrap().samples.iter().copied().collect();
        assert!(queued.len() <= 26 * 480 * 2, "duplicates were played: {} samples", queued.len());
        // Skip the codec's start-up delay, then measure the pitch on the left channel.
        let left: Vec<f32> = queued.chunks(2).map(|c| c[0]).skip(2048).collect();
        assert!(left.len() > SAMPLE_RATE as usize / 10, "too little audio: {}", left.len());
        let crossings = left.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count();
        let hz = crossings as f32 / 2.0 / (left.len() as f32 / SAMPLE_RATE as f32);
        assert!((hz - 440.0).abs() < 15.0, "decoded tone is {hz} Hz");
        let peak = left.iter().fold(0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.2, "decoded tone too quiet: {peak}");
    }

    #[test]
    fn client_volume_maps_db_to_gain() {
        let output = silent_output();
        output.set_client_volume_db(-144.0);
        assert_eq!(output.target_gain(), 0.0);
        output.set_client_volume_db(0.0);
        assert_eq!(output.target_gain(), 1.0);
        output.set_client_volume_db(-6.0);
        assert!((output.target_gain() - 0.501).abs() < 0.01);
    }
}
