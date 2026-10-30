//! The AirPlay control connection: RTSP/1.0 requests on the main port that walk
//! through /info, pairing, FairPlay, SETUP of the streams, and TEARDOWN.

use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use plist::{Dictionary, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::code::CodeGate;
use super::hub::Hub;
use super::pairing::{self, Identity, Verify};
use super::{Receiver, audio, fairplay, mdns, mirror, timing};

const MAX_BODY: usize = 1024 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_LINE: usize = 8 * 1024;
const PLIST: &str = "application/x-apple-binary-plist";
const OCTETS: &str = "application/octet-stream";

/// Advertised display; the client scales its screen to fit inside it.
const DISPLAY_WIDTH: u64 = 1920;
const DISPLAY_HEIGHT: u64 = 1080;
const MAX_FPS: u64 = 60;

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

pub struct Context {
    pub identity: Identity,
    pub receiver: Receiver,
    pub name: std::sync::RwLock<String>,
    pub hub: Arc<Hub>,
    pub output: Arc<audio::Output>,
    pub code: CodeGate,
}

pub async fn serve(listener: TcpListener, ctx: Arc<Context>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    log::info!("control connection from {peer}");
                    if let Err(err) = Connection::new(ctx, peer).run(stream).await {
                        log::info!("control connection {peer} closed: {err}");
                    }
                });
            }
            Err(err) => log::warn!("accept failed: {err}"),
        }
    }
}

struct Request {
    method: String,
    url: String,
    protocol: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    fn plist(&self) -> Option<Dictionary> {
        Value::from_reader(Cursor::new(&self.body)).ok()?.into_dictionary()
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    close: bool,
}

impl Response {
    fn ok() -> Self {
        Response { status: 200, reason: "OK", headers: Vec::new(), body: Vec::new(), close: false }
    }

    fn error(status: u16, reason: &'static str) -> Self {
        Response { status, reason, ..Self::ok() }
    }

    fn with_body(content_type: &'static str, body: Vec<u8>) -> Self {
        let mut response = Self::ok();
        response.headers.push(("Content-Type", content_type.into()));
        response.body = body;
        response
    }

    fn plist(dict: Dictionary) -> Self {
        let mut body = Vec::new();
        plist::to_writer_binary(&mut body, &Value::Dictionary(dict)).expect("plist serialization");
        Self::with_body(PLIST, body)
    }
}

struct Connection {
    ctx: Arc<Context>,
    session: u64,
    peer: SocketAddr,
    local_ip: IpAddr,
    verify: Option<Verify>,
    fairplay_keymsg: Option<[u8; 164]>,
    aeskey: Option<[u8; 16]>,
    aesiv: [u8; 16],
    device: String,
    timing: Option<JoinHandle<()>>,
    audio: Option<JoinHandle<()>>,
    mirror: Option<JoinHandle<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop_streams();
        self.ctx.hub.end(self.session);
    }
}

impl Connection {
    fn new(ctx: Arc<Context>, peer: SocketAddr) -> Self {
        Connection {
            ctx,
            session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
            peer,
            local_ip: IpAddr::from([0, 0, 0, 0]),
            verify: None,
            fairplay_keymsg: None,
            aeskey: None,
            aesiv: [0; 16],
            device: "iPhone".into(),
            timing: None,
            audio: None,
            mirror: None,
        }
    }

    async fn run(mut self, stream: TcpStream) -> anyhow::Result<()> {
        stream.set_nodelay(true)?;
        self.local_ip = stream.local_addr()?.ip();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut kicked = self.ctx.hub.kicked();
        loop {
            let request = tokio::select! {
                request = read_request(&mut reader) => request?,
                _ = kicked.recv() => {
                    if self.mirror.is_some() {
                        log::info!("stopping mirroring at the user's request");
                        return Ok(());
                    }
                    continue;
                }
            };
            let Some(request) = request else { return Ok(()) };
            log::debug!("{} {} ({} bytes)", request.method, request.url, request.body.len());
            let response = self.handle(&request).await;
            let close = response.close;
            writer.write_all(&encode(&request, response)).await?;
            if close {
                return Ok(());
            }
        }
    }

