//! WebSocket bridge between the Bevy app (single-threaded ECS) and the async
//! networking runtime, using channels.
//!
//! Outgoing `ClientMsg`s go through a tokio unbounded channel (its `send` is
//! synchronous, so it's callable from ECS systems). Incoming `ServerMsg`s come
//! back over a crossbeam channel that systems drain each frame with `try_recv`.

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use futures_util::{SinkExt, StreamExt};
use soils_protocol::{ClientMsg, ServerMsg, decode, encode};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_tungstenite::tungstenite::Message;

const SERVER_URL: &str = "ws://127.0.0.1:9001";

/// Channels to talk to the server. Inserted as a Bevy resource.
#[derive(Resource)]
pub struct NetClient {
    to_server: UnboundedSender<ClientMsg>,
    from_server: Receiver<ServerMsg>,
}

impl NetClient {
    /// Queue a message to the server. Silently dropped if the link is down.
    pub fn send(&self, msg: ClientMsg) {
        let _ = self.to_server.send(msg);
    }

    /// Non-blocking drain of all pending server messages.
    pub fn drain(&self) -> impl Iterator<Item = ServerMsg> + '_ {
        self.from_server.try_iter()
    }
}

/// Spawn the networking thread and return the ECS-side channel handles.
pub fn connect() -> NetClient {
    let (to_tx, mut to_rx) = unbounded_channel::<ClientMsg>();
    let (from_tx, from_rx): (Sender<ServerMsg>, Receiver<ServerMsg>) = unbounded();

    std::thread::Builder::new()
        .name("soils-net".into())
        .spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async move {
                let (ws, _) = match tokio_tungstenite::connect_async(SERVER_URL).await {
                    Ok(ok) => ok,
                    Err(e) => {
                        eprintln!("failed to connect to {SERVER_URL}: {e}");
                        return;
                    }
                };
                info!("connected to {SERVER_URL}");
                let (mut ws_tx, mut ws_rx) = ws.split();

                // Reader: server -> ECS.
                let reader = tokio::spawn(async move {
                    while let Some(Ok(frame)) = ws_rx.next().await {
                        if let Message::Binary(bytes) = frame {
                            if let Some(msg) = decode::<ServerMsg>(bytes.as_ref()) {
                                if from_tx.send(msg).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });

                // Writer: ECS -> server.
                while let Some(msg) = to_rx.recv().await {
                    if ws_tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
                        break;
                    }
                }
                reader.abort();
            });
        })
        .expect("spawn net thread");

    NetClient { to_server: to_tx, from_server: from_rx }
}
