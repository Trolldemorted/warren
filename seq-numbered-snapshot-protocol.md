# Seq-Numbered Snapshot Protocol (residual bug + future hardening)

**Status:** design only. Authored 2026-07-06 after the §A.6 mouseTracking fix
left one observable failure mode: empty `screen_snapshot` envelopes can still
wipe the term-ring replay on page-load. This is the §A.7+
replacement for the current "best-effort" snapshot apply, plus the contract
that makes future incremental work (dirty-line snapshots, reconnect catch-up,
frame-loss detection) possible.

---

## 0. Problem the protocol solves

Today's late-join flow has three sources of truth, joined at the browser
without explicit ordering:

1. **term_ring replay buffer** — bounded ring of recent raw PTY bytes, replayed
   verbatim on WS open.
2. **`screen_snapshot` envelope** — `Vec<String>` rows + cursor position,
   computed at a single moment in the rabbit PTY blocking thread.
3. **Live binary frames** — every chunk the blocking thread reads after the
   snapshot point.

The wire preserves ordering (single-blocking-thread producer, single TCP
stream, single tokio broadcast consumer per browser), so the browser sees
`replay → snapshot → live[0] → live[1] → …` in that order. **What it
*cannot* tell** is which `live[k]` frames the snapshot already accounts for.
For mostly-empty snapshots the only safe behavior is to skip the apply; we
currently do that via a heuristic (`env.text.some(row => row.trim().length > 0)`)
patched into `applyMeta` at `warren/templates/agent_claude.html:339`.

The seq-numbered protocol replaces that heuristic with an explicit
high-water mark on the snapshot and a per-frame seq on every binary chunk.
From there, three downstream properties fall out:

1. **Deterministic snapshot coverage.** The browser can ask "do I have
   every frame the snapshot already paints?" — yes, because every binary
   frame in the pipe carries its own seq ≤ `snapshot.after_seq`.
2. **Wire-driven, not heuristic-driven, "skip empty" decision.** A snapshot
   with `text == []` *and* `after_seq == 0` is a deliberate "we have no data"
   (skip); a snapshot with `text == []` *and* `after_seq > 0` is "claude's
   view at HWM happens to be empty" (apply). No more `row.trim()` guessing.
3. **Detectable frame loss.** If a `SeqGap` appears (broadcast drop, or
   future reconnect catch-up miss), the browser can fall back to requesting
   a fresh snapshot instead of guessing whether the visible state is right.

---

## 1. Wire format — minimal change

**Binary frames (rabbit → warren → browser) for terminal bytes.**

Today: `<chan:1> <bytes…>`
New: `<chan:1> <seq:8 BE u64> <bytes…>`

8 bytes of overhead per frame. `chan:0x01` and `chan:0x02` are the only
defined channels today; both carry their own per-channel seq counter. The
`0x03+` channels (none yet) would each get their own counter on demand.

Why BE u64: 8-byte aligned writes at the rabbit driver are already natural
(we already emit `PtyEvt::Read(Vec<u8>)` and split into frames; prepending a
hand-built 8-byte header is two `put_u64` calls). u64 wraps cleanly; the
arithmetic we do is "`<=`" comparisons on seq values, so wrap is fine as
long as we never compare across >2^63 of separation in a single browser
session — not realistic for a single claude PTY.

