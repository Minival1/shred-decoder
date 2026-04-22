use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use itertools::Itertools;
use solana_ledger::{
    blockstore::MAX_DATA_SHREDS_PER_SLOT,
    shred::{merkle::Shred, Shredder},
};
use solana_perf::{
    deduper::{dedup_packets_and_count_discards, Deduper},
    packet::PacketBatchRecycler,
    recycler::Recycler,
};
use solana_sdk::clock::{Slot, MAX_PROCESSING_AGE};
use solana_streamer::streamer::{self, StreamerReceiveStats};
use tracing::{debug, info, warn};

use crate::{
    recovery::{RecoveryJob, RecoveryPool, RecoveryResult},
    state::{
        get_data_shred_info, get_indexes, update_state_tracker, ComparableShred,
        ShredsStateTracker,
    },
    types::{DecoderError, ShredHandler, TransactionEvent},
};

const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
const DEDUPER_NUM_BITS: u64 = 637_534_199; // 76MB
const DEDUPER_RESET_CYCLE: Duration = Duration::from_secs(5 * 60);
const SLOT_LOOKBACK: Slot = 50;
/// Recover synchronously on the ingest thread when at most this many data
/// shreds are missing. Set to 0 to disable inline recovery entirely — all
/// FEC recoveries go through the worker pool. Inline was tried empirically
/// and regressed tail latency under this workload; keeping the branch in
/// place so it can be re-enabled by raising this threshold.
const INLINE_RECOVERY_MISSING_THRESHOLD: u16 = 0;

pub struct DecoderStats {
    pub recovered_count: AtomicU64,
    pub entry_count: AtomicU64,
    pub txn_count: AtomicU64,
    pub fec_recovery_error_count: AtomicU64,
    pub bincode_deserialize_error_count: AtomicU64,
}

impl Default for DecoderStats {
    fn default() -> Self {
        Self {
            recovered_count: AtomicU64::new(0),
            entry_count: AtomicU64::new(0),
            txn_count: AtomicU64::new(0),
            fec_recovery_error_count: AtomicU64::new(0),
            bincode_deserialize_error_count: AtomicU64::new(0),
        }
    }
}

pub struct ShredDecoderBuilder {
    bind: Option<SocketAddr>,
    receive_threads: usize,
    recovery_workers: usize,
    recv_buffer_bytes: usize,
    handler: Option<Arc<dyn ShredHandler>>,
}

impl ShredDecoderBuilder {
    pub fn new() -> Self {
        Self {
            bind: None,
            receive_threads: 4,
            recovery_workers: 8,
            recv_buffer_bytes: 256 * 1024,
            handler: None,
        }
    }

    pub fn bind(mut self, addr: SocketAddr) -> Self {
        self.bind = Some(addr);
        self
    }

    pub fn receive_threads(mut self, n: usize) -> Self {
        self.receive_threads = n.max(1);
        self
    }

    pub fn recovery_workers(mut self, n: usize) -> Self {
        self.recovery_workers = n.max(1);
        self
    }

    pub fn recv_buffer_bytes(mut self, bytes: usize) -> Self {
        self.recv_buffer_bytes = bytes;
        self
    }

