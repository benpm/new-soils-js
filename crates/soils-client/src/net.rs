//! Network bridge between the Bevy app (single-threaded ECS) and the async
//! networking runtime, using channels. Two transports behind one URL scheme
//! switch (plan-game-systems §3): `ws://` (WebSocket, everything reliable)
//! and `wt://` (WebTransport/QUIC — inputs and snapshots ride datagrams, the
//! rest a reliable bi stream). The ECS side is transport-blind.
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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_tungstenite::tungstenite::Message;

/// Preferred URL scheme for bare `host:port` addresses: `wt` (WebTransport)
/// when `SOILS_WT=1`, `ws` otherwise. WebTransport is opt-in while it soaks —
/// the server always listens on both.
pub fn default_scheme() -> &'static str {
    if std::env::var("SOILS_WT").is_ok_and(|v| v != "0") { "wt" } else { "ws" }
}

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
                    if let Some(host) = url.strip_prefix("wt://") {
                        wt_session(host, &from_tx, &mut to_rx).await;
                    } else {
                        ws_session(&url, &from_tx, &mut to_rx).await;
                    }
                }
            });
        })
        .expect("spawn net thread");

    NetClient { to_server: to_tx, connect_to: ctrl_tx, events: from_rx }
}

/// One WebSocket session: connect, pump until either side closes. Borrows
/// `to_rx` so queued (and future) messages survive into the next session.
async fn ws_session(
    url: &str,
    from_tx: &Sender<NetEvent>,
    to_rx: &mut UnboundedReceiver<ClientMsg>,
) {
    let (ws, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(ok) => ok,
        Err(e) => {
            let _ = from_tx.send(NetEvent::ConnectFailed(format!("{e}")));
            return;
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

    // Writer: ECS -> server.
    while let Some(msg) = to_rx.recv().await {
        if ws_tx.send(Message::Binary(encode(&msg).into())).await.is_err() {
            break;
        }
    }
    reader.abort();
}

/// One WebTransport session (mirrors the server's `pump_wt_connection`):
/// the reliable lane is a client-opened bi stream of 4-byte-LE length-framed
/// bincode; inputs go out as datagrams (loss-tolerant — every `Inputs`
/// bundles the last 3 frames) and snapshot datagrams come back in. The
/// server's certificate is a per-boot self-signed identity, so verification
/// is skipped — LAN-play semantics, same trust model as plain `ws://`.
async fn wt_session(
    host: &str,
    from_tx: &Sender<NetEvent>,
    to_rx: &mut UnboundedReceiver<ClientMsg>,
) {
    let fail = |e: String| {
        let _ = from_tx.send(NetEvent::ConnectFailed(e));
    };
    let config = wtransport::ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();
    let endpoint = match wtransport::Endpoint::client(config) {
        Ok(e) => e,
        Err(e) => return fail(format!("{e}")),
    };
    let conn = match endpoint.connect(format!("https://{host}")).await {
        Ok(c) => c,
        Err(e) => return fail(format!("{e}")),
    };
    // Two awaits: flow-control admission, then stream initialization.
    let (mut stream_tx, mut stream_rx) = match conn.open_bi().await {
        Ok(opening) => match opening.await {
            Ok(s) => s,
            Err(e) => return fail(format!("{e}")),
        },
        Err(e) => return fail(format!("{e}")),
    };
    info!("connected to wt://{host} (QUIC datagram snapshots)");
    let _ = from_tx.send(NetEvent::Connected);

    // Reliable reader: length-framed ServerMsg until the stream closes.
    let rtx = from_tx.clone();
    let reader = tokio::spawn(async move {
        let mut len = [0u8; 4];
        loop {
            if read_exact(&mut stream_rx, &mut len).await.is_err() {
                break;
            }
            let n = u32::from_le_bytes(len) as usize;
            if n > MAX_WT_FRAME {
                break; // decode-bomb guard
            }
            let mut buf = vec![0u8; n];
            if read_exact(&mut stream_rx, &mut buf).await.is_err() {
                break;
            }
            if let Some(msg) = decode::<ServerMsg>(&buf)
                && rtx.send(NetEvent::Msg(msg)).is_err()
            {
                break;
            }
        }
    });
    // Datagram reader: snapshots on the unreliable lane.
    let dtx = from_tx.clone();
    let dconn = conn.clone();
    let dgrams = tokio::spawn(async move {
        while let Ok(d) = dconn.receive_datagram().await {
            if let Some(msg) = decode::<ServerMsg>(d.payload().as_ref())
                && dtx.send(NetEvent::Msg(msg)).is_err()
            {
                break;
            }
        }
    });

    // Writer: the input hot path rides datagrams; everything else (login,
    // edits, view radius) stays reliable and ordered.
    while let Some(msg) = to_rx.recv().await {
        if matches!(msg, ClientMsg::Inputs { .. }) {
            let _ = conn.send_datagram(encode(&msg));
        } else {
            let bytes = encode(&msg);
            let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&bytes);
            if stream_tx.write_all(&framed).await.is_err() {
                break;
            }
        }
    }
    reader.abort();
    dgrams.abort();
}

/// Largest accepted reliable-stream frame (chunk bundles are the biggest
/// message; a 16-chunk raw-dense bundle is ~0.5 MB).
const MAX_WT_FRAME: usize = 1 << 24;

/// Fill `buf` from a WT receive stream (`read` returns partial chunks).
async fn read_exact(rx: &mut wtransport::RecvStream, buf: &mut [u8]) -> Result<(), ()> {
    let mut filled = 0;
    while filled < buf.len() {
        match rx.read(&mut buf[filled..]).await {
            Ok(Some(n)) if n > 0 => filled += n,
            _ => return Err(()),
        }
    }
    Ok(())
}