    async fn handle(&mut self, req: &Request) -> Response {
        match (req.method.as_str(), req.url.as_str()) {
            ("GET", url) if url.contains("/info") => self.info(req),
            ("POST", "/pair-setup") => self.pair_setup(req),
            ("POST", "/pair-verify") => self.pair_verify(req),
            ("POST", "/fp-setup") => self.fp_setup(req),
            ("POST", _) => Response::ok(), // /feedback heartbeats, /audioMode, ...
            ("OPTIONS", _) => {
                let mut response = Response::ok();
                response
                    .headers
                    .push(("Public", "SETUP, RECORD, FLUSH, TEARDOWN, OPTIONS, GET_PARAMETER, SET_PARAMETER".into()));
                response
            }
            ("SETUP", _) => self.setup(req).await,
            // These act on the shared audio output, so they wait for the connection code.
            ("SET_PARAMETER" | "FLUSH", _) if self.aeskey.is_none() && self.ctx.code.required() => {
                Response::error(401, "Unauthorized")
            }
            ("GET_PARAMETER", _) => {
                if req.body.starts_with(b"volume") {
                    Response::with_body("text/parameters", b"volume: 0.000000\r\n".to_vec())
                } else {
                    Response::ok()
                }
            }
            ("SET_PARAMETER", _) => {
                // The client's own volume buttons arrive as "volume: <dB>".
                let text = String::from_utf8_lossy(&req.body);
                if let Some(db) = text.strip_prefix("volume:").and_then(|v| v.trim().parse::<f32>().ok()) {
                    self.ctx.output.set_client_volume_db(db);
                }
                Response::ok()
            }
            ("FLUSH", _) => {
                self.ctx.output.flush();
                Response::ok()
            }
            ("RECORD", _) => {
                let mut response = Response::ok();
                response.headers.push(("Audio-Latency", "0".into()));
                response.headers.push(("Audio-Jack-Status", "connected; type=analog".into()));
                response
            }
            ("TEARDOWN", _) => self.teardown(req),
            _ => {
                log::warn!("unhandled request {} {}", req.method, req.url);
                Response::error(501, "Not Implemented")
            }
        }
    }

    fn info(&self, req: &Request) -> Response {
        let receiver = &self.ctx.receiver;
        let code_required = self.ctx.code.required();
        let airplay_txt = mdns::txt_wire(&mdns::airplay_txt(receiver, code_required));

        // The first /info carries a plist asking only for the AirPlay TXT record.
        if req.header("Content-Type").is_some_and(|t| t.contains(PLIST)) {
            let mut dict = Dictionary::new();
            dict.insert("txtAirPlay".into(), Value::Data(airplay_txt));
            return Response::plist(dict);
        }

        let mut dict = Dictionary::new();
        dict.insert("deviceID".into(), receiver.device_id_string().into());
        dict.insert("macAddress".into(), receiver.device_id_string().into());
        dict.insert("pk".into(), Value::Data(receiver.public_key.to_vec()));
        dict.insert("features".into(), mdns::features(code_required).into());
        dict.insert("name".into(), self.ctx.name.read().unwrap().clone().into());
        dict.insert("pi".into(), mdns::PI.into());
        dict.insert("vv".into(), 2u64.into());
        dict.insert("statusFlags".into(), 68u64.into());
        dict.insert("keepAliveLowPower".into(), true.into());
        dict.insert("keepAliveSendStatsAsBody".into(), true.into());
        dict.insert("sourceVersion".into(), mdns::SOURCE_VERSION.into());
        dict.insert("model".into(), mdns::MODEL.into());
        dict.insert("initialVolume".into(), 0.0f64.into());

        let audio = |kind: u64, entries: &[(&str, Value)]| {
            let mut d = Dictionary::new();
            d.insert("type".into(), kind.into());
            for (key, value) in entries {
                d.insert((*key).into(), value.clone());
            }
            Value::Dictionary(d)
        };
        let latency = [
            ("audioType", Value::from("default")),
            ("inputLatencyMicros", 0u64.into()),
            ("outputLatencyMicros", false.into()),
        ];
        let formats = [("audioInputFormats", Value::from(0x3ff_fffcu64)), ("audioOutputFormats", 0x3ff_fffcu64.into())];
        dict.insert("audioLatencies".into(), Value::Array(vec![audio(100, &latency), audio(101, &latency)]));
        dict.insert("audioFormats".into(), Value::Array(vec![audio(100, &formats), audio(101, &formats)]));

        let mut display = Dictionary::new();
        display.insert("uuid".into(), "e0ff8a27-6738-3d56-8a16-cc53aacee925".into());
        display.insert("widthPhysical".into(), 0u64.into());
        display.insert("heightPhysical".into(), 0u64.into());
        display.insert("width".into(), DISPLAY_WIDTH.into());
        display.insert("height".into(), DISPLAY_HEIGHT.into());
        display.insert("widthPixels".into(), DISPLAY_WIDTH.into());
        display.insert("heightPixels".into(), DISPLAY_HEIGHT.into());
        display.insert("rotation".into(), false.into());
        display.insert("refreshRate".into(), (1.0 / MAX_FPS as f64).into());
        display.insert("maxFPS".into(), MAX_FPS.into());
        display.insert("overscanned".into(), false.into());
        display.insert("features".into(), 14u64.into());
        dict.insert("displays".into(), Value::Array(vec![Value::Dictionary(display)]));

        Response::plist(dict)
    }

