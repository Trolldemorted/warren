//! §D Milestone 4 — seq/ack meta replay across a WS reconnect.
//!
//! `MetaRing` (`meta_ring.rs`) buffers every structured meta event the link
//! sends, keyed by the seq stamped into its envelope. On each WS attempt the
//! link replays everything still buffered so a flapping connection never loses
//! an event warren hasn't acked. `EnvelopeBody::Ack { ack_seq }` from warren
//! trims the acked prefix.
//!
//! The unit tests in `meta_ring.rs` cover the ring in isolation; this file
//! exercises the *link* end-to-end against a real (tiny) WebSocket server:
//! it drives real meta events out, drops the connection mid-stream, and
//! asserts the buffered events replay on reconnect with their **original seq**.
//! A second test proves an `Ack` trims the ring so acked events don't replay.

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
    Envelope, EnvelopeBody, LogLine, StateFrame, TermSize, UsageSnapshot, PROTOCOL_VERSION,
};

type Ws = WebSocketStream<TcpStream>;

/// Accept one WS connection from the link under test.
async fn accept(listener: &TcpListener) -> Ws {
    let (stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("timed out waiting for link to connect")
        .expect("accept");
    tokio_tungstenite::accept_async(stream)
        .await
        .expect("ws handshake")
}

/// Read the next structured envelope, skipping binary/ping/pong frames.
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
        seq: 0, // inbound seq is irrelevant except for Ack, which carries ack_seq
        body,
    };
    ws.send(Message::Text(serde_json::to_string(&env).unwrap()))
        .await
        .expect("send to link");
}

fn state(s: &str) -> EnvelopeBody {
    EnvelopeBody::State(StateFrame {
        state: s.into(),
        session_id: None,
        reason: None,
    })
}

fn usage() -> EnvelopeBody {
    EnvelopeBody::Usage(UsageSnapshot {
        source: "transcript".into(),
        ..Default::default()
    })
}

fn log_line() -> EnvelopeBody {
    EnvelopeBody::Log(LogLine {
        level: "warn".into(),
        message: "buffered".into(),
    })
}

/// Build a `Link` wired to `127.0.0.1:{port}` and spawn `run()`.
/// Returns the cmd sender, event receiver, and shared meta ring.
fn spawn_link(port: u16) -> (mpsc::Sender<LinkCmd>, mpsc::Receiver<LinkEvent>, Arc<MetaRing>) {
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
    (cmd_tx, event_rx, ring)
}

#[tokio::test]
async fn replays_unacked_meta_with_original_seq_after_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (cmd_tx, _event_rx, _ring) = spawn_link(port);

    // --- first connection: send three meta events, record their seqs ---
    let mut c1 = accept(&listener).await;
    let hello = next_env(&mut c1).await;
    assert!(matches!(hello.body, EnvelopeBody::Hello(_)), "first frame is Hello");

    cmd_tx.send(LinkCmd::SendMeta(state("idle"))).await.unwrap();
    cmd_tx.send(LinkCmd::SendMeta(usage())).await.unwrap();
    cmd_tx.send(LinkCmd::SendMeta(log_line())).await.unwrap();

    let e1 = next_env(&mut c1).await;
    let e2 = next_env(&mut c1).await;
    let e3 = next_env(&mut c1).await;

    // seqs are monotonic and follow the Hello's seq.
    assert_eq!(e1.seq, hello.seq + 1);
    assert_eq!(e2.seq, hello.seq + 2);
    assert_eq!(e3.seq, hello.seq + 3);
    assert!(matches!(e1.body, EnvelopeBody::State(_)));
    assert!(matches!(e2.body, EnvelopeBody::Usage(_)));
    assert!(matches!(e3.body, EnvelopeBody::Log(_)));

    // --- drop the link mid-stream: nothing was acked ---
    drop(c1);

    // --- reconnect: Hello, then the three buffered metas replay verbatim ---
    let mut c2 = accept(&listener).await;
    let hello2 = next_env(&mut c2).await;
    assert!(matches!(hello2.body, EnvelopeBody::Hello(_)));
    assert!(hello2.seq > e3.seq, "reconnect Hello uses a fresh seq");

    let r1 = next_env(&mut c2).await;
    let r2 = next_env(&mut c2).await;
    let r3 = next_env(&mut c2).await;

    assert_eq!(
        (r1.seq, r2.seq, r3.seq),
        (e1.seq, e2.seq, e3.seq),
        "replayed frames carry their original seq"
    );
    assert!(matches!(r1.body, EnvelopeBody::State(_)));
    assert!(matches!(r2.body, EnvelopeBody::Usage(_)));
    assert!(matches!(r3.body, EnvelopeBody::Log(_)));
}

#[tokio::test]
async fn ack_trims_ring_so_acked_events_are_not_replayed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (cmd_tx, mut event_rx, _ring) = spawn_link(port);

    // --- first connection: two meta events ---
    let mut c1 = accept(&listener).await;
    let hello = next_env(&mut c1).await;

    cmd_tx.send(LinkCmd::SendMeta(state("idle"))).await.unwrap();
    cmd_tx.send(LinkCmd::SendMeta(usage())).await.unwrap();
    let a = next_env(&mut c1).await;
    let b = next_env(&mut c1).await;
    assert_eq!(a.seq, hello.seq + 1);
    assert_eq!(b.seq, hello.seq + 2);

    // Ack through the second event, then send a non-Ack marker. TCP ordering
    // guarantees the link reads the Ack before the marker, so once the marker
    // surfaces on event_rx we know the Ack has been processed (ring trimmed).
    send_env(&mut c1, EnvelopeBody::Ack { ack_seq: b.seq }).await;
    send_env(&mut c1, EnvelopeBody::Interrupt).await;
    match tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
        .await
        .expect("timed out waiting for marker")
        .expect("event channel closed")
    {
        LinkEvent::Text(env) => assert!(matches!(env.body, EnvelopeBody::Interrupt)),
        LinkEvent::Binary { .. } => panic!("expected the text marker, got binary"),
    }

    // A third meta event, sent after the Ack was processed, stays un-acked.
    cmd_tx.send(LinkCmd::SendMeta(log_line())).await.unwrap();
    let c = next_env(&mut c1).await;
    assert_eq!(c.seq, b.seq + 1);

    // --- reconnect: only the un-acked third event replays ---
    drop(c1);
    let mut c2 = accept(&listener).await;
    let hello2 = next_env(&mut c2).await;
    assert!(matches!(hello2.body, EnvelopeBody::Hello(_)));

    let replayed = next_env(&mut c2).await;
    assert_eq!(
        replayed.seq, c.seq,
        "acked events (seq <= {}) are trimmed; only the un-acked event replays",
        b.seq
    );
    assert!(matches!(replayed.body, EnvelopeBody::Log(_)));

    // The very next frame is a fresh live event — proving exactly one frame was
    // replayed (had the acked prefix survived, seq {a,b} would arrive first).
    cmd_tx.send(LinkCmd::SendMeta(state("running"))).await.unwrap();
    let live = next_env(&mut c2).await;
    assert!(live.seq > replayed.seq, "live event follows the single replay");
    assert!(matches!(live.body, EnvelopeBody::State(_)));
}