**Meta envelopes stay text.** No change to `Envelope { v, seq, body }` —
the envelope-level `seq` is unrelated to the per-frame counter (it remains
the meta-replay `LinkCmd::SendMeta` ack-stream seq, governed by §D
Milestone 5's `MetaRing`).

**`ScreenSnapshotBody` gains one field.**

```rust
// rabbit/src/wire.rs and warren/src/agents_live/wire.rs (both must mirror):
pub struct ScreenSnapshotBody {
    pub chan: u8,
    pub cols: u16,
    pub rows: u16,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub cursor_visible: bool,
    pub text: Vec<String>,
    /// NEW. Per-`chan` counter of the last byte whose cells are *fully
    /// represented* in `text`. The browser drops any buffered frames
    /// with `seq <= after_seq` before applying the snapshot. Special
    /// value `0` means "no bytes fed yet on this channel — do not
    /// discard anything; treat as a fresh connect."
    pub after_seq: u64,
}
```

That's it for the wire surface: one widened binary frame, one added u64 on
the snapshot body. No new envelope variants, no breaking renames (existing
JS keys remain valid, new key has `#[serde(default)]` for forward compat).

---

## 2. Rabbit side — per-channel counter + emit on read

**2.1 Where the counter lives.** Inside the blocking PTY thread closure in
`rabbit/src/supervisor.rs::spawn_run_one`. The thread is the single owner of
the VT and the single producer of `PtyEvt::Read`, so it's the natural
home. Each PTY (claude + shell) has its own; the channel byte selects which
counter increments.

**2.2 Counter shape.** A local `u64` initialized to `1` (so `seq=0` is
reserved for "no bytes yet"). Incremented *before* assigning — the first
byte read off the wire gets seq=1. `Ordering::Relaxed` is fine (single
producer on this thread, no observers on other threads).

**2.3 Read arm.** Replace the current

```rust
pty_evt_tx.blocking_send(PtyEvt::Read(io_buf[..n].to_vec()))
```

with

```rust
let seq = next_seq;
next_seq = next_seq.wrapping_add(1);
pty_evt_tx.blocking_send(PtyEvt::Read {
    chan: TERM_CHAN_CLAUDE,
    seq,
    data: io_buf[..n].to_vec(),
});
```

(The shell PTY in `rabbit/src/shell.rs` does the same with `TERM_CHAN_SHELL`
and its own counter; the two counters are independent.)

**2.4 Snapshot arm.** The existing `PtyCmd::Snapshot { chan }` handler in
`spawn_run_one`'s `match cmd`:

```rust
PtyCmd::Snapshot { chan } => {
    let snap = vt.snapshot();
    let body = ScreenSnapshotBody {
        chan,
        cols: snap.cols,
        rows: snap.rows,
        cursor_col: snap.cursor_col,
        cursor_row: snap.cursor_row,
        cursor_visible: snap.cursor_visible,
        text: snap.text,
        // `after_seq` semantics: if any bytes have been read since last
        // snapshot, this is `next_seq - 1` (most recent seq assigned).
        // If no bytes have ever been read, this is `0` ("we have no
        // data, don't discard anything").
        after_seq: if bytes_read_since_spawn > 0 { next_seq.wrapping_sub(1) } else { 0 },
    };
    pty_evt_tx.blocking_send(PtyEvt::Meta(EnvelopeBody::ScreenSnapshot(body)))
        .ok();
}
```

The `bytes_read_since_spawn` boolean flips to `true` the first time
`reader.read()` returns `Ok(n)` with `n > 0` — it's distinct from the
counter (counter starts at 1 unconditionally, the boolean tracks "did we
ever feed at least one byte"). Cheaper than threading two counters.

**2.5 Driver task — unchanged outer shape, wider frame.**

The driver in `supervisor.rs` currently routes `PtyEvt::Read → LinkCmd::SendBinary`.
With `PtyEvt::Read` now a struct (`{ chan, seq, data }` — replace the tuple
so `chan` and `seq` are explicit fields), the driver emits:

```rust
LinkCmd::SendBinary {
    chan: ev.chan,
    seq: ev.seq,
    data: ev.data.into(),
}
```

`LinkCmd::SendBinary` gains a `seq: u64` field. The `Link` layer
(`rabbit/src/link.rs`) prepends `<chan:1> <seq:8 BE>` when serializing the
outbound binary frame; existing `Link::write_binary` allocates a new
`BytesMut` and writes the channel byte — we extend the prelude.

**2.6 `PtyEvt::Read` is no longer a tuple.** Currently
`PtyEvt::Read(Vec<u8>)`. Becomes `PtyEvt::Read { chan: u8, seq: u64, data: Vec<u8> }`.
Update the producer (blocking thread) and consumer (driver task) — one
struct field at each site. Wire-it-through locations enumerated in §7.

**2.7 `supervisor.rs::dispatch_to_pty`** stays as-is (it deals in frames
going the other direction — warren → rabbit — and doesn't need the seq).

---

## 3. Warren side — pass seq through, store it, never invent one

**3.1 Link ingress.** `warren/src/agents_live/link.rs::on_binary_frame`
(whatever the entry point is named in the parallel-warren mirror of
`rabbit/src/link.rs`) splits the incoming frame into `(chan, seq, data)`,
not just `(chan, data)` as today. `seq` rides through verbatim — warren is
a dumb pipe here and never invents or rewrites a seq (the invariant is
"warren's outgoing seq on chan X equals rabbit's emitted seq on chan X").

**3.2 `handle.replay_term()` today** returns `VecDeque<Bytes>` with
`<chan:1> <bytes>` payloads. It becomes `VecDeque<TermFrame { chan, seq,
data }>` (or equivalent). The browser-side caller in `ws_browser.rs`
writes the bytes back to the WS with the same `<chan:1> <seq:8> <bytes>`
prelude — *the seq is preserved through warren unchanged*.

**3.3 Live binary arm in `ws_browser.rs`** (currently
`bytes = match chunk { Ok(b) => b, ... }`) handles `TermFrame` and writes
`<chan:1> <seq:8> <data>` to the sink. Same code path as replay so far as
the browser is concerned.

**3.4 `ScreenSnapshot` envelope** gains `after_seq: u64` (mirrored field,
since `warren/src/agents_live/wire.rs` and `rabbit/src/wire.rs` are the
two parallel enums and stay in lockstep). It's just a number the browser
will read off `env.after_seq`.

**3.5 No changes to:** term-ring handling logic, meta broadcast, replay
ordering, snapshot request lifecycle, leader/claim wire, prompt policy.
The change is purely "carry an additional u64 alongside the bytes."

---

## 4. Browser policy — the two-step apply

The browser (`warren/templates/agent_claude.html`) gains a small,
windowed ring of pending frames and a new policy on snapshot arrival.

**4.1 Per-channel seq state.** Module-scope:

```js
const lastSeenSeq = new Map();   // chan → highest seq observed
function noteSeq(chan, seq) {
  const cur = lastSeenSeq.get(chan);
  if (cur === undefined || seqAfter(cur, seq)) lastSeenSeq.set(chan, seq);
}
function seqAfter(a, b) {
  // u64 with wrap; treat separations > 2^63 as "before" rather than after,
  // i.e. (b - a) mod 2^64 is in [1, 2^63).
  return (b - a + 2n ** 64n) % (2n ** 64n) < 2n ** 63n;
}
```

Bigint is fine — the cost is negligible at human typing rates. (If
perf-sensitive: `DataView.getBigUint64` plus a `seqAfter` helper operating
on `bigint`.)

**4.2 Wire-side parsing.** In `ws.onmessage`:

```js
if (ev.data instanceof ArrayBuffer) {
  const view = new Uint8Array(ev.data);
  if (view.length < 9) return;        // need chan + 8 seq + at least 0 bytes
  if (view[0] !== 0x01) return;       // claude channel only
  const seq = new DataView(view.buffer, view.byteOffset + 1, 8)
                 .getBigUint64(0, /*BE=*/false);  // not BE!
  noteSeq(0x01, seq);
  pendingFrames.push({
    chan: view[0],
    seq,
    data: view.subarray(9),
  });
  // Optional: cap pendingFrames to e.g. 256 entries to bound memory.
  return;
}
```

(The "BE=false" looks wrong but is right — `DataView.getBigUint64`'s
`littleEndian` flag. We ARE writing BE on the server; flag is
`littleEndian=false` which is what passes BE through.)

**4.3 Snapshot policy.** Replaces the current `hasContent` heuristic:

```js
case 'screen_snapshot':
  if (!Array.isArray(env.text)) break;

  const hwm = env.after_seq | 0;   // 0 means "no bytes fed yet"
  if (hwm > 0) {
    // Drop every pending frame whose seq is at or below the HWM. Frames
    // with seq > hwm remain (they were in flight after the snapshot
    // computation point and aren't reflected in `text`).
    let i = 0;
    while (i < pendingFrames.length && pendingFrames[i].seq <= hwm) i++;
    if (i > 0) pendingFrames.splice(0, i);
  }

  // Empty snapshot at first connect (hwm === 0): we have nothing to
  // destroy and the term-ring replay already painted; skip the apply.
  // Empty snapshot later (hwm > 0): claude genuinely *is* showing blank
  // text right now — apply it; the delta frames in the pending ring will
  // arrive next (and would-be already-arrived delta is now in the ring,
  // replayed after the apply so visible state is consistent).
  if (pendingFrames.length === 0 || hasContent) {
    if (!hasContent && pendingFrames.length > 0) {
      // Genuine-empty-but-live-bytes-pending: reset to clear whatever
      // stale state the term-ring painted, then replay pending frames so
      // the visible state is the actual post-HWM state. (Otherwise the
      // user would see term-ring-painted state → blank → live bytes,
      // an obvious flicker.)
      term.reset();
      for (const f of pendingFrames) term.write(bytesToUtf8(f.data));
    } else {
      term.reset();
      term.write(env.text.join('\r\n') + '\r\n');
    }
    const r = (env.cursor_row || 0) + 1;
    const c = (env.cursor_col || 0) + 1;
    term.write(`\x1b[${r};${c}H`);
  } else {
    // empty snapshot + hwm == 0: nothing to apply, leave term-ring-painted
    // state alone. The frames that arrived before the snapshot are
    // already painted; pending ring is empty (hwm == 0).
  }
  pendingFrames.length = 0;   // we've either applied or accepted them
  break;
```

(Note: the pseudocode above still includes a `hasContent` check; the
distinction is now driven by `hwm`, not by row content alone. `hasContent`
stays as an "anything to write" probe for the synthetic delta-replay branch
above — but the *primary* decision is "is this snapshot authoritative?")

**4.4 The two-step apply**, named for this protocol:

> **Step 1**: Trim pending frames to those strictly after the snapshot's
> HWM. They're the "delta" the snapshot doesn't cover.
>
> **Step 2**: Apply the snapshot *as if* the delta weren't there. The
> delta is then either already painted (frames that arrived before the
> snapshot — they were buffered and drop on the floor when seq ≤ hwm) or
> pending (frames > hwm — replayed after the apply). Either way, the
> visible state lands correct.

This is the "two steps": trim, then apply. Adjacent frames with
seq > hwm are guaranteed to come after the snapshot on the wire (per §3.1,
warren passes through with no reordering) — they're just delayed by the
JS event-loop microtask queue, never "before" the snapshot.

---

## 5. Backward compatibility

**5.1 Wire version bump.** Bump `PROTOCOL_VERSION` from 1 to 2 (constant
in `warren/src/agents_live/wire.rs::PROTOCOL_VERSION` and the matching
`rabbit/src/wire.rs`). Old warren rejects unknown `v`; new warren accepts
both with a `#[serde(default)] on after_seq` shim and a frame-length
tolerance:

- New browser → old warren: old warren strips the channel byte and tries
  to parse 8 bytes of `seq` as terminal text → garbled display but no
  crash; bumps PROTOCOL_VERSION gate makes the browser refuse to connect
  early. **Mitigation**: the browser reads the page's rendered
  `PROTOCOL_VERSION` constant (templated by warren) and refuses to
  handshake if it's < 2.
- Old browser → new warren: new warren's binary frame has 9 bytes of
  header but old browser strips only 1; the seq lands in xterm.js
  verbatim (8 raw bytes). Visible garbage in the pane; no crash.
  **Mitigation**: new warren detects old-browser via the missing
  seq-aware behavior in a probe envelope OR simply refuses new-version
  binary frames to old browsers via the same constant.

**5.2 No operator switches.** The flag is the version. Rabbit doesn't get
a config knob for "send v1 vs v2"; rabbit *is* v2 once it ships. Old
warren binaries are replaced in lockstep (same image, same rollout).

**5.3 Schema.** No migrations, no DB shape change. Memory rule
"never edit existing migrations" preserved trivially; the rule
"never bypass atlas" is N/A (no SQL touched).

---

## 6. Test strategy

**6.1 rabbit lib tests** (`rabbit/src/supervisor.rs::tests` + `rabbit/src/wire.rs::tests`):

- `read_arm_assigns_monotonic_seqs_per_channel`: drive a `/bin/cat` PTY
  for 5 chunks, assert each `PtyEvt::Read` carries `seq ∈ [1..6]` in
  order. Two channels in parallel: claude seqs and shell seqs are
  independent counters.
- `snapshot_after_seq_reflects_last_fed`: feed 3 chunks (seqs 1, 2, 3),
  request snapshot, assert `after_seq == 3`.
- `snapshot_before_any_read_carries_after_seq_zero`: spawn, ask snapshot
  immediately without feeding anything (via `PtyCmd::Snapshot` queued
  before the first `try_recv`/`reader.read`), assert `after_seq == 0`.
- `driver_sends_linkcmd_with_chan_seq_data_tuple`: a `PtyEvt::Read
  { chan, seq, data }` arrives at the driver, a `LinkCmd::SendBinary
  { chan, seq, data }` is emitted with the same `seq` byte-for-byte.
- `wire_emit_prepends_chan_then_seq_then_data`: drain the binary output
  of a small driver run, assert the wire bytes are
  `<chan><8 BE seq><data>` exactly, three times in a row.

**6.2 rabbit integration tests** (extend `rabbit/tests/snapshot_roundtrip.rs`):

- `snapshot_roundtrip_with_after_seq`: drive a real `/bin/echo` PTY for
  ~30 bytes, request `Snapshot { chan: 0x01 }`, parse the resulting
  `EnvelopeBody::ScreenSnapshot` JSON, assert `text` matches expected
  rows *and* `after_seq > 0`. Add a second `Snapshot` after more bytes
  and assert `after_seq` strictly increases.
- `empty_vt_snapshot_carries_after_seq_zero`: spawn a process whose first
  byte doesn't arrive for 1 second (`sleep 1; echo hi`), request
  snapshot at t=0, assert `after_seq == 0`. After 1s+10ms, request
  again, assert `after_seq > 0`.
- `seq_holds_through_replay_window`: send 1 MiB through the PTY so the
  term_ring overflows its 512 KiB cap; reconnect a fake WS; the
  replayed frames carry seq values *contiguous from the cap point on*,
  no gaps in the wire-visible window.

**6.3 warren bin tests** (`warren/src/agents_live/ws_browser.rs::tests`,
mirror the §A.6 routing tests):

- `binary_frame_passes_seq_through_unchanged`: a fake `WebSocketStream`
  sends a v2 binary `<chan><seq:8><data>`; assert the bytes written
  back into the sink (when re-broadcasting to other browsers) are
  byte-equal. Catches a refactor that "helpfully" rewrites seq.
- `screen_snapshot_envelope_carries_after_seq_to_browser`: synthesize
  an inbound `Meta(ScreenSnapshot { after_seq: 42, ... })`, drive the
  meta path, assert the JSON the browser saw has
  `"after_seq": 42` literally. Wire-tag shape lock.

**6.4 Browser tests** (deferred to manual smoke today; the existing
project has no headless JS harness):

Smoke checklist (committed message form):

1. Open `/agent/:id/claude` cold: VT is empty ⇒ snapshot
   `text: ["" × 40], after_seq: 0` ⇒ skip apply, term-ring content
   painted, no black screen.
2. Open with claude running and the TUI settled: VT has content ⇒
   snapshot `text: [...], after_seq: 1024` ⇒ apply as today; cursor lands
   at the snapshot's reported position; subsequent live bytes paint
   correctly.
3. Open with claude in mid-redraw: snapshot occasionally returns
   empty *with* `after_seq > 0` ⇒ synthetic delta-replay branch fires
   (`term.reset() + replay pendingFrames`) — visually identical to (2)
   within one frame.
4. Network blip while claude is running: WS reconnects, new
   snapshot arrives, fired-bytes with seq > hwm may already be in
   pendingFrames ⇒ apply the snapshot, replay the delta — no
   visible flicker.
5. Stale-leader scenario: the §A.6 leader path is unaffected. Spot-check
   that leader resize → rabbit `PtyCmd::Resize` → resync ⇒ no seq drift
   (resize doesn't assign a seq; the seq counter is bytes-fed-only).

**6.5 Property test** (optional, future): generate random `(chan,
seq, data)` triples from a grammar that respects monotonicity, run them
through the browser's pending-ring code in a jsdom-style harness, assert
invariant `ring.filter(f => f.seq > hwm)` never empties a frame whose seq
materially affects the visible state. This is the test that catches
"off-by-one wrap" bugs in the seq comparator.

---

## 7. Files touched (full enumeration)

**7.1 Rabbit crate** (`/workdir/rabbit/`):

- `src/wire.rs` — `ScreenSnapshotBody` gains `after_seq: u64` field with
  `#[serde(default)]`; new wire enum tag stays `"screen_snapshot"`.
- `src/supervisor.rs` — `PtyEvt::Read` becomes a struct; `spawn_run_one`
  blocking thread holds a per-channel `next_seq` counter (local mutable
  in the `tokio::task::spawn_blocking` closure); the `reader.read()`
  arm assigns `seq` and emits a struct; the `PtyCmd::Snapshot { chan }`
  arm populates `after_seq`; the driver task's `PtyEvt::Read → LinkCmd`
  mapping carries `seq` through.
- `src/shell.rs` — same struct change for `PtyEvt::Read` on the shell
  channel; counter starts at 1, increments per read.
- `src/link.rs` — `LinkCmd::SendBinary { chan: u8, seq: u64, data: Bytes }`
  (struct with three fields, replacing the two-field one); the writer's
  binary path writes `<chan:1> <seq:8 BE> <data>` instead of
  `<chan:1> <data>`.
- `src/vt.rs` — no functional change (`TermTracker::snapshot` stays
  author-of-VT-text; the supervisor's `PtyCmd::Snapshot` arm just
  attaches `after_seq` from the counter). One new optional helper:
  `TermTracker::empty() -> bool` for test ergonomics (lets tests assert
  "VT genuinely produced empty rows" without leaking avt internals).

**7.2 Warren crate** (`/workdir/warren/`):

- `src/agents_live/wire.rs` — `ScreenSnapshotBody` mirrors
  `rabbit/src/wire.rs::ScreenSnapshotBody` (after_seq + serde default).
- `src/agents_live/link.rs` (or wherever the binary ingress lives) —
  parses incoming frames as `<chan:1> <seq:8 BE> <data>` and forwards
  both `chan` and `seq` through the live broadcast path.
- `src/agents_live/handle.rs` — `subscribe_term` and the broadcast
  payload become `TermFrame { chan: u8, seq: u64, data: Bytes }`. Test
  fixtures that construct term frames need updating.
- `src/agents_live/ws_browser.rs` — `bytes.first() != Some(&TERM_CHAN_CLAUDE)`
  check stays; the live binary arm now pulls 8 bytes of seq after the
  channel byte and writes them through verbatim; `subscribe_term()`
  caller is the `chunk = term_rx.recv()` arm. The meta-arm JSON-encodes
  `ScreenSnapshotBody` (which already includes `after_seq`); no
  per-arm change.
- `src/agents_live/ws_shell.rs` — mirror ws_browser.rs for the shell
  channel's own seq counter; the chan byte is `0x02` and the seq
  counter is shell-local.
- `Cargo.toml` — no new deps; `bytes::Bytes` was already in use.

**7.3 Template** (`/workdir/warren/templates/`):

- `agent_claude.html` — `applyMeta`'s `screen_snapshot` case rewritten
  per §4.3; new module-scope `pendingFrames` array + `lastSeenSeq` map
  + `noteSeq`/`seqAfter` helpers; `ws.onmessage`'s binary arm pulls the
  seq from the wire and either pushes onto `pendingFrames` (if it can
  bind to a not-yet-arrived snapshot) or applies directly (if no
  snapshot is outstanding).
- `agent_shell.html` — same template changes, but `chan:0x02` and a
  separate `pendingFrames` queue (per-channel isolation — the two
  counters don't interfere).

**7.4 Docs/tests:**

- `rabbit/tests/snapshot_roundtrip.rs` — extend per §6.2.
- `warren/src/agents_live/ws_browser.rs::tests` — add 2 tests per §6.3.
- `rabbit/src/supervisor.rs::tests` — add 4 tests per §6.1.
- `rabbit/src/wire.rs::tests` — serde roundtrip for the new
  `after_seq` field shape; assert `#[serde(default)]` makes v1 JSON
  deserialize cleanly into a v2 struct.
- `rabbit/src/shell.rs::tests` — extend the existing round-trip test to
  assert `seq` is 1 on the first chunk and increments monotonically.
- `TODOs.md` — add an entry under "Done in a prior session" once
  shipped, cross-linking this design doc.

---

## 8. Risks & open questions

**8.1 The seq preamble is overhead.** 8 bytes per binary frame is
negligible at human typing rates (~tens of frames per second worst case)
but the asciicast recorder writes similar rates. Cost is ~10% larger
frames; not a wire-shape concern, just bandwidth.

**8.2 Wraparound handling in JS.** The `seqAfter(a, b)` bigint math
above works under monotonic assumptions + small windows. A pathological
scenario (browser tab frozen for 2^63 increments) is not realistic.
Tests should pin only the "single session, sub-2^60 increments" property.

**8.3 Browser `pendingFrames` memory bound.** If many live frames
arrive during a slow JS turn (or while a snapshot computation is
in-flight in the browser), `pendingFrames` grows. Cap at ~256 frames ×
~4 KiB = 1 MiB. If exceeded, log a `console.warn` and force-apply
whatever's pending (degrades to today's behavior in the worst case).

**8.4 Resize-driven redraws.** The §A.6 leader-driven Resize path
already passes through unchanged. Adds `PtyCmd::Resize` doesn't bump
the seq counter (resize is meta, not bytes), so the snapshot's
`after_seq` doesn't change on resize. That matches intuition.

**8.5 Recorder side.** The asciicast recorder in `rabbit/src/recorder.rs`
already takes `PtyEvt::Read` data; once `PtyEvt::Read` becomes a struct,
the recorder's `feed()` call pulls `data` and ignores `seq` (or
optionally embeds seq in the asciicast output for replay tools — a
follow-up, not v1).

**8.6 Why this is §A.7 and not §A.6.** The mouse-tracking fix was
specific to the user's reported symptom (Firefox desktop, easy repro,
small fix). This protocol is the underlying architecture that makes
*future* browsers, reconnect paths, and replay tools correct by
construction. It's not blocking; the §A.6 + §D Milestone 5 patches
keep the current code correct in the common case. Land when the
incremental snapshots / dirty-lines work queues up next.

**8.7 Manual-claim leader compat.** The §A.6 path computes term_size
on Hello and stores it on `AgentHandle`; the snapshot path adds
`after_seq` and two counters — these don't overlap. No collision.

---

## 9. Backout plan

If the v2 wire breaks something unexpected: drop the
`PROTOCOL_VERSION` bump, revert the `<chan:1> <seq:8 BE>` preludes
to `<chan:1>`, drop `after_seq` from `ScreenSnapshotBody` (and the
`serde(default)`), restore `hasContent` as the sole browser-side
guard. Pure mechanical; the rest of the code paths (live binary
forwarding, snapshot request/computation) didn't gain any new
contracts that would need unwinding.

---

## 10. Implementation order

1. **Add `after_seq` field, no behavior change.** Mirror in
   `rabbit/src/wire.rs` and `warren/src/agents_live/wire.rs`; add
   `#[serde(default)]`; `cargo test --workspace` clean. (No protocol
   bump yet.)
2. **Thread `seq` through `PtyEvt::Read` (struct shape) and
   `LinkCmd::SendBinary`.** Same prelude shape as today; counter
   increments but the reader doesn't put bytes on the wire differently
   yet. `cargo build` clean.
3. **Emit `<chan:1> <seq:8 BE>` on the wire (server → browser).**
   Browser parses and stores but doesn't act on `seq` yet. v1
   browser's pane shows 8 garbled bytes per frame but otherwise
   continues; acceptable because PROTOCOL_VERSION bump gates old
   browsers from connecting to new warren in the same release.
4. **Bump `PROTOCOL_VERSION` to 2.** Old `v=1` clients hand-shake-refused.
5. **Browser policy: rewrite `applyMeta::screen_snapshot` per §4.3.**
   `pendingFrames` ring + `noteSeq` helper + two-step apply.
6. **Tests per §6.** New rabbit lib + integration; new warren bin
   tests; manual smoke checklist. `cargo test --workspace` clean.
7. **TODOs.md entry** + cross-link.

Each step compiles + tests green in isolation; the full pipeline is
end-to-end only at step (5) — meaning step (3)'s "8 garbled bytes" is
the worst-case intermediate state, gone by step (5).
