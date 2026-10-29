//! Drop: file sharing with nearby devices over LocalSend, built on LocalSend's
//! own Rust core (vendor/localsend-core). Real AirDrop needs Wi-Fi modes
//! (AWDL / Wi-Fi Aware) that Windows drivers don't expose; the free LocalSend
//! app gives the iPhone a Share-sheet target instead. Incoming files are
//! accepted automatically and saved to the Downloads folder.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use localsend::discovery::{
    DEFAULT_DISCOVERY_TIMEOUT, DeviceChannel, DeviceIdentity, DiscoveredDevice, DiscoveryConfig, DiscoveryEvent,
    DiscoveryHandle, HttpChannel,
};
use localsend::http::client::ClientError;
use localsend::http::client::v2::LsHttpClientV2;
use localsend::http::dto_v2::{PrepareUploadRequestDtoV2, RegisterDtoV2};
use localsend::http::server::common::save::FileUploadTarget;
use localsend::http::server::v2::{PrepareUploadDecisionV2, ServerEventV2, SessionEndReasonV2};
use localsend::http::server::web::WebConfig;
use localsend::http::server::{ServerConfigV2, ServerHandle, TlsConfig, start_with_port};
use localsend::http::state::ClientInfo;
use localsend::model::discovery::{DeviceType, PROTOCOL_VERSION_V2, ProtocolType};
use localsend::model::transfer::{FileContent, FileDto};
use localsend::multicast::{DEFAULT_MULTICAST_GROUP, DEFAULT_MULTICAST_GROUP_V6, DEFAULT_PORT, MulticastDevice};
use localsend::util::filename::{Rules, sanitize_path};
use localsend::util::interface::{InterfaceFilter, local_interface_addresses};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::airplay::hub::Hub;

const KEEP_RECEIVED: usize = 100;
/// Devices not heard from for this long drop off the Nearby list.
const NEARBY_FOR: Duration = Duration::from_secs(75);
const DEVICE_MODEL: &str = "Windows";

/// This PC's LocalSend identity: a self-signed certificate whose SHA-256
/// fingerprint other devices pin, kept on disk so it survives restarts.
struct Identity {
    cert_pem: String,
    key_pem: String,
    fingerprint: String,
}

impl Identity {
    fn load_or_create(path: &Path) -> anyhow::Result<Self> {
        if let Ok(text) = std::fs::read_to_string(path) {
            let blocks = pem::parse_many(&text)?;
            let cert = blocks.iter().find(|b| b.tag() == "CERTIFICATE");
            let key = blocks.iter().find(|b| b.tag().ends_with("PRIVATE KEY"));
            if let (Some(cert), Some(key)) = (cert, key) {
                return Ok(Identity {
                    fingerprint: localsend::crypto::cert::fingerprint_from_cert_der(cert.contents()),
                    cert_pem: pem::encode(cert),
                    key_pem: pem::encode(key),
                });
            }
        }
        let cert = localsend::crypto::cert::generate_self_signed()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, format!("{}{}", cert.certificate_pem, cert.private_key_pem))?;
        Ok(Identity { cert_pem: cert.certificate_pem, key_pem: cert.private_key_pem, fingerprint: cert.fingerprint })
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Received {
    id: u64,
    name: String,
    size: u64,
    from: String,
    path: String,
    image: bool,
}

#[derive(Clone, Serialize)]
struct Outgoing {
    id: u64,
    name: String,
    size: u64,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Nearby {
    fingerprint: String,
    alias: String,
    device_type: &'static str,
    device_model: Option<String>,
}

/// An upload session another device opened with us.
struct Incoming {
    from: String,
    /// IDs of files whose upload started, for marking them cancelled.
    items: Vec<u64>,
}

/// A transfer we're running to another device.
struct Sending {
    host: String,
    session: Option<String>,
    cancel: CancellationToken,
}

struct Network {
    server_stop: oneshot::Sender<()>,
    server: ServerHandle,
    discovery: Arc<DiscoveryHandle>,
    discovery_stop: oneshot::Sender<()>,
}

pub struct FileDrop {
    hub: Arc<Hub>,
    folder: PathBuf,
    identity: Identity,
    alias: Mutex<String>,
    network: tokio::sync::Mutex<Option<Network>>,
    discovery: Mutex<Option<Arc<DiscoveryHandle>>>,
    last_seen: Mutex<HashMap<String, Instant>>,
    incoming: Mutex<HashMap<String, Incoming>>,
    sending: Mutex<HashMap<String, Sending>>,
    received: Mutex<Vec<Received>>,
    outbox: Mutex<Vec<Outgoing>>,
    next_id: AtomicU64,
    error: Mutex<Option<String>>,
}

impl FileDrop {
    pub async fn start(
        alias: String,
        folder: PathBuf,
        identity_file: &Path,
        hub: Arc<Hub>,
    ) -> anyhow::Result<Arc<Self>> {
        let files = Arc::new(FileDrop {
            hub,
            folder,
            identity: Identity::load_or_create(identity_file)?,
            alias: Mutex::new(alias),
            network: tokio::sync::Mutex::new(None),
            discovery: Mutex::new(None),
            last_seen: Mutex::new(HashMap::new()),
            incoming: Mutex::new(HashMap::new()),
            sending: Mutex::new(HashMap::new()),
            received: Mutex::new(Vec::new()),
            outbox: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            error: Mutex::new(None),
        });
        if let Err(err) = files.clone().start_network().await {
            log::warn!("LocalSend unavailable: {err:#}");
            *files.error.lock().unwrap() = Some(format!(
                "Port {DEFAULT_PORT} is in use. If the LocalSend app is running on this PC, quit it and restart RustPlay."
            ));
        }
        // Keep the Nearby list fresh: LocalSend apps answer announcements while they're open.
        let weak = Arc::downgrade(&files);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            loop {
                tick.tick().await;
                let Some(files) = weak.upgrade() else { return };
                let discovery = files.discovery.lock().unwrap().clone();
                if let Some(discovery) = discovery {
                    discovery.announce().await;
                }
                files.publish_nearby();
            }
        });
        Ok(files)
    }

    /// Shows this PC under a new name; the server and discovery restart with it.
    pub async fn set_alias(self: &Arc<Self>, alias: String) {
        *self.alias.lock().unwrap() = alias;
        if let Err(err) = self.clone().start_network().await {
            log::warn!("restarting LocalSend failed: {err:#}");
        }
    }

    async fn start_network(self: Arc<Self>) -> anyhow::Result<()> {
        let mut network = self.network.lock().await;
        if let Some(old) = network.take() {
            let _ = old.server_stop.send(());
            let _ = old.discovery_stop.send(());
            let _ = tokio::time::timeout(Duration::from_secs(1), old.server.wait_stopped()).await;
            let _ = tokio::time::timeout(Duration::from_secs(1), old.discovery.wait_stopped()).await;
        }

        let (server_tx, mut server_rx) = mpsc::channel::<ServerEventV2>(32);
        let (server_stop, server_stop_rx) = oneshot::channel();
        let server = start_with_port(
            DEFAULT_PORT,
            Some(TlsConfig { cert: self.identity.cert_pem.clone(), private_key: self.identity.key_pem.clone() }),
            self.client_info(),
            None,
            Some(ServerConfigV2 { pin: None, verify_checksums: true, event_tx: server_tx }),
            WebConfig::default(),
            server_stop_rx,
        )
        .await?;

        let (discovery_tx, mut discovery_rx) = mpsc::channel::<DiscoveryEvent>(32);
        let (discovery_stop, discovery_stop_rx) = oneshot::channel();
        let discovery = Arc::new(
            localsend::discovery::start(
                DiscoveryConfig {
                    group: DEFAULT_MULTICAST_GROUP,
                    group_v6: Some(DEFAULT_MULTICAST_GROUP_V6),
                    port: DEFAULT_PORT,
                    interface_filter: InterfaceFilter::default(),
                    device: self.multicast_device(),
                    identity: DeviceIdentity {
                        cert_pem: self.identity.cert_pem.clone(),
                        private_key_pem: self.identity.key_pem.clone(),
                    },
                    timeout: DEFAULT_DISCOVERY_TIMEOUT,
                    event_tx: Some(discovery_tx),
                },
                discovery_stop_rx,
            )
            .await,
        );
        if let Some(err) = discovery.multicast_error() {
            log::warn!("LocalSend multicast unavailable: {err:#}");
        }
        *self.discovery.lock().unwrap() = Some(discovery.clone());
        *network = Some(Network { server_stop, server, discovery: discovery.clone(), discovery_stop });
        drop(network);
        *self.error.lock().unwrap() = None;
        log::info!("LocalSend on port {DEFAULT_PORT} as \"{}\"", self.alias.lock().unwrap());

        let me = self.clone();
        tokio::spawn(async move {
            while let Some(event) = server_rx.recv().await {
                me.clone().on_server_event(event);
            }
        });
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(event) = discovery_rx.recv().await {
                if let DiscoveryEvent::Discovered { device } | DiscoveryEvent::Updated { device } = event {
                    me.last_seen.lock().unwrap().insert(device.fingerprint.clone(), Instant::now());
                    me.publish_nearby();
                }
            }
        });
        self.scan();
        Ok(())
    }

    fn client_info(&self) -> ClientInfo {
        ClientInfo {
            alias: self.alias.lock().unwrap().clone(),
            version: PROTOCOL_VERSION_V2.to_string(),
            device_model: Some(DEVICE_MODEL.into()),
            device_type: Some(DeviceType::Desktop),
            token: self.identity.fingerprint.clone(),
        }
    }

    fn register_dto(&self) -> RegisterDtoV2 {
        RegisterDtoV2 {
            alias: self.alias.lock().unwrap().clone(),
            version: PROTOCOL_VERSION_V2.to_string(),
            device_model: Some(DEVICE_MODEL.into()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: self.identity.fingerprint.clone(),
            port: DEFAULT_PORT,
            protocol: ProtocolType::Https,
            download: false,
        }
    }

    fn multicast_device(&self) -> MulticastDevice {
        MulticastDevice {
            alias: self.alias.lock().unwrap().clone(),
            version: PROTOCOL_VERSION_V2.to_string(),
            device_model: Some(DEVICE_MODEL.into()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: self.identity.fingerprint.clone(),
            port: DEFAULT_PORT,
            protocol: ProtocolType::Https,
            download: false,
        }
    }

    /// Announces this PC, probes known devices, then sweeps the local subnets.
    fn scan(&self) {
        let Some(discovery) = self.discovery.lock().unwrap().clone() else { return };
        let known = discovery
            .devices()
            .iter()
            .filter_map(|d| d.get_best_channel().and_then(|c| c.http()).cloned())
            .collect::<Vec<_>>();
        let interface_ips = local_interface_addresses(&InterfaceFilter::default()).unwrap_or_default();
        tokio::spawn(async move {
            let _ = discovery
                .discover_staged(known, interface_ips, DEFAULT_PORT, ProtocolType::Https, Duration::from_secs(1))
                .await;
        });
    }

    /// A device proved it's reachable outside discovery (it registered, or sent us files).
    fn confirm(&self, host: String, info: RegisterDtoV2) {
        if info.fingerprint == self.identity.fingerprint {
            return;
        }
        self.last_seen.lock().unwrap().insert(info.fingerprint.clone(), Instant::now());
        let device = DiscoveredDevice {
            alias: info.alias,
            version: info.version,
            device_model: info.device_model,
            device_type: info.device_type,
            fingerprint: info.fingerprint,
            channel: DeviceChannel::Http(HttpChannel { host, port: info.port, protocol: info.protocol }),
            download: info.download,
        };
        if let Some(discovery) = self.discovery.lock().unwrap().clone() {
            tokio::spawn(async move {
                discovery.add_device(device).await;
            });
        }
    }

    fn on_server_event(self: Arc<Self>, event: ServerEventV2) {
        match event {
            ServerEventV2::Register { ip, info } => self.confirm(ip.to_string(), info),
            ServerEventV2::PrepareUpload { session_id, ip, info, files, decision_tx, .. } => {
                let from = info.alias.clone();
                self.confirm(ip.to_string(), info);
                // A text message arrives as a single file whose preview holds the text.
                let message =
                    (files.len() == 1).then(|| files.values().next().and_then(|f| f.preview.clone())).flatten();
                if let Some(text) = message {
                    self.event(json!({ "event": "message", "from": from, "text": text }));
                    let _ = decision_tx.send(PrepareUploadDecisionV2::Accept(HashSet::new()));
                    return;
                }
                // Received files are saved automatically, without asking.
                self.incoming.lock().unwrap().insert(session_id, Incoming { from, items: Vec::new() });
                let _ = decision_tx.send(PrepareUploadDecisionV2::Accept(files.into_keys().collect()));
            }
            ServerEventV2::FileUpload { session_id, file, target_tx, .. } => {
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                let from = {
                    let mut incoming = self.incoming.lock().unwrap();
                    let session = incoming
                        .entry(session_id)
                        .or_insert_with(|| Incoming { from: "Unknown".into(), items: Vec::new() });
                    session.items.push(id);
                    session.from.clone()
                };
                let path = unique_path(&self.folder, &received_name(&file.file_name));
                if let Err(err) = std::fs::create_dir_all(&self.folder) {
                    log::warn!("can't create {}: {err}", self.folder.display());
                }
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.event(json!({ "event": "incoming", "id": id, "name": name, "total": file.size, "from": from }));

                let (result_tx, result_rx) = oneshot::channel();
                let (progress_tx, mut progress_rx) = mpsc::channel::<u64>(8);
                let target = FileUploadTarget::Path { path: path.clone(), result_tx, progress_tx: Some(progress_tx) };
                if target_tx.send(target).is_err() {
                    return;
                }
                tokio::spawn(async move {
                    let mut last = Instant::now();
                    while let Some(written) = progress_rx.recv().await {
                        if last.elapsed() > Duration::from_millis(150) {
                            last = Instant::now();
                            self.event(json!({ "event": "progress", "id": id, "received": written }));
                        }
                    }
                    match result_rx.await {
                        Ok(Ok(())) => {
                            log::info!("received {} from {from}", path.display());
                            mark_from_internet(&path);
                            let item = Received {
                                id,
                                name,
                                size: file.size,
                                from,
                                image: is_previewable(&path),
                                path: path.display().to_string(),
                            };
                            let mut received = self.received.lock().unwrap();
                            received.insert(0, item.clone());
                            received.truncate(KEEP_RECEIVED);
                            drop(received);
                            self.event(json!({ "event": "received", "item": item }));
                        }
                        other => {
                            log::warn!("receiving {name} failed: {other:?}");
                            let _ = std::fs::remove_file(&path);
                            self.event(json!({ "event": "failed", "id": id }));
                        }
                    }
                });
            }
            ServerEventV2::SessionEnd { session_id, reason } => {
                let session = self.incoming.lock().unwrap().remove(&session_id);
                if let (Some(session), SessionEndReasonV2::Cancelled) = (session, reason) {
                    for id in session.items {
                        self.event(json!({ "event": "failed", "id": id }));
                    }
                }
            }
            ServerEventV2::PrepareUploadAborted { session_id } => {
                self.incoming.lock().unwrap().remove(&session_id);
            }
            ServerEventV2::CancelReceived { ip, session_id } => {
                let host = ip.to_string();
                for sending in self.sending.lock().unwrap().values() {
                    if sending.host == host && sending.session.as_deref() == Some(session_id.as_str()) {
                        sending.cancel.cancel();
                    }
                }
            }
            ServerEventV2::ListenerFailed { error } => {
                log::warn!("LocalSend server stopped: {error}; restarting");
                tokio::spawn(async move {
                    let _ = self.start_network().await;
                });
            }
        }
    }

    /// Commands from the window (see App::command in main.rs).
    pub fn command(self: &Arc<Self>, request: &Value) -> Value {
        match request["type"].as_str().unwrap_or_default() {
            "drop-info" => json!({
                "ok": true,
                "folder": self.folder.display().to_string(),
                "fingerprint": self.identity.fingerprint,
                "error": *self.error.lock().unwrap(),
                "outbox": self.outbox.lock().unwrap().clone(),
                "received": self.received.lock().unwrap().clone(),
                "nearby": self.nearby(),
            }),
            "drop-scan" => {
                self.scan();
                json!({ "ok": true })
            }
            "drop-stage-paths" => {
                for path in request["paths"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                    self.queue(PathBuf::from(path));
                }
                self.publish_outbox();
                json!({ "ok": true })
            }
            "drop-remove" => {
                let id = request["item"].as_u64().unwrap_or(0);
                self.outbox.lock().unwrap().retain(|item| item.id != id);
                self.publish_outbox();
                json!({ "ok": true })
            }
            "drop-send" => {
                let fingerprint = request["fingerprint"].as_str().unwrap_or_default().to_string();
                match self.clone().send(fingerprint) {
                    Ok(()) => json!({ "ok": true }),
                    Err(error) => json!({ "ok": false, "error": error }),
                }
            }
            "drop-cancel" => {
                let fingerprint = request["fingerprint"].as_str().unwrap_or_default();
                if let Some(sending) = self.sending.lock().unwrap().get(fingerprint) {
                    sending.cancel.cancel();
                }
                json!({ "ok": true })
            }
            "drop-open" | "drop-reveal" => {
                let id = request["item"].as_u64().unwrap_or(0);
                let path = self.received.lock().unwrap().iter().find(|r| r.id == id).map(|r| PathBuf::from(&r.path));
                match path {
                    Some(path) => {
                        reveal(&path, request["type"] == "drop-reveal" || !opens_directly(&path));
                        json!({ "ok": true })
                    }
                    None => json!({ "ok": false, "error": "That file is gone." }),
                }
            }
            "drop-open-folder" => {
                reveal(&self.folder, false);
                json!({ "ok": true })
            }
            _ => json!({ "ok": false, "error": "unknown command" }),
        }
    }

    fn event(&self, mut event: Value) {
        event["type"] = "drop".into();
        self.hub.notify(event);
    }

    fn nearby(&self) -> Vec<Nearby> {
        let Some(discovery) = self.discovery.lock().unwrap().clone() else { return Vec::new() };
        let seen = self.last_seen.lock().unwrap();
        let mut recent: Vec<_> = discovery
            .devices()
            .into_iter()
            .filter(|d| d.device.fingerprint != self.identity.fingerprint)
            .filter_map(|d| seen.get(&d.device.fingerprint).filter(|t| t.elapsed() < NEARBY_FOR).map(|t| (*t, d)))
            .collect();
        // A device that reinstalled LocalSend comes back with a new identity
        // at the same address; show only its newest one.
        recent.sort_by(|a, b| b.0.cmp(&a.0));
        let mut shown = HashSet::new();
        let mut devices: Vec<Nearby> = recent
            .into_iter()
            .map(|(_, d)| d)
            .filter(|d| {
                let host = d.get_best_channel().and_then(|c| c.http()).map(|h| h.host.clone()).unwrap_or_default();
                shown.insert((d.device.alias.clone(), host))
            })
            .map(|d| Nearby {
                fingerprint: d.device.fingerprint.clone(),
                alias: d.device.alias.clone(),
                device_type: match d.device.device_type {
                    Some(DeviceType::Mobile) => "mobile",
                    Some(DeviceType::Web) => "web",
                    _ => "desktop",
                },
                device_model: d.device.device_model.clone(),
            })
            .collect();
        devices.sort_by(|a, b| a.alias.to_lowercase().cmp(&b.alias.to_lowercase()));
        devices
    }

    fn publish_nearby(&self) {
        self.event(json!({ "event": "nearby", "devices": self.nearby() }));
    }

    fn publish_outbox(&self) {
        let items = self.outbox.lock().unwrap().clone();
        self.event(json!({ "event": "outbox", "items": items }));
    }

    fn queue(&self, path: PathBuf) {
        let Ok(meta) = std::fs::metadata(&path) else { return };
        if !meta.is_file() {
            return;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "File".into());
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.outbox.lock().unwrap().push(Outgoing { id, name, size: meta.len(), path });
    }

    /// Sends everything in the outbox to one device; progress arrives as "send" events.
    fn send(self: Arc<Self>, fingerprint: String) -> Result<(), String> {
        let items = self.outbox.lock().unwrap().clone();
        if items.is_empty() {
            return Err("Add files to send first.".into());
        }
        let discovery = self.discovery.lock().unwrap().clone().ok_or("LocalSend isn't running.")?;
        let device = discovery.device_by_fingerprint(&fingerprint).ok_or("That device is no longer nearby.")?;
        let http = device.get_best_channel().and_then(|c| c.http()).cloned().ok_or("That device has no address.")?;
        let cancel = CancellationToken::new();
        {
            let mut sending = self.sending.lock().unwrap();
            if sending.contains_key(&fingerprint) {
                return Err("Already sending to that device.".into());
            }
            sending.insert(
                fingerprint.clone(),
                Sending { host: http.host.clone(), session: None, cancel: cancel.clone() },
            );
        }
        let alias = device.device.alias.clone();
        tokio::spawn(async move {
            let state = self.transfer(&fingerprint, &http, &items, cancel).await;
            self.sending.lock().unwrap().remove(&fingerprint);
            log::info!("sending to {alias}: {state}");
            self.event(json!({ "event": "send", "fingerprint": fingerprint, "state": state }));
        });
        Ok(())
    }

    async fn transfer(
        &self,
        fingerprint: &str,
        http: &HttpChannel,
        items: &[Outgoing],
        cancel: CancellationToken,
    ) -> &'static str {
        let status = |state: &str, progress: f64| {
            self.event(json!({ "event": "send", "fingerprint": fingerprint, "state": state, "progress": progress }));
        };
        status("waiting", 0.0);
        // A device that left (or a firewall) would otherwise leave the request
        // hanging; the accept prompt on the other side gets no time limit.
        let probe = tokio::net::TcpStream::connect((http.host.as_str(), http.port));
        if !matches!(tokio::time::timeout(Duration::from_secs(3), probe).await, Ok(Ok(_))) {
            return "unreachable";
        }
        let expected = matches!(http.protocol, ProtocolType::Https).then(|| fingerprint.to_string());
        let client = match LsHttpClientV2::try_new(&self.identity.key_pem, &self.identity.cert_pem, expected, None) {
            Ok(client) => client,
            Err(err) => {
                log::warn!("LocalSend client: {err}");
                return "failed";
            }
        };
        let files: HashMap<String, FileDto> = items
            .iter()
            .map(|item| {
                let id = item.id.to_string();
                let dto = FileDto {
                    id: id.clone(),
                    file_name: item.name.clone(),
                    size: item.size,
                    file_type: content_type(&item.path).into(),
                    sha256: None,
                    preview: None,
                    metadata: None,
                };
                (id, dto)
            })
            .collect();
        let payload = PrepareUploadRequestDtoV2 { info: self.register_dto(), files: files.clone() };
        let prepared = client
            .prepare_upload(http.protocol.clone(), &http.host, http.port, None, payload, None, cancel.clone())
            .await;
        let response = match prepared {
            Ok(prepared) => match prepared.response {
                Some(response) if prepared.status_code == 200 => response,
                _ => return "declined",
            },
            Err(ClientError::StatusCode(err)) if err.status == 403 => return "declined",
            Err(ClientError::StatusCode(err)) if err.status == 409 => return "busy",
            Err(_) if cancel.is_cancelled() => return "cancelled",
            Err(err) => {
                log::warn!("prepare-upload failed: {err}");
                return "failed";
            }
        };
        if let Some(sending) = self.sending.lock().unwrap().get_mut(fingerprint) {
            sending.session = Some(response.session_id.clone());
        }

        let total: u64 = response.files.keys().filter_map(|id| files.get(id)).map(|f| f.size).sum::<u64>().max(1);
        let sent = Arc::new(AtomicU64::new(0));
        status("sending", 0.0);
        for item in items {
            let id = item.id.to_string();
            let Some(token) = response.files.get(&id) else { continue };
            let base = sent.load(Ordering::Relaxed);
            let counter = sent.clone();
            let mut last = Instant::now();
            // The body outlives this call, so it reports through its own handles.
            let (hub, who) = (self.hub.clone(), fingerprint.to_string());
            let body = upload_body(FileContent::Path(item.path.clone()), move |of_file| {
                counter.store(base + of_file, Ordering::Relaxed);
                if last.elapsed() > Duration::from_millis(150) {
                    last = Instant::now();
                    let progress = (base + of_file) as f64 / total as f64;
                    hub.notify(json!({ "type": "drop", "event": "send", "fingerprint": who, "state": "sending", "progress": progress }));
                }
            });
            let uploaded = client
                .upload(
                    http.protocol.clone(),
                    &http.host,
                    http.port,
                    None,
                    &response.session_id,
                    &id,
                    token,
                    body,
                    cancel.clone(),
                )
                .await;
            match uploaded {
                Ok(()) => sent.store(base + item.size, Ordering::Relaxed),
                Err(err) => {
                    let _ = client.cancel(http.protocol.clone(), &http.host, http.port, &response.session_id).await;
                    if cancel.is_cancelled() {
                        return "cancelled";
                    }
                    log::warn!("upload of {} failed: {err}", item.name);
                    return "failed";
                }
            }
        }
        "done"
    }
}

/// A streaming request body that reports how many bytes of the file went out.
fn upload_body(content: FileContent, mut progress: impl FnMut(u64) + Send + 'static) -> localsend::reqwest::Body {
    let mut sent = 0u64;
    let stream = content.into_stream().map(move |chunk| {
        let chunk: Bytes = chunk?;
        sent += chunk.len() as u64;
        progress(sent);
        Ok::<Bytes, anyhow::Error>(chunk)
    });
    localsend::reqwest::Body::wrap_stream(stream)
}

fn extension(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

fn content_type(path: &Path) -> &'static str {
    match extension(path).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" => "image/heic",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Images the window can show a thumbnail for (WebView2 can't decode HEIC).
fn is_previewable(path: &Path) -> bool {
    matches!(extension(path).as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp")
}

/// Received files open straight away only if they're media, PDFs or text; anything
/// else (programs, scripts, shortcuts, disk images…) is shown in Explorer instead.
fn opens_directly(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "webp"
            | "heic"
            | "heif"
            | "bmp"
            | "tif"
            | "tiff"
            | "mp4"
            | "mov"
            | "m4v"
            | "mkv"
            | "webm"
            | "avi"
            | "mp3"
            | "m4a"
            | "aac"
            | "wav"
            | "flac"
            | "pdf"
            | "txt"
    )
}

/// A sender's file name made safe for Downloads: no folders or reserved names, and
/// no invisible direction marks that could make "photo\u{202E}gpj.exe" look like a JPEG.
fn received_name(name: &str) -> String {
    let visible: String = name
        .chars()
        .filter(
            |c| !matches!(c, '\u{200E}' | '\u{200F}' | '\u{061C}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'),
        )
        .collect();
    sanitize_path(&visible, Rules::Windows)
}

/// Tags a received file as coming from the internet (Mark of the Web), so Windows
/// warns before running it and Office opens it in Protected View.
fn mark_from_internet(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    if let Err(err) = std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n") {
        log::warn!("couldn't mark {} as downloaded: {err}", path.display());
    }
}

