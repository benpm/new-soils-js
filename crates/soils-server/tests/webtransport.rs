//! Scripted WebTransport scenario (plan-game-systems §3, phase 14): the same
//! embedded server accepts WT sessions on UDP at its game port, the reliable
//! lane is a client-opened bi stream of length-framed bincode, and snapshots
//! arrive as datagrams. This drives the full login → inputs → snapshot loop
//! over the new transport, mirroring `entities_move_by_inputs...` from the
//! WebSocket scenarios.

mod common;

use std::time::Duration;

use common::TestServer;
use soils_protocol::{ClientMsg, InputFrame, ServerMsg, SnapshotTracker, decode, encode};
use wtransport::{ClientConfig, Endpoint, RecvStream, SendStream};

async fn send_framed(tx: &mut SendStream, msg: &ClientMsg) {
    let bytes = encode(msg);
    let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&bytes);
    tx.write_all(&framed).await.expect("stream write");
}

async fn read_framed(rx: &mut RecvStream) -> ServerMsg {
    let mut len = [0u8; 4];
    read_exact(rx, &mut len).await;
    let mut buf = vec![0u8; u32::from_le_bytes(len) as usize];
    read_exact(rx, &mut buf).await;
    decode::<ServerMsg>(&buf).expect("server frame decodes")
}

async fn read_exact(rx: &mut RecvStream, buf: &mut [u8]) {
    let mut filled = 0;
    while filled < buf.len() {
        match rx.read(&mut buf[filled..]).await.expect("stream read") {
            Some(0) | None => panic!("stream closed mid-frame"),
            Some(n) => filled += n,
        }
    }
}

#[tokio::test]
async fn webtransport_clients_login_move_and_stream_snapshots() {
    let server = TestServer::start("webtransport");
    let port = server.addr().port();

    let config = ClientConfig::builder()
        .with_bind_default()
        .with_no_cert_validation()
        .build();
    let conn = Endpoint::client(config)
        .expect("wt client endpoint")
        .connect(format!("https://127.0.0.1:{port}"))
        .await
        .expect("wt connect (udp game port)");
    let (mut tx, mut rx) = conn.open_bi().await.expect("open").await.expect("bi stream");

    // Login over the reliable stream; Init comes back on it.
    send_framed(&mut tx, &ClientMsg::Login {
        name: "wt-alice".into(),
        password: String::new(),
        signup: true,
    })
    .await;
    let (self_entity, spawn) = loop {
        match tokio::time::timeout(Duration::from_secs(10), read_framed(&mut rx))
            .await
            .expect("timed out waiting for Init")
        {
            ServerMsg::Init { self_entity, spawn, .. } => break (self_entity, spawn),
            ServerMsg::LoginError { message } => panic!("login failed: {message}"),
            _ => {}
        }
    };

    // Keep the reliable stream drained (chunk bundles etc.) in the background.
    tokio::spawn(async move { while rx.read(&mut [0u8; 4096]).await.is_ok() {} });

    // Drive 16 forward ticks facing -Z through the *datagram* lane, acking
    // tick 0 (no baseline yet — snapshots are full until we ack).
    let frames: Vec<InputFrame> = (1..=16)
        .map(|seq| {
            let input = soils_sim::PlayerInput {
                move_axes: glam::Vec2::new(0.0, 1.0),
                ..Default::default()
            };
            let (buttons, flags, yaw) = soils_sim::pack_input(&input);
            InputFrame { seq, buttons, flags, yaw }
        })
        .collect();
    conn.send_datagram(encode(&ClientMsg::Inputs { ack_tick: 0, frames })).expect("datagram");

    // Snapshots arrive as datagrams; the server integrates 16/64 s × 8 u/s =
    // 2.0 units of -Z movement.
    let mut tracker = SnapshotTracker::default();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let moved = loop {
        let d = tokio::time::timeout_at(deadline, conn.receive_datagram())
            .await
            .expect("timed out waiting for snapshot datagrams")
            .expect("datagram receive");
        let Some(ServerMsg::Snapshot { tick, baseline_tick, payload, .. }) =
            decode::<ServerMsg>(d.payload().as_ref())
        else {
            continue;
        };
        let Some(updated) = tracker.apply(tick, baseline_tick, &payload) else { continue };
        if let Some(s) = updated.into_iter().find(|s| s.id == self_entity)
            && s.pos[2] < spawn[2] - 1.5
        {
            break s;
        }
    };
    assert!(
        (moved.pos[2] - (spawn[2] - 2.0)).abs() < 0.5
            && (moved.pos[0] - spawn[0]).abs() < 0.1,
        "server-integrated position should be ~2 units -Z from spawn, got {:?}",
        moved.pos
    );
}
