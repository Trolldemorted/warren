//! §D Milestone 5 — Phase B wire round-trip for `ScreenSnapshot` /
//! `SnapshotRequest`.
//!
//! These two envelopes travel across the rabbit↔warren link: warren sends
//! `SnapshotRequest { chan }` after the replay buffer lands in a fresh
//! xterm.js pane; rabbit answers with a `ScreenSnapshot { cols, rows,
//! cursor_*, text }` synthesized from its server-side `TermTracker`.
//!
//! The `TermTracker` itself is covered by `vt::tests::*` (6 unit tests,
//! including cursor + grid + UTF-8 reassembly). What this file pins down is
//! the wire format: the envelopes serialize and deserialize cleanly across
//! a real WebSocket through the `Link`, and the channel byte / field names
//! match what `warren/src/agents_live/wire.rs` expects. A failure here
//! means warren's `applyMeta` (or `envelope_kind`) would silently misroute
//! the snapshot.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use uuid::Uuid;

use rabbit::link::{Link, LinkCmd, LinkEvent, ReplaySnapFn};
use rabbit::meta_ring::MetaRing;
use rabbit::wire::{
    Envelope, EnvelopeBody, ScreenSnapshotBody, TermSize, PROTOCOL_VERSION, TERM_CHAN_CLAUDE,
};

type Ws = WebSocketStream<TcpStream>;

async fn accept(listener: &TcpListener) -> Ws {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("timed out waiting for link to connect")
        .expect("accept");
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("ws handshake")
}

async fn next_env(ws: &mut Ws) -> Envelope {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("stream ended unexpectedly")
            .expect("ws read error");
        match msg {
            Message::Text(t) => return serde_json::from_str(&t).expect("bad envelope json"),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => panic!("unexpected close from link"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

async fn send_env(ws: &mut Ws, body: EnvelopeBody) {
    let env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 0,
        body,
    };
    ws.send(Message::Text(serde_json::to_string(&env).unwrap()))
        .await
        .expect("send to link");
}

fn spawn_link(port: u16) -> (mpsc::Sender<LinkCmd>, mpsc::Receiver<LinkEvent>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<LinkCmd>(128);
    let (event_tx, event_rx) = mpsc::channel::<LinkEvent>(128);
    let ring = Arc::new(MetaRing::new(262_144));
    let replay_snap: ReplaySnapFn = Arc::new(Vec::new);

    let link = Link::new(
        format!("http://127.0.0.1:{port}"),
        "test-token".into(),
        Uuid::nil(),
        "test-1.0".into(),
        TermSize { cols: 80, rows: 24 },
        cmd_rx,
        event_tx,
        replay_snap,
        ring.clone(),
        // §D Milestone 5: tests don't care about the recorder URL.
        None,
    );
    tokio::spawn(async move {
        let _ = link.run().await;
    });
    (cmd_tx, event_rx)
}

/// `SnapshotRequest { chan }` from the fake warren side must reach rabbit's
/// inbound handler — the supervisor converts it into a `PtyCmd::Snapshot`,
/// and the blocking PTY thread reads `vt.snapshot()`. The link doesn't
/// generate the response itself; what it does is forward the request out the
/// other side as `LinkEvent::Text(env)`. This test pins that contract so a
/// future refactor that drops the envelope or renames the field would fail
/// loudly.
#[tokio::test]
async fn snapshot_request_arrives_as_text_event_with_chan_byte() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (cmd_tx, mut event_rx) = spawn_link(port);

    let mut ws = accept(&listener).await;
    let hello = next_env(&mut ws).await;
    assert!(matches!(hello.body, EnvelopeBody::Hello(_)));

    // Server-to-server: warren would push SnapshotRequest through the actor.
    send_env(
        &mut ws,
        EnvelopeBody::SnapshotRequest {
            chan: TERM_CHAN_CLAUDE,
        },
    )
    .await;

    let got = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("timed out waiting for the inbound snapshot request")
        .expect("event channel closed");
    match got {
        LinkEvent::Text(env) => match env.body {
            EnvelopeBody::SnapshotRequest { chan } => {
                assert_eq!(
                    chan,
                    TERM_CHAN_CLAUDE,
                    "channel byte must survive the wire intact"
                );
            }
            other => panic!("expected SnapshotRequest, got {other:?}"),
        },
        LinkEvent::Binary { .. } => panic!("snapshot request arrived as binary"),
    }

    // Silence the unused-sender lint by holding the cmd_tx alive until the
    // test exits (the link task owns the corresponding receiver).
    drop(cmd_tx);
}

/// A `ScreenSnapshot` produced by rabbit's blocking PTY thread rides back to
/// warren as a structured meta envelope via `LinkCmd::SendMeta`. The link
/// assigns a `seq` and the broker stores it; on the wire the JSON must
/// carry the channel byte, dimensions, cursor, and grid text in the exact
/// shape `warren/src/agents_live/wire.rs::ScreenSnapshotBody` parses.
#[tokio::test]
async fn screen_snapshot_serializes_with_all_fields_and_correct_tag() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (cmd_tx, _event_rx) = spawn_link(port);

    let mut ws = accept(&listener).await;
    let hello = next_env(&mut ws).await;
    assert!(matches!(hello.body, EnvelopeBody::Hello(_)));

    // Compose a snapshot that mirrors what `TermTracker::snapshot()` would
    // produce for a 4×3 VT showing "hi" + "yo" with cursor on row 1 col 0.
    let snap = ScreenSnapshotBody {
        chan: TERM_CHAN_CLAUDE,
        cols: 4,
        rows: 3,
        cursor_col: 0,
        cursor_row: 1,
        cursor_visible: true,
        text: vec!["hi  ".into(), "yo  ".into(), "    ".into()],
    };
    cmd_tx
        .send(LinkCmd::SendMeta(EnvelopeBody::ScreenSnapshot(snap.clone())))
        .await
        .expect("send snapshot to link");

    let frame = next_env(&mut ws).await;
    assert_eq!(frame.v, PROTOCOL_VERSION);
    assert!(
        frame.seq > hello.seq,
        "snapshot must carry a fresh seq past the hello"
    );
    match frame.body {
        EnvelopeBody::ScreenSnapshot(body) => {
            assert_eq!(body.chan, TERM_CHAN_CLAUDE);
            assert_eq!(body.cols, 4);
            assert_eq!(body.rows, 3);
            assert_eq!(body.cursor_col, 0);
            assert_eq!(body.cursor_row, 1);
            assert!(body.cursor_visible);
            assert_eq!(body.text.len(), 3);
            assert_eq!(body.text[0], "hi  ");
            assert_eq!(body.text[1], "yo  ");
            assert_eq!(body.text[2], "    ");
        }
        other => panic!("expected ScreenSnapshot, got {other:?}"),
    }
}