/// "photo.jpg", then "photo (2).jpg", "photo (3).jpg", …
fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let candidate = folder.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    (2..).map(|n| folder.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap()
}

/// Opens a file or folder, or shows it selected in Explorer.
fn reveal(path: &Path, select: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut explorer = std::process::Command::new("explorer");
        if select {
            explorer.raw_arg(format!("/select,\"{}\"", path.display()));
        } else {
            explorer.arg(path);
        }
        if let Err(err) = explorer.spawn() {
            log::warn!("couldn't open {}: {err}", path.display());
        }
    }
    #[cfg(not(windows))]
    let _ = (path, select);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_duplicate_file_names() {
        let dir = std::env::temp_dir().join(format!("rustplay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("photo.jpg"), b"x").unwrap();
        std::fs::write(dir.join("photo (2).jpg"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "photo.jpg"), dir.join("photo (3).jpg"));
        assert_eq!(unique_path(&dir, "new.png"), dir.join("new.png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn peer_file_names_cannot_escape_downloads() {
        assert_eq!(sanitize_path("../../evil/boot.ini", Rules::Windows), "boot.ini");
        assert_eq!(sanitize_path("Photos\\IMG_1.HEIC", Rules::Windows), "IMG_1.HEIC");
        assert!(!sanitize_path("CON", Rules::Windows).eq_ignore_ascii_case("con"));
    }

    #[test]
    fn received_names_lose_direction_marks() {
        assert_eq!(received_name("photo\u{202E}gpj.exe"), "photogpj.exe");
        assert_eq!(received_name("..\\\u{2067}evil\u{2069}.txt"), "evil.txt");
        assert!(!opens_directly(Path::new("photogpj.exe")));
        assert!(opens_directly(Path::new("IMG_1.HEIC")));
    }

    #[test]
    fn received_files_are_marked_as_downloaded() {
        let file = std::env::temp_dir().join(format!("rustplay-motw-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        mark_from_internet(&file);
        let mut stream = file.as_os_str().to_owned();
        stream.push(":Zone.Identifier");
        assert!(std::fs::read_to_string(stream).unwrap().contains("ZoneId=3"));
        std::fs::remove_file(&file).unwrap();
    }
}