    pub fn handler<H: ShredHandler + 'static>(mut self, handler: H) -> Self {
        self.handler = Some(Arc::new(handler));
        self
    }

    pub fn handler_arc(mut self, handler: Arc<dyn ShredHandler>) -> Self {
        self.handler = Some(handler);
        self
    }

    pub fn build(self) -> Result<ShredDecoder, DecoderError> {
        let bind = self.bind.ok_or(DecoderError::MissingBind)?;
        let handler = self.handler.ok_or(DecoderError::MissingHandler)?;

        let exit = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(DecoderStats::default());

        let (_port, sockets) = solana_net_utils::multi_bind_in_range_with_config(
            bind.ip(),
            (bind.port(), bind.port() + 1),
            solana_net_utils::SocketConfig::default().reuseport(true),
            self.receive_threads,
        )
        .map_err(|e| DecoderError::Bind {
            addr: bind.to_string(),
            source: e,
        })?;

        for socket in &sockets {
            let sock_ref = socket2::SockRef::from(socket);
            if let Err(e) = sock_ref.set_recv_buffer_size(self.recv_buffer_bytes) {
                warn!(error = %e, "shred-decoder: failed to set SO_RCVBUF");
            }
        }

        info!(
            bind = %bind,
            receive_threads = sockets.len(),
            recovery_workers = self.recovery_workers,
            "shred-decoder: listening"
        );

        let recycler: PacketBatchRecycler = Recycler::warmed(100, 1024);
        let forward_stats = Arc::new(StreamerReceiveStats::new("shred_decoder_receiver"));

        let (packet_tx, packet_rx) = crossbeam_channel::bounded(2048);

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        for (i, socket) in sockets.into_iter().enumerate() {
            let recv_thread = streamer::receiver(
                format!("shredDecRecv{i}"),
                Arc::new(socket),
                exit.clone(),
                packet_tx.clone(),
                recycler.clone(),
                forward_stats.clone(),
                None, // coalesce: Option<Duration> since solana-streamer 2.3
                false,
                None,
                false,
            );
            handles.push(recv_thread);
        }
        drop(packet_tx);

        let recovery_pool = RecoveryPool::new(self.recovery_workers);

        let ingest_exit = exit.clone();
        let ingest_stats = stats.clone();
        let ingest_handler = handler.clone();
        let ingester = std::thread::Builder::new()
            .name("shredDecIngest".to_string())
            .spawn(move || {
                run_ingest_loop(
                    packet_rx,
                    recovery_pool,
                    ingest_handler,
                    ingest_stats,
                    ingest_exit,
                );
            })?;
        handles.push(ingester);

        Ok(ShredDecoder {
            exit,
            handles,
            stats,
        })
    }
}

impl Default for ShredDecoderBuilder {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShredDecoder {
    exit: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    stats: Arc<DecoderStats>,
}

impl ShredDecoder {
    pub fn builder() -> ShredDecoderBuilder {
        ShredDecoderBuilder::new()
    }

    pub fn stats(&self) -> &Arc<DecoderStats> {
        &self.stats
    }