    fn pair_setup(&self, req: &Request) -> Response {
        if req.body.len() != 32 {
            return Response::error(400, "Bad Request");
        }
        Response::with_body(OCTETS, self.ctx.identity.public_key().to_vec())
    }

    fn pair_verify(&mut self, req: &Request) -> Response {
        match req.body.first() {
            Some(1) => match pairing::verify_start(&self.ctx.identity, &req.body) {
                Some((reply, verify)) => {
                    self.verify = Some(verify);
                    Response::with_body(OCTETS, reply)
                }
                None => Response::error(400, "Bad Request"),
            },
            Some(0) => {
                match &self.verify {
                    Some(verify) if pairing::verify_finish(verify, &req.body) => {}
                    _ => log::warn!("pair-verify: client signature did not check out"),
                }
                Response::with_body(OCTETS, Vec::new())
            }
            _ => Response::error(400, "Bad Request"),
        }
    }

    fn fp_setup(&mut self, req: &Request) -> Response {
        match req.body.len() {
            16 => match fairplay::setup_reply(&req.body) {
                Some(reply) => Response::with_body(OCTETS, reply),
                None => Response::error(501, "Not Implemented"),
            },
            164 => match fairplay::handshake_reply(&req.body) {
                Some((reply, keymsg)) => {
                    self.fairplay_keymsg = Some(keymsg);
                    Response::with_body(OCTETS, reply)
                }
                None => Response::error(501, "Not Implemented"),
            },
            _ => Response::error(400, "Bad Request"),
        }
    }

