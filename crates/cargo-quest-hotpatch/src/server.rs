//! The dioxus-protocol devserver: WebSocket at `ws://<host>:<port>/_dioxus`.
//!
//! Bevy's built-in `connect_subsecond()` (dioxus-devtools) dials this path
//! with `?aslr_reference=..&build_id=..&pid=..` and consumes text JSON frames
//! of `DevserverMsg`. We emit `DevserverMsg::HotReload` holding the JumpTable.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::middleware::{self, Next};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::response::Response;
use axum::routing::get;
use dioxus_devtools_types::{DevserverMsg, HotReloadMsg};
use serde::Deserialize;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct ClientSession {
    pub pid: u32,
    pub build_id: u64,
    pub aslr_reference: u64,
}

#[derive(Clone)]
pub struct DevServer {
    pub session: Arc<std::sync::RwLock<Option<ClientSession>>>,
    pub tx: broadcast::Sender<String>,
}

impl DevServer {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self {
            session: Arc::new(std::sync::RwLock::new(None)),
            tx,
        }
    }

    pub fn current_session(&self) -> Option<ClientSession> {
        self.session.read().unwrap().clone()
    }

    /// Send a patch (or a "full reload" command) to whatever app is connected.
    pub fn push_msg(&self, msg: DevserverMsg) {
        let _ = self.tx.send(serde_json::to_string(&msg).unwrap());
    }

    pub fn request_full_reload(&self) {
        self.push_msg(DevserverMsg::FullReloadCommand);
    }

    pub fn send_patch_for(&self, aslr_reference: u64, jump_table: subsecond_types::JumpTable) {
        // for_pid must match the app's pid so its connect_subsecond applies it.
        let pid = self.current_session().map(|s| s.pid);
        self.push_msg(DevserverMsg::HotReload(HotReloadMsg {
            templates: vec![],
            assets: vec![],
            ms_elapsed: 0,
            jump_table: Some(jump_table),
            for_build_id: None,
            for_pid: pid,
        }));
        // keep aslr warm for future patches even if session vanished
        let _ = aslr_reference;
    }

    pub async fn serve(self, addr: std::net::SocketAddr) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/_dioxus", get(ws_handler))
            .layer(middleware::from_fn(log_every_request))
            .with_state(self);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("devserver listening on ws://{addr}/_dioxus");
        axum::serve(listener, app).await?;
        Ok(())
    }
}

#[derive(Deserialize, Default)]
struct ConnQuery {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    build_id: Option<u64>,
    #[serde(default)]
    aslr_reference: Option<u64>,
}

/// Log every request to the devserver, before extractors run, so we can see
/// even handshakes that later get rejected (silent 400s).
async fn log_every_request(
    req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    tracing::info!(
        method = %req.method(),
        uri = %req.uri(),
        upgrade = %req.headers()
            .get("upgrade").map(|v| v.to_str().unwrap_or("?")).unwrap_or("none"),
        "HTTP -> /_dioxus"
    );
    next.run(req).await
}

fn parse_query_manual(raw: &str) -> ConnQuery {
    let mut q = ConnQuery::default();
    for pair in raw.split('&') {
        let mut it = pair.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match k {
            "pid" => q.pid = v.parse().ok(),
            "build_id" => q.build_id = v.parse().ok(),
            "aslr_reference" => q.aslr_reference = v.parse().ok(),
            _ => {}
        }
    }
    q
}

async fn ws_handler(
    RawQuery(raw): RawQuery,
    ws: WebSocketUpgrade,
    State(s): State<DevServer>,
) -> Response {
    let q = raw.as_deref().map(parse_query_manual).unwrap_or_default();
    ws.on_upgrade(move |socket| handle_socket(socket, q, s))
}

async fn handle_socket(mut socket: WebSocket, q: ConnQuery, s: DevServer) {
    if let Some(aslr) = q.aslr_reference {
        *s.session.write().unwrap() = Some(ClientSession {
            pid: q.pid.unwrap_or(0),
            build_id: q.build_id.unwrap_or(0),
            aslr_reference: aslr,
        });
        tracing::info!(pid=?q.pid, aslr=%aslr, build_id=?q.build_id, "app connected");
    }
    let mut rx = s.tx.subscribe();
    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // ignore client->server chatter
                    Some(Err(_)) => break,
                }
            }
        }
    }
    tracing::info!("app disconnected");
    *s.session.write().unwrap() = None;
}
