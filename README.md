# shred-decoder

High-level UDP shred decoder for Solana with parallel Reed-Solomon FEC recovery
and an event-driven ingest loop. Given a bind address and a handler, emits
every `VersionedTransaction` recovered from the shred stream.

Target Solana: `v2.2.1` runtime + `jito-solana` ledger fork (branch
`eric/v2.2-merkle-recovery`).

## Why

The straightforward approach — decode shreds, recover FEC sets, deshred, and
emit transactions — all on one thread — becomes the bottleneck under real
validator load because Reed-Solomon decode is CPU-heavy. This crate splits
the pipeline so that:

- UDP recv runs on N kernel-assisted SO_REUSEPORT sockets
- a single ingest thread owns the state (lock-free)
- RS recovery runs on a dedicated worker pool
- the ingest thread reacts to packet arrivals *and* recovery completions via
  `crossbeam_channel::select!` — no polling, no fixed sleep windows

## Quick start

```toml
[dependencies]
shred-decoder = { git = "https://github.com/Minival1/shred-decoder.git" }
```

```rust
use std::sync::Arc;
use shred_decoder::{ShredDecoder, ShredHandler, TransactionEvent};

struct Printer;

impl ShredHandler for Printer {
    fn handle_transaction(&self, event: &TransactionEvent<'_>) {
        println!(
            "slot {} sig {} tx_at {}us",
            event.slot, event.signature, event.processed_at_micros,
        );
    }
}

fn main() -> anyhow::Result<()> {
    let decoder = ShredDecoder::builder()
        .bind("0.0.0.0:8001".parse()?)
        .receive_threads(4)
        .recovery_workers(8)
        .handler(Printer)
        .build()?;

    // Decoder runs in background threads. Block until you're done.
    std::thread::park();

    decoder.shutdown();
    Ok(())
}
```

`handler` is the only required configuration besides `bind`. Everything else
has defaults tuned for a single validator-grade node.

## Builder options

| Method | Default | Notes |
|---|---|---|
| `.bind(SocketAddr)` | — | **Required.** Local UDP bind. A range of `receive_threads` ports starting here is reserved. |
| `.receive_threads(n)` | 4 | Number of SO_REUSEPORT recv threads. Each kernel-selects its own packets. |
| `.recovery_workers(n)` | 8 | Worker pool size for parallel FEC recovery. |
| `.recv_buffer_bytes(b)` | 256 KiB | `SO_RCVBUF` per socket. Raise this under heavy loss. |
| `.handler(H)` | — | **Required.** `impl ShredHandler`. Called once per recovered transaction. |
| `.handler_arc(Arc<dyn ShredHandler>)` | — | Same as `.handler` when you already have an `Arc`. |

## Handler trait

```rust
pub trait ShredHandler: Send + Sync {
    fn handle_transaction(&self, event: &TransactionEvent<'_>);
}

pub struct TransactionEvent<'a> {
    pub slot: Slot,
    pub signature: Signature,
    pub transaction: &'a VersionedTransaction,
    pub processed_at_micros: u64,
}
```

The handler is called from the ingest thread. **Do not block** — push to a
channel / Tokio mpsc / lock-free queue and process downstream. Blocking inside
`handle_transaction` will stall UDP recv.

`processed_at_micros` is wall-clock time (UNIX micros) captured at emit; use
it for latency measurements against your ground truth.

## Stats

```rust
let stats = decoder.stats();
stats.recovered_count.load(Ordering::Relaxed);          // individual shreds recovered by FEC
stats.entry_count.load(Ordering::Relaxed);              // entries deserialized
stats.txn_count.load(Ordering::Relaxed);                // transactions emitted
stats.fec_recovery_error_count.load(Ordering::Relaxed); // FEC sets that failed to recover
stats.bincode_deserialize_error_count.load(Ordering::Relaxed);
```

`DecoderStats` is `Arc<AtomicU64>` fields — safe to snapshot from any thread.

## Shutdown

```rust
decoder.shutdown();  // blocks until recv, ingest, and recovery threads exit
```

Owned — consumes `self`. Typically wrap in `Arc` if you need multiple callers.

## Architecture

```
   UDP socket         UDP socket            UDP socket
       │                  │                     │
  [recv thread 0] … [recv thread N-1]    (SO_REUSEPORT, kernel-sharded)
       └──────┬───────────┘
              │ crossbeam_channel::bounded(2048)
              ▼
     ┌────────────────────────────────────────────────────┐
     │               ingest thread                        │
     │  select! {                                         │
     │      recv(packet_rx)    → dedup + insert shred     │
     │      recv(result_rx)    → apply RS recovery result │
     │      default(5ms)       → exit check               │
     │  }                                                 │
     │    → dispatch_recoveries → worker pool             │
     │    → drain_recovery_results (again)                │
     │    → emit_events → handler.handle_transaction()    │
     └────────────────────────────────────────────────────┘
              │                          ▲
              │ RecoveryJob              │ RecoveryResult
              ▼                          │
    [recovery worker 0] … [recovery worker M-1]
```

Key properties:

- **State is single-owner.** `all_shreds`, `ShredsStateTracker`, and dedup are
  owned by the ingest thread. No locks on the hot path.
- **Event-driven, not polled.** `select!` blocks the ingest thread until the
  kernel has either a packet *or* a recovery result ready. The 5ms `default`
  branch only runs when both queues are idle, used for exit-flag checks.
- **Recovery is off the hot path.** The ingest thread dispatches a
  `RecoveryJob` via a non-blocking `try_send`; if the queue is full the job
  is retried on the next tick. Results are drained twice per tick so latency
  between "worker finished" and "transaction emitted" is sub-millisecond.

## Tuning

- **Many simultaneous FEC sets needing recovery?** Bump `recovery_workers`.
- **Recovery queue saturates?** Increase `RecoveryPool` queue depth in
  `recovery.rs` (default `workers * 4`).
- **Heavy packet loss?** Raise `recv_buffer_bytes` (kernel also needs
  `net.core.rmem_max` bumped via sysctl).
- **Leader equivocation / unpatched jito-solana?** Log noise from
  `Invalid Merkle root` is already demoted to `debug` level; raise RUST_LOG to
  observe if debugging.

## Logging

Uses `tracing`. Set up any subscriber before calling `.build()`. Levels:

- `info` — bind details, startup
- `warn` — socket config failures, rare recovery edge cases
- `debug` — FEC recovery failures (frequent under leader equivocation),
  per-shred recovery errors, deshred/bincode errors

## License

MIT OR Apache-2.0