    async fn setup(&mut self, req: &Request) -> Response {
        let Some(body) = req.plist() else { return Response::error(400, "Bad Request") };
        let mut out = Dictionary::new();

        // First SETUP: session keys and timing.
        if let (Some(ekey), Some(eiv)) = (data(&body, "ekey"), data(&body, "eiv")) {
            // A connection that already has its keys stays admitted.
            if self.aeskey.is_none() {
                let name = body.get("name").and_then(Value::as_string).unwrap_or(&self.device);
                if let Err(challenge) = self.ctx.code.admit(&req.method, req.header("Authorization"), name) {
                    let mut response = Response::error(401, "Unauthorized");
                    response.headers.push(("WWW-Authenticate", challenge));
                    return response;
                }
            }

            let (Ok(ekey), Ok(eiv), Some(keymsg)) =
                (<[u8; 72]>::try_from(ekey), <[u8; 16]>::try_from(eiv), self.fairplay_keymsg.as_ref())
            else {
                log::warn!("SETUP without a usable FairPlay key");
                return Response::error(400, "Bad Request");
            };
            let aeskey = fairplay::decrypt_key(keymsg, &ekey);
            self.aeskey = Some(pairing::salt_aes_key(aeskey, self.verify.as_ref().map(|v| &v.shared)));
            self.aesiv = eiv;

            if let Some(name) = body.get("name").and_then(Value::as_string) {
                self.device = name.to_string();
            }
            log::info!(
                "{} ({}) is connecting",
                self.device,
                body.get("model").and_then(Value::as_string).unwrap_or("unknown model")
            );
            self.ctx.hub.begin(self.session, self.device.clone());

            let remote_timing = body
                .get("timingPort")
                .and_then(Value::as_unsigned_integer)
                .filter(|port| *port > 0 && *port <= u16::MAX as u64)
                .map(|port| SocketAddr::new(self.peer.ip(), port as u16));
            match timing::start(self.local_ip, remote_timing).await {
                Ok((port, task)) => {
                    replace_task(&mut self.timing, task);
                    out.insert("timingPort".into(), (port as u64).into());
                }
                Err(err) => {
                    log::error!("timing port: {err}");
                    return Response::error(500, "Internal Server Error");
                }
            }
            out.insert("eventPort".into(), 0u64.into());
        }

        // Later SETUPs: the streams themselves.
        if let Some(streams) = body.get("streams").and_then(Value::as_array) {
            let mut replies = Vec::new();
            for stream in streams.iter().filter_map(Value::as_dictionary) {
                let kind = stream.get("type").and_then(Value::as_unsigned_integer).unwrap_or(0);
                let mut reply = Dictionary::new();
                reply.insert("type".into(), kind.into());
                match kind {
                    110 => {
                        let Some(aeskey) = self.aeskey else {
                            log::warn!("mirroring SETUP before key exchange");
                            return Response::error(400, "Bad Request");
                        };
                        let stream_id = stream_connection_id(stream);
                        log::debug!("streamConnectionID {stream_id}");
                        self.ctx.hub.reclaim(self.session, &self.device);
                        let hub = self.ctx.hub.clone();
                        match mirror::listen(self.local_ip, aeskey, stream_id, hub, self.session).await {
                            Ok((port, task)) => {
                                replace_task(&mut self.mirror, task);
                                reply.insert("dataPort".into(), (port as u64).into());
                            }
                            Err(err) => {
                                log::error!("mirror port: {err}");
                                return Response::error(500, "Internal Server Error");
                            }
                        }
                    }
                    96 => {
                        let Some(aeskey) = self.aeskey else {
                            log::warn!("audio SETUP before key exchange");
                            return Response::error(400, "Bad Request");
                        };
                        let ct = stream.get("ct").and_then(Value::as_unsigned_integer).unwrap_or(0);
                        log::info!("audio stream, format ct={ct}");
                        let output = self.ctx.output.clone();
                        match audio::listen(self.local_ip, aeskey, self.aesiv, ct, output).await {
                            Ok((data_port, control_port, task)) => {
                                replace_task(&mut self.audio, task);
                                reply.insert("dataPort".into(), (data_port as u64).into());
                                reply.insert("controlPort".into(), (control_port as u64).into());
                            }
                            Err(err) => {
                                log::error!("audio ports: {err}");
                                return Response::error(500, "Internal Server Error");
                            }
                        }
                    }
                    other => {
                        log::warn!("SETUP for unknown stream type {other}");
                        continue;
                    }
                }
                replies.push(Value::Dictionary(reply));
            }
            out.insert("streams".into(), Value::Array(replies));
        }

        Response::plist(out)
    }

    fn teardown(&mut self, req: &Request) -> Response {
        let types: Vec<u64> = req
            .plist()
            .and_then(|body| body.get("streams").and_then(Value::as_array).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|s| s.as_dictionary()?.get("type")?.as_unsigned_integer())
            .collect();
        let mut response = Response::ok();
        response.headers.push(("Connection", "close".into()));

        if types.contains(&96) {
            abort(&mut self.audio);
            self.ctx.output.flush();
        } else if types.contains(&110) {
            abort(&mut self.mirror);
            self.ctx.hub.end(self.session);
        } else {
            self.stop_streams();
            self.ctx.hub.end(self.session);
        }
        response
    }

    fn stop_streams(&mut self) {
        abort(&mut self.mirror);
        if self.audio.is_some() {
            abort(&mut self.audio);
            self.ctx.output.flush();
        }
        abort(&mut self.timing);
    }
}

