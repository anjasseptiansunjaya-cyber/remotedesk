//! HTTPS + WebSocket: serves the web viewer, streams frames, receives input.

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{sync::watch, time::{sleep, timeout}};

use crate::{Host, capture::Gop};

const INDEX: &str = include_str!("../web/index.html");

#[derive(Clone)]
pub struct AppState {
    pub password: Arc<String>,
    pub host: Arc<Host>,
    pub frames: watch::Receiver<Arc<Gop>>,
    pub viewers: Arc<watch::Sender<usize>>,
    /// Pixel size of the encoded frames.
    pub frame_size: (i32, i32),
    pub codec: &'static str,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> impl IntoResponse {
    ws.max_message_size(64 * 1024).on_upgrade(move |s| session(s, st))
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum Msg {
    #[serde(rename = "auth")]
    Auth { pw: String },
    #[serde(rename = "m")]
    Move { x: f64, y: f64 },
    #[serde(rename = "b")]
    Button { b: u8, d: u8 },
    #[serde(rename = "w")]
    Wheel { dx: i32, dy: i32 },
    #[serde(rename = "k")]
    Key { c: String, d: u8 },
    #[serde(rename = "u")]
    Text { s: String },
}

struct ViewerGuard(Arc<watch::Sender<usize>>);
impl ViewerGuard {
    fn new(tx: Arc<watch::Sender<usize>>) -> Self {
        tx.send_modify(|n| *n += 1);
        Self(tx)
    }
}
impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

async fn session(mut sock: WebSocket, st: AppState) {
    if !authenticate(&mut sock, &st).await {
        return;
    }
    let _guard = ViewerGuard::new(st.viewers.clone());
    let (mut tx, mut rx) = sock.split();

    let mut frames = st.frames.clone();
    frames.mark_changed(); // deliver the current group to a late joiner
    let sender = async move {
        let (mut seen, mut sent) = (u64::MAX, 0usize);
        while frames.changed().await.is_ok() {
            let gop = frames.borrow_and_update().clone();
            if gop.id != seen {
                // New keyframe group: decoding restarts from its first frame.
                seen = gop.id;
                sent = 0;
            }
            for f in &gop.frames[sent.min(gop.frames.len())..] {
                if tx.send(Message::Binary(f.clone())).await.is_err() {
                    return;
                }
            }
            sent = gop.frames.len();
        }
    };

    let host = st.host.clone();
    let receiver = async move {
        while let Some(Ok(msg)) = rx.next().await {
            let Message::Text(t) = msg else { continue };
            let Ok(m) = serde_json::from_str::<Msg>(t.as_str()) else { continue };
            let r = match m {
                Msg::Move { x, y } => host.pointer_move(x, y).await,
                Msg::Button { b, d } => host.pointer_button(b, d != 0).await,
                Msg::Wheel { dx, dy } => host.scroll(dx, dy).await,
                Msg::Key { c, d } => host.key(&c, d != 0).await,
                Msg::Text { s } => host.type_text(&s).await,
                Msg::Auth { .. } => Ok(()),
            };
            if let Err(e) = r {
                eprintln!("input error: {e}");
            }
        }
    };

    tokio::select! { _ = sender => {}, _ = receiver => {} }
}

async fn authenticate(sock: &mut WebSocket, st: &AppState) -> bool {
    let first = timeout(Duration::from_secs(10), sock.recv()).await;
    let ok = match first {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str::<Msg>(t.as_str()) {
            Ok(Msg::Auth { pw }) => ct_eq(pw.as_bytes(), st.password.as_bytes()),
            _ => false,
        },
        _ => false,
    };
    if !ok {
        sleep(Duration::from_secs(1)).await; // slow down guessing
        let _ = sock.send(Message::Text(r#"{"t":"err"}"#.into())).await;
        return false;
    }
    let hello = format!(
        r#"{{"t":"ok","w":{},"h":{},"codec":"{}"}}"#,
        st.frame_size.0, st.frame_size.1, st.codec
    );
    sock.send(Message::Text(hello.into())).await.is_ok()
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