    pub fn shutdown(self) {
        self.exit.store(true, Ordering::Release);
        for h in self.handles {
            let _ = h.join();
        }
    }
}

fn run_ingest_loop(
    packet_rx: crossbeam_channel::Receiver<solana_perf::packet::PacketBatch>,
    recovery_pool: RecoveryPool,
    handler: Arc<dyn ShredHandler>,
    stats: Arc<DecoderStats>,
    exit: Arc<AtomicBool>,
) {
    let mut deduper =
        Deduper::<2, [u8]>::new(&mut rand::thread_rng(), DEDUPER_NUM_BITS);
    let mut rng = rand::thread_rng();
    let mut last_dedup_reset = Instant::now();

    let mut all_shreds = ahash::HashMap::<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >::default();
    let mut highest_slot_seen: Slot = 0;
    let mut touched_fec_sets = Vec::<(Slot, u32)>::new();
    let mut pending_shreds_buf = Vec::<Shred>::new();

    while !exit.load(Ordering::Relaxed) {
        // 1. Drain any completed recoveries first so state is fresh before ingest.
        drain_recovery_results(
            &recovery_pool,
            &mut all_shreds,
            &mut touched_fec_sets,
            &stats,
        );

        // 2. Wait for *any* event — new packet batch OR a completed recovery.
        //    Event-driven rather than polled: removes the fixed recv_timeout
        //    window that otherwise caps how fast we can emit recovered txs.
        let mut batch_opt: Option<solana_perf::packet::PacketBatch> = None;
        let mut disconnected = false;
        crossbeam_channel::select! {
            recv(packet_rx) -> msg => match msg {
                Ok(batch) => batch_opt = Some(batch),
                Err(_) => disconnected = true,
            },
            recv(recovery_pool.result_receiver()) -> msg => match msg {
                Ok(result) => apply_recovery_result(
                    result,
                    &mut all_shreds,
                    &mut touched_fec_sets,
                    &stats,
                ),
                Err(_) => {} // pool shutting down; fall through
            },
            default(Duration::from_millis(5)) => {
                // Both queues empty — brief idle tick so exit flag is checked.
            }
        }

        if disconnected {
            break;
        }

        if let Some(mut batch) = batch_opt {
            dedup_packets_and_count_discards(&deduper, std::slice::from_mut(&mut batch));

            if last_dedup_reset.elapsed() >= Duration::from_secs(2) {
                deduper.maybe_reset(
                    &mut rng,
                    DEDUPER_FALSE_POSITIVE_RATE,
                    DEDUPER_RESET_CYCLE,
                );
                last_dedup_reset = Instant::now();
            }

            ingest_batch(
                &batch,
                &mut all_shreds,
                &mut highest_slot_seen,
                &mut touched_fec_sets,
            );
        }

        // 3. Dispatch recoveries for any FEC set with enough shreds.
        dispatch_recoveries(
            &recovery_pool,
            &mut all_shreds,
            &mut touched_fec_sets,
            &mut pending_shreds_buf,
            &stats,
        );

        // 3b. Drain again before emit: workers may have completed a recovery
        //     during steps 2-3, and we don't want to wait a whole tick to
        //     surface those transactions.
        drain_recovery_results(
            &recovery_pool,
            &mut all_shreds,
            &mut touched_fec_sets,
            &stats,
        );

        // 4. Deshred + deserialize + emit events for any completed FEC sets.
        if !touched_fec_sets.is_empty() {
            touched_fec_sets.sort_unstable();
            touched_fec_sets.dedup();
            emit_events(
                &mut all_shreds,
                &touched_fec_sets,
                handler.as_ref(),
                &stats,
            );
            touched_fec_sets.clear();
        }

        // 5. Periodic cleanup of old slots.
        if all_shreds.len() > MAX_PROCESSING_AGE {
            let slot_threshold = highest_slot_seen.saturating_sub(SLOT_LOOKBACK);
            let mut incomplete_count = 0u64;
            all_shreds.retain(|slot, (fec_set_indexes, state_tracker)| {
                if *slot >= slot_threshold {
                    return true;
                }
                for (fec_set_index, _shreds) in fec_set_indexes.iter() {
                    if !state_tracker.already_recovered_fec_sets[*fec_set_index as usize] {
                        incomplete_count += 1;
                    }
                }
                false
            });
            if incomplete_count > 0 {
                debug!(
                    "shred-decoder: cleaned up old slots with {incomplete_count} incomplete FEC sets"
                );
            }
        }
    }

    recovery_pool.shutdown();
}

fn ingest_batch(
    batch: &solana_perf::packet::PacketBatch,
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    highest_slot_seen: &mut Slot,
    touched: &mut Vec<(Slot, u32)>,
) {
    for packet in batch.iter().filter_map(|p| p.data(..)) {
        if packet.len() < 64 {
            continue;
        }
        let Ok(shred) = solana_ledger::shred::Shred::new_from_serialized_shred(packet.to_vec())
            .and_then(Shred::try_from)
        else {
            continue;
        };

        let slot = shred.common_header().slot;
        let index = shred.index() as usize;
        let fec_set_index = shred.fec_set_index();

        if index >= MAX_DATA_SHREDS_PER_SLOT
            || (fec_set_index as usize) >= MAX_DATA_SHREDS_PER_SLOT
        {
            continue;
        }

        let (fec_map, state_tracker) = all_shreds.entry(slot).or_default();

        if highest_slot_seen.saturating_sub(SLOT_LOOKBACK) > slot {
            continue;
        }
        if state_tracker.already_recovered_fec_sets[fec_set_index as usize]
            || state_tracker.already_deshredded[index]
        {
            continue;
        }
        if update_state_tracker(&shred, state_tracker).is_none() {
            continue;
        }

        fec_map
            .entry(fec_set_index)
            .or_default()
            .insert(ComparableShred(shred));
        touched.push((slot, fec_set_index));
        *highest_slot_seen = std::cmp::max(*highest_slot_seen, slot);
    }
}

fn dispatch_recoveries(
    pool: &RecoveryPool,
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    touched: &mut Vec<(Slot, u32)>,
    shreds_buf: &mut Vec<Shred>,
    stats: &DecoderStats,
) {
    // Work on a sorted+deduped local copy so we don't double-dispatch the same FEC set
    // within one tick, and so we don't iterate the same entry multiple times.
    touched.sort_unstable();
    touched.dedup();

    let mut pool_full = false;

    for (slot, fec_set_index) in touched.iter() {
        let Some((fec_map, state_tracker)) = all_shreds.get_mut(slot) else {
            continue;
        };
        let fec_idx = *fec_set_index as usize;
        if state_tracker.already_recovered_fec_sets[fec_idx]
            || state_tracker.recovery_in_flight[fec_idx]
        {
            continue;
        }
        let Some(shreds) = fec_map.get(fec_set_index) else {
            continue;
        };

        let (num_expected_data, _num_expected_coding, num_data, _num_coding) =
            get_data_shred_info(shreds);
        let min_needed = num_expected_data as usize;
        if num_expected_data == 0
            || shreds.len() < min_needed
            || num_data == num_expected_data
        {
            continue;
        }

        let missing = num_expected_data.saturating_sub(num_data);

        shreds_buf.clear();
        shreds_buf.extend(
            shreds
                .iter()
                .sorted_by_key(|s| (u8::MAX - s.shred_type() as u8, s.index()))
                .map(|s| s.0.clone()),
        );
        let shreds_vec = std::mem::take(shreds_buf);

        // Fast-path: small recoveries run synchronously. Channel + wake-up cost
        // dominates the RS decode when only 1-2 data shreds are missing, and the
        // ingest thread is already holding the mutable state we need to update.
        if missing <= INLINE_RECOVERY_MISSING_THRESHOLD {
            let recovered = pool.recover_inline(*slot, *fec_set_index, shreds_vec);

            if recovered.is_empty() {
                stats
                    .fec_recovery_error_count
                    .fetch_add(1, Ordering::Relaxed);
                // Legacy-style: leave FEC set in memory; retry when a new
                // shred arrives or slot ages out via SLOT_LOOKBACK cleanup.
                continue;
            }

            let mut local_recovered = 0u64;
            for shred in recovered {
                if update_state_tracker(&shred, state_tracker).is_some() {
                    local_recovered += 1;
                }
            }
            if local_recovered > 0 {
                stats
                    .recovered_count
                    .fetch_add(local_recovered, Ordering::Relaxed);
            }

            state_tracker.already_recovered_fec_sets[fec_idx] = true;
            if let Some(shreds) = fec_map.get_mut(fec_set_index) {
                shreds.clear();
            }
            // (*slot, *fec_set_index) is still in `touched`; emit_events will
            // pick up the now-complete range on this same tick.
            continue;
        }

        // Slow-path: many missing shreds → hand off to worker pool so the
        // ingest thread isn't blocked on a big RS decode.
        if pool_full {
            continue;
        }
        let job = RecoveryJob {
            slot: *slot,
            fec_set_index: *fec_set_index,
            shreds: shreds_vec,
        };
        match pool.try_dispatch(job) {
            Ok(()) => {
                state_tracker.recovery_in_flight[fec_idx] = true;
            }
            Err(_) => {
                // Queue full — stop dispatching this tick but keep looking for
                // inline-eligible sets later in `touched`.
                pool_full = true;
            }
        }
    }
}

fn apply_recovery_result(
    result: RecoveryResult,
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    touched: &mut Vec<(Slot, u32)>,
    stats: &DecoderStats,
) {
    let RecoveryResult {
        slot,
        fec_set_index,
        recovered,
    } = result;
    let Some((fec_map, state_tracker)) = all_shreds.get_mut(&slot) else {
        return;
    };

    let fec_idx = fec_set_index as usize;
    state_tracker.recovery_in_flight[fec_idx] = false;

    if recovered.is_empty() {
        stats
            .fec_recovery_error_count
            .fetch_add(1, Ordering::Relaxed);
        // Legacy-style: leave FEC set in memory. Retry happens naturally
        // when another shred arrives for this (slot, fec_set_index) and
        // re-adds it to `touched`. If no more shreds arrive, the slot
        // ages out via SLOT_LOOKBACK cleanup below.
        return;
    }

    let mut local_recovered = 0u64;
    for shred in recovered {
        if update_state_tracker(&shred, state_tracker).is_some() {
            local_recovered += 1;
        }
    }
    if local_recovered > 0 {
        stats
            .recovered_count
            .fetch_add(local_recovered, Ordering::Relaxed);
    }

    state_tracker.already_recovered_fec_sets[fec_idx] = true;
    if let Some(shreds) = fec_map.get_mut(&fec_set_index) {
        shreds.clear();
    }

    // Re-visit this FEC set in this same tick's deshred pass.
    touched.push((slot, fec_set_index));
}

fn drain_recovery_results(
    pool: &RecoveryPool,
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    touched: &mut Vec<(Slot, u32)>,
    stats: &DecoderStats,
) {
    while let Some(result) = pool.try_recv() {
        apply_recovery_result(result, all_shreds, touched, stats);
    }
}

fn emit_events(
    all_shreds: &mut ahash::HashMap<
        Slot,
        (
            ahash::HashMap<u32, HashSet<ComparableShred>>,
            ShredsStateTracker,
        ),
    >,
    touched: &[(Slot, u32)],
    handler: &dyn ShredHandler,
    stats: &DecoderStats,
) {
    for (slot, fec_set_index) in touched.iter() {
        let Some((_fec_map, state_tracker)) = all_shreds.get_mut(slot) else {
            continue;
        };
        let Some((start, end, _unknown_start)) =
            get_indexes(state_tracker, *fec_set_index as usize)
        else {
            continue;
        };

        let deshredded_payload = {
            let to_deshred = &state_tracker.data_shreds[start..=end];
            match Shredder::deshred(to_deshred.iter().map(|s| s.as_ref().unwrap().payload())) {
                Ok(v) => v,
                Err(e) => {
                    debug!(
                        slot,
                        fec_set_index,
                        error = %e,
                        "shred-decoder: deshred failed"
                    );
                    stats
                        .fec_recovery_error_count
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
        };

        let entries =
            match bincode::deserialize::<Vec<solana_entry::entry::Entry>>(&deshredded_payload) {
                Ok(entries) => entries,
                Err(e) => {
                    debug!(
                        slot,
                        fec_set_index,
                        error = %e,
                        "shred-decoder: bincode deserialize failed"
                    );
                    stats
                        .bincode_deserialize_error_count
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

        stats
            .entry_count
            .fetch_add(entries.len() as u64, Ordering::Relaxed);
        let entry_txn_count: u64 = entries.iter().map(|e| e.transactions.len() as u64).sum();
        stats.txn_count.fetch_add(entry_txn_count, Ordering::Relaxed);

        for entry in &entries {
            for tx in &entry.transactions {
                let processed_at_micros = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_micros() as u64)
                    .unwrap_or(0);
                let signature = tx.signatures.first().copied().unwrap_or_default();
                let event = TransactionEvent {
                    slot: *slot,
                    signature,
                    transaction: tx,
                    processed_at_micros,
                };
                handler.handle_transaction(&event);
            }
        }

        // Mark all shreds in this range as deshredded so we don't redo work.
        let status_len = state_tracker.data_status.len();
        for i in start..=end {
            if i >= status_len {
                break;
            }
            if let Some(shred) = state_tracker.data_shreds[i].as_ref() {
                let fi = shred.fec_set_index() as usize;
                if fi < state_tracker.already_recovered_fec_sets.len() {
                    state_tracker.already_recovered_fec_sets[fi] = true;
                }
            }
            state_tracker.already_deshredded[i] = true;
        }
    }
}
