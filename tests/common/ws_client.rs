//! A WebSocket client for the streaming transcription route, for the two test
//! files that drive it. Included with `#[path = "common/ws_client.rs"]` rather
//! than through `common/mod.rs` on purpose (see `transcription_app.rs`).

#![allow(dead_code)]

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::common::transcription_app::{TranscriptionApp, TOKEN};

pub type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub fn ws_url(app: &TranscriptionApp) -> String {
    format!("{}/v1/audio/transcriptions/stream", app.base_url.replace("http://", "ws://"))
}

/// Open the route with `token` as the bearer (plus any extra headers).
/// Returns the handshake error for a refused upgrade.
pub async fn connect_with(
    app: &TranscriptionApp,
    token: Option<&str>,
    extra_headers: &[(&'static str, &str)],
) -> Result<ClientSocket, tokio_tungstenite::tungstenite::Error> {
    let mut request = ws_url(app).into_client_request().unwrap();
    if let Some(token) = token {
        request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    }
    for (name, value) in extra_headers {
        request.headers_mut().insert(*name, value.parse().unwrap());
    }
    tokio_tungstenite::connect_async(request).await.map(|(socket, _)| socket)
}

pub async fn connect(app: &TranscriptionApp) -> ClientSocket {
    connect_with(app, Some(TOKEN), &[]).await.expect("upgrade accepted")
}

/// The status of a refused upgrade, or a panic for any other failure.
pub fn refused_status(err: tokio_tungstenite::tungstenite::Error) -> u16 {
    match err {
    tokio_tungstenite::tungstenite::Error::Http(response) => response.status().as_u16(),
    other => panic!("expected an HTTP refusal, got {other:?}"),
    }
}

pub async fn send_json(socket: &mut ClientSocket, value: serde_json::Value) {
    socket.send(Message::Text(value.to_string())).await.unwrap();
}

pub async fn send_audio(socket: &mut ClientSocket, bytes: Vec<u8>) {
    socket.send(Message::Binary(bytes)).await.unwrap();
}

/// The next JSON text frame, skipping pings; `None` at close.
pub async fn next_json(socket: &mut ClientSocket) -> Option<serde_json::Value> {
    loop {
    match tokio::time::timeout(std::time::Duration::from_secs(20), socket.next()).await {
        Err(_) => panic!("no frame within 20s"),
        Ok(None) => return None,
        Ok(Some(Ok(Message::Text(text)))) => return Some(serde_json::from_str(&text).unwrap()),
        Ok(Some(Ok(Message::Close(_)))) => return None,
        Ok(Some(Ok(_))) => continue,
        Ok(Some(Err(e))) => panic!("socket error: {e}"),
    }
    }
}

/// Every JSON frame until the socket closes.
pub async fn drain_json(socket: &mut ClientSocket) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while let Some(v) = next_json(socket).await {
    out.push(v);
    }
    out
}