/// The stream ID salts the video keys as an unsigned decimal; iOS may send it
/// as a negative signed integer, which means the same 64 bits.
fn stream_connection_id(stream: &Dictionary) -> u64 {
    stream
        .get("streamConnectionID")
        .and_then(|v| v.as_unsigned_integer().or(v.as_signed_integer().map(|i| i as u64)))
        .unwrap_or(0)
}

fn data<'a>(dict: &'a Dictionary, key: &str) -> Option<&'a [u8]> {
    dict.get(key).and_then(Value::as_data)
}

fn replace_task(slot: &mut Option<JoinHandle<()>>, task: JoinHandle<()>) {
    abort(slot);
    *slot = Some(task);
}

fn abort(slot: &mut Option<JoinHandle<()>>) {
    if let Some(task) = slot.take() {
        task.abort();
    }
}

/// Reads one line, refusing any longer than MAX_LINE so a client can't make it grow without end.
async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, line: &mut String) -> std::io::Result<usize> {
    line.clear();
    let read = (&mut *reader).take(MAX_LINE as u64).read_line(line).await?;
    if read == MAX_LINE && !line.ends_with('\n') {
        return Err(std::io::Error::other("request line too long"));
    }
    Ok(read)
}

async fn read_request<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    loop {
        if read_line(reader, &mut line).await? == 0 {
            return Ok(None);
        }
        if !line.trim().is_empty() {
            break;
        }
    }
    let mut parts = line.trim_end().splitn(3, ' ');
    let (Some(method), Some(url), Some(protocol)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(std::io::Error::other(format!("malformed request line {line:?}")));
    };
    let mut request = Request {
        method: method.to_string(),
        url: url.to_string(),
        protocol: protocol.to_string(),
        headers: Vec::new(),
        body: Vec::new(),
    };

    loop {
        if read_line(reader, &mut line).await? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if request.headers.len() >= MAX_HEADERS {
            return Err(std::io::Error::other("too many headers"));
        }
        if let Some((name, value)) = header.split_once(':') {
            request.headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let length: usize = request.header("Content-Length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if length > MAX_BODY {
        return Err(std::io::Error::other(format!("request body of {length} bytes is too large")));
    }
    request.body = vec![0; length];
    reader.read_exact(&mut request.body).await?;
    Ok(Some(request))
}

fn encode(request: &Request, response: Response) -> Vec<u8> {
    let mut head = format!("{} {} {}\r\n", request.protocol, response.status, response.reason);
    let cseq = request.header("CSeq");
    if cseq.is_some() && request.method != "RECORD" {
        head.push_str("Audio-Jack-Status: connected; type=digital\r\n");
    }
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("Server: AirTunes/{}\r\n", mdns::SOURCE_VERSION));
    if let Some(cseq) = cseq {
        head.push_str(&format!("CSeq: {cseq}\r\n"));
    }
    if !response.body.is_empty() {
        head.push_str(&format!("Content-Length: {}\r\n", response.body.len()));
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(&response.body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(hex_plist: &str) -> Dictionary {
        let bytes = hex::decode(hex_plist).unwrap();
        Value::from_reader(Cursor::new(bytes)).unwrap().into_dictionary().unwrap()
    }

    #[test]
    fn stream_id_signed_and_wide_encodings_agree() {
        // The same 64 bits, written as a negative 8-byte int and as a 16-byte int.
        let signed = parse(
            "62706c6973743030d101025f101273747265616d436f6e6e656374696f6e494413be560c4b72bedf99080b200000000000000101000000000000000300000000000000000000000000000029",
        );
        let wide = parse(
            "62706c6973743030d101025f101273747265616d436f6e6e656374696f6e4944140000000000000000be560c4b72bedf99080b200000000000000101000000000000000300000000000000000000000000000031",
        );
        assert_eq!(stream_connection_id(&signed), 13715163233390616473);
        assert_eq!(stream_connection_id(&wide), 13715163233390616473);
    }

    #[tokio::test]
    async fn refuses_endless_lines() {
        let flood = vec![b'A'; MAX_LINE * 4];
        assert!(read_request(&mut BufReader::new(flood.as_slice())).await.is_err());

        let request = b"OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let parsed = read_request(&mut BufReader::new(&request[..])).await.unwrap().unwrap();
        assert_eq!((parsed.method.as_str(), parsed.header("CSeq")), ("OPTIONS", Some("1")));
    }
}
