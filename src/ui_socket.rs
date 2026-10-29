//! Localhost WebSocket between the backend and the window: status and H.264
//! data flow out, and commands (rename, volume, stop) come back in as JSON
//! `{"id": n, "type": ...}` with a `{"type": "reply", "id": n, ...}` answer.
//! A random token in the URL keeps other local pages from using it.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

use crate::airplay::hub::{Hub, UiMessage};

pub type Commands = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// Binds the socket and returns the URL the UI should connect to.
pub async fn start(hub: Arc<Hub>, commands: Commands) -> anyhow::Result<String> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
    let token = hex::encode(crate::airplay::random_token());
    let url = format!("ws://127.0.0.1:{}/stream?token={token}", listener.local_addr()?.port());
    log::debug!("ui stream at {url}");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let hub = hub.clone();
            let commands = commands.clone();
            let token = token.clone();
            tokio::spawn(async move {
                if let Err(err) = serve(stream, hub, commands, &token).await {
                    log::debug!("ui socket closed: {err}");
                }
            });
        }
    });
    Ok(url)
}

async fn serve(stream: TcpStream, hub: Arc<Hub>, commands: Commands, token: &str) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let expected = format!("token={token}");
    let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().query() == Some(expected.as_str()) {
            Ok(resp)
        } else {
            let mut denied = ErrorResponse::new(None);
            *denied.status_mut() = StatusCode::FORBIDDEN;
            Err(denied)
        }
    };
    let ws = tokio_tungstenite::accept_hdr_async(stream, check).await?;
    let (mut sink, mut incoming) = ws.split();

    // Commands are read on their own task, so mouse and key input never waits
    // behind a large video frame being written.
    let (replies, mut outgoing) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = incoming.next().await {
            let Message::Text(text) = message else { continue };
            let Ok(request) = serde_json::from_str::<Value>(&text) else { continue };
            let mut reply = commands(&request);
            // Requests without an id (mouse and key updates) want no answer.
            if request["id"].is_null() {
                continue;
            }
            reply["type"] = "reply".into();
            reply["id"] = request["id"].clone();
            if replies.send(Message::Text(reply.to_string().into())).is_err() {
                break;
            }
        }
    });

    let written: anyhow::Result<()> = async {
        let (initial, mut feed) = hub.subscribe();
        for message in initial {
            sink.send(to_ws(message)).await?;
        }
        loop {
            tokio::select! {
                message = feed.recv() => match message {
                    Ok(message) => sink.send(to_ws(message)).await?,
                    Err(RecvError::Lagged(skipped)) => log::warn!("ui fell behind, skipped {skipped} messages"),
                    Err(RecvError::Closed) => return Ok(()),
                },
                // None once the window has gone and the reader stopped.
                reply = outgoing.recv() => match reply {
                    Some(reply) => sink.send(reply).await?,
                    None => return Ok(()),
                },
            }
        }
    }
    .await;
    reader.abort();
    written
}

fn to_ws(message: UiMessage) -> Message {
    match message {
        UiMessage::Text(text) => Message::Text(text.into()),
        UiMessage::Binary(bytes) => Message::Binary(bytes),
    }
}