/// `envelope_kind` on the warren side classifies these envelopes as
/// `"snapshot_request"` and `"screen_snapshot"`. The link doesn't run that
/// classifier (warren owns persistence), but the JSON tag warren sees must
/// be exactly `t: "snapshot_request"` / `t: "screen_snapshot"` so the
/// `serde(tag = "t", rename_all = "snake_case")` derive resolves them.
#[tokio::test]
async fn envelope_tags_are_snake_case_to_match_warren_derive() {
    // Direct serde check: serialize each envelope, parse the JSON back as
    // a generic `serde_json::Value`, and assert the `t` field is exactly
    // what warren's `EnvelopeBody::deserialize` expects.
    let req_env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 1,
        body: EnvelopeBody::SnapshotRequest {
            chan: TERM_CHAN_CLAUDE,
        },
    };
    let req_json = serde_json::to_value(&req_env).unwrap();
    assert_eq!(
        req_json["t"], "snapshot_request",
        "warren uses snake_case tag"
    );
    assert_eq!(req_json["chan"], 0x01);

    let snap_env = Envelope {
        v: PROTOCOL_VERSION,
        seq: 2,
        body: EnvelopeBody::ScreenSnapshot(ScreenSnapshotBody {
            chan: TERM_CHAN_CLAUDE,
            cols: 2,
            rows: 1,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: false,
            text: vec!["ok".into()],
        }),
    };
    let snap_json = serde_json::to_value(&snap_env).unwrap();
    assert_eq!(snap_json["t"], "screen_snapshot");
    assert_eq!(snap_json["cols"], 2);
    assert_eq!(snap_json["cursor_visible"], false);
    assert_eq!(snap_json["text"][0], "ok");
}