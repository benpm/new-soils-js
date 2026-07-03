//! WebSocket bridge between the Bevy app (single-threaded ECS) and the async
//! networking runtime, using channels.
//!
//! The connection is *deferred*: the `soils-net` thread is spawned at startup
//! but stays idle until the user picks a server (via [`NetClient::connect`]),
//! so the address can be chosen at runtime on the login screen. Outgoing
//! `ClientMsg`s go through a tokio unbounded channel (its `send` is synchronous,
//! so it's callable from ECS systems) and buffer until the link is up. Incoming
//! events come back over a crossbeam channel that systems drain each frame.

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, ServerMsg, decode, encode};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_tungstenite::tungstenite::Message;

/// A client-side networking event. Connection status is client-local and never
/// crosses the wire, so it is carried here rather than in `ServerMsg`.
pub enum NetEvent {
    /// The WebSocket handshake to the chosen server succeeded.
    Connected,
    /// The connection attempt failed (host unreachable, bad address, …).
    ConnectFailed(String),
    /// A decoded message from the server.
    Msg(ServerMsg),
}

/// Channels to talk to the server. Inserted as a Bevy resource.
#[derive(Resource)]
pub struct NetClient {
    to_server: UnboundedSender<ClientMsg>,
    connect_to: UnboundedSender<String>,
    events: Receiver<NetEvent>,
}

impl NetClient {
    /// Queue a message to the server. Silently dropped if the link is down.
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.to_server.send(msg);
    }

    /// Ask the networking thread to (re)connect to a WebSocket URL. Any messages
    /// queued via [`send`](Self::send) before the link is up buffer and flush
    /// once connected.
    pub fn connect(&self, url: String) {
        let _ = self.connect_to.send(url);
    }

    /// Non-blocking drain of all pending networking events.
    pub fn drain(&self) -> impl Iterator<Item = NetEvent> + '_ {
        self.events.try_iter()
    }
}

/// Spawn the (initially idle) networking thread and return the ECS-side handles.
/// No connection is made until [`NetClient::connect`] is called.
pub fn connect() -> NetClient {
    let (to_tx, mut to_rx) = unbounded_channel::<ClientMsg>();
    let (ctrl_tx, mut ctrl_rx) = unbounded_channel::<String>();
    let (from_tx, from_rx): (Sender<NetEvent>, Receiver<NetEvent>) = unbounded();

    std::thread::Builder::new()
        .name("soils-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                // Each pass: wait for a target URL, run one session, then loop
                // back for the next connect request (e.g. after a failure or a
                // dropped link).
                loop {
                    let url = match ctrl_rx.recv().await {
                        Some(u) => u,
                        None => return, // ECS side dropped; shut down.
                    };

                    let (ws, _) = match tokio_tungstenite::connect_async(&url).await {
                        Ok(ok) => ok,
                        Err(e) => {
                            let _ = from_tx.send(NetEvent::ConnectFailed(format!("{e}")));
                            continue;
                        }
                    };
                    info!("connected to {url}");
                    let _ = from_tx.send(NetEvent::Connected);
                    let (mut ws_tx, mut ws_rx) = ws.split();

                    // Reader: server -> ECS.
                    let rtx = from_tx.clone();
                    let reader = tokio::spawn(async move {
                        while let Some(Ok(frame)) = ws_rx.next().await {
                            if let Message::Binary(bytes) = frame {
                                if let Some(msg) = decode::<ServerMsg>(bytes.as_ref()) {
                                    if rtx.send(NetEvent::Msg(msg)).is_err() {
                                        break;
                                    }
                                }
                            }
                        }
                    });

                    // Writer: ECS -> server. Borrows `to_rx` so any queued (and
                    // future) messages survive into the next session.
                    while let Some(msg) = to_rx.recv().await {
                        if ws_tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
                            break;
                        }
                    }
                    reader.abort();
                }
            });
        })
        .expect("spawn net thread");

    NetClient { to_server: to_tx, connect_to: ctrl_tx, events: from_rx }
}
