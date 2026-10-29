//! Bridge between AirPlay sessions and the UI: state changes and video data
//! are broadcast to every connected UI socket.

use std::sync::Mutex;

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::broadcast;

/// Binary message tags understood by the UI.
const TAG_CODEC: u8 = 1;
const TAG_FRAME: u8 = 2;

#[derive(Clone)]
pub enum UiMessage {
    Text(String),
    Binary(Bytes),
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Connecting,
    Mirroring,
    Paused,
}

#[derive(Clone, Serialize)]
struct Status {
    #[serde(rename = "type")]
    kind: &'static str,
    state: State,
    device: Option<String>,
    receiver: String,
    address: Option<String>,
    volume: f32,
}

struct Inner {
    owner: Option<u64>,
    status: Status,
    codec: Option<Bytes>,
}

pub struct Hub {
    tx: broadcast::Sender<UiMessage>,
    kick: broadcast::Sender<()>,
    inner: Mutex<Inner>,
}

impl Hub {
    pub fn new(receiver: String, address: Option<String>, volume: f32) -> Self {
        let (tx, _) = broadcast::channel(2048);
        let (kick, _) = broadcast::channel(4);
        Hub {
            tx,
            kick,
            inner: Mutex::new(Inner {
                owner: None,
                status: Status { kind: "status", state: State::Idle, device: None, receiver, address, volume },
                codec: None,
            }),
        }
    }

    /// Current state for a newly connected UI, plus the live feed.
    pub fn subscribe(&self) -> (Vec<UiMessage>, broadcast::Receiver<UiMessage>) {
        let inner = self.inner.lock().unwrap();
        let mut initial = vec![UiMessage::Text(serde_json::to_string(&inner.status).unwrap())];
        if let Some(codec) = &inner.codec {
            initial.push(UiMessage::Binary(codec.clone()));
        }
        (initial, self.tx.subscribe())
    }

    /// Signal that fires when the user asks to stop mirroring.
    pub fn kicked(&self) -> broadcast::Receiver<()> {
        self.kick.subscribe()
    }

    pub fn stop_mirroring(&self) {
        let _ = self.kick.send(());
    }

    pub fn set_receiver(&self, name: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.status.receiver = name;
        let state = inner.status.state;
        self.publish(&mut inner, state);
    }

    /// Remembered so a reopened window shows the current level; the window itself
    /// is what changes it, so nothing is broadcast.
    /// Broadcasts an event (such as file-transfer progress) to every window.
    pub fn notify(&self, event: serde_json::Value) {
        let _ = self.tx.send(UiMessage::Text(event.to_string()));
    }

    pub fn set_volume(&self, volume: f32) {
        self.inner.lock().unwrap().status.volume = volume;
    }

    /// A client started setting up a mirroring session; it takes over the screen.
    pub fn begin(&self, session: u64, device: String) {
        let mut inner = self.inner.lock().unwrap();
        inner.owner = Some(session);
        inner.codec = None;
        inner.status.device = Some(device);
        self.publish(&mut inner, State::Connecting);
    }

    /// A stream SETUP after a teardown on the same connection takes the screen back.
    pub fn reclaim(&self, session: u64, device: &str) {
        let owned = self.inner.lock().unwrap().owner == Some(session);
        if !owned {
            self.begin(session, device.to_string());
        }
    }

    /// New SPS/PPS from the client (sent at start, on rotation, and around sleep).
    pub fn codec(&self, session: u64, width: u16, height: u16, avcc: &[u8], paused: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.owner != Some(session) {
            return;
        }
        let mut msg = Vec::with_capacity(6 + avcc.len());
        msg.extend_from_slice(&[TAG_CODEC, 0]);
        msg.extend_from_slice(&width.to_le_bytes());
        msg.extend_from_slice(&height.to_le_bytes());
        msg.extend_from_slice(avcc);
        let msg = Bytes::from(msg);
        inner.codec = Some(msg.clone());
        let _ = self.tx.send(UiMessage::Binary(msg));
        self.publish(&mut inner, if paused { State::Paused } else { State::Mirroring });
    }

    pub fn set_paused(&self, session: u64, paused: bool) {
        let mut inner = self.inner.lock().unwrap();
        if inner.owner != Some(session) || inner.status.state == State::Connecting {
            return;
        }
        self.publish(&mut inner, if paused { State::Paused } else { State::Mirroring });
    }

    /// `frame` must start with two reserved bytes, followed by the AVCC access unit.
    pub fn frame(&self, session: u64, keyframe: bool, mut frame: Vec<u8>) {
        if self.inner.lock().unwrap().owner != Some(session) {
            return;
        }
        frame[0] = TAG_FRAME;
        frame[1] = keyframe as u8;
        let _ = self.tx.send(UiMessage::Binary(Bytes::from(frame)));
    }

    pub fn end(&self, session: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.owner != Some(session) {
            return;
        }
        inner.owner = None;
        inner.codec = None;
        inner.status.device = None;
        self.publish(&mut inner, State::Idle);
    }

    fn publish(&self, inner: &mut Inner, state: State) {
        inner.status.state = state;
        let _ = self.tx.send(UiMessage::Text(serde_json::to_string(&inner.status).unwrap()));
    }
}
