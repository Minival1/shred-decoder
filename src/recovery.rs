use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use crossbeam_channel::{bounded, Receiver, Sender};
use solana_ledger::shred::{
    merkle::{self, Shred},
    ReedSolomonCache,
};
use solana_sdk::clock::Slot;
use tracing::debug;

pub(crate) struct RecoveryJob {
    pub slot: Slot,
    pub fec_set_index: u32,
    pub shreds: Vec<Shred>,
}

pub(crate) struct RecoveryResult {
    pub slot: Slot,
    pub fec_set_index: u32,
    pub recovered: Vec<Shred>,
}

pub(crate) struct RecoveryPool {
    job_tx: Sender<RecoveryJob>,
    result_rx: Receiver<RecoveryResult>,
    handles: Vec<JoinHandle<()>>,
    exit: Arc<AtomicBool>,
    /// Cache used only by the ingest thread's inline recoveries. Kept
    /// separate from `worker_cache` so that inline calls don't contend
    /// with pool workers on the cache's internal locks.
    inline_cache: Arc<ReedSolomonCache>,
}

impl RecoveryPool {
    pub fn new(workers: usize) -> Self {
        let workers = workers.max(1);
        let queue_depth = (workers * 4).max(8);
        let (job_tx, job_rx) = bounded::<RecoveryJob>(queue_depth);
        let (result_tx, result_rx) = bounded::<RecoveryResult>(queue_depth);
        let exit = Arc::new(AtomicBool::new(false));
        let worker_cache = Arc::new(ReedSolomonCache::default());
        let inline_cache = Arc::new(ReedSolomonCache::default());

        let mut handles = Vec::with_capacity(workers);
        for worker_id in 0..workers {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let rs_cache = worker_cache.clone();
            let exit = exit.clone();
            let handle = std::thread::Builder::new()
                .name(format!("shredRecover{worker_id}"))
                .spawn(move || worker_loop(job_rx, result_tx, rs_cache, exit))
                .expect("failed to spawn recovery worker thread");
            handles.push(handle);
        }

        Self {
            job_tx,
            result_rx,
            handles,
            exit,
            inline_cache,
        }
    }

    /// Synchronous recovery on the caller's thread. Use this when dispatch
    /// overhead (channel send + worker wake-up + result poll) is likely to
    /// dominate the actual Reed-Solomon decode cost, e.g. for small FEC sets
    /// or under light load.
    pub fn recover_inline(&self, slot: Slot, fec_set_index: u32, shreds: Vec<Shred>) -> Vec<Shred> {
        run_recovery(slot, fec_set_index, shreds, &self.inline_cache, "inline")
    }

    /// Non-blocking dispatch. Returns the job back to the caller if the queue is full,
    /// so the caller can retry on the next tick without losing state.
    pub fn try_dispatch(&self, job: RecoveryJob) -> Result<(), RecoveryJob> {
        match self.job_tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(j)) => Err(j),
            Err(crossbeam_channel::TrySendError::Disconnected(j)) => Err(j),
        }
    }

    pub fn try_recv(&self) -> Option<RecoveryResult> {
        self.result_rx.try_recv().ok()
    }

    /// Direct access to the result channel so callers can compose it into a
    /// `crossbeam_channel::select!` alongside their own sources.
    pub fn result_receiver(&self) -> &Receiver<RecoveryResult> {
        &self.result_rx
    }

    pub fn shutdown(self) {
        self.exit.store(true, Ordering::Release);
        drop(self.job_tx);
        for h in self.handles {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    job_rx: Receiver<RecoveryJob>,
    result_tx: Sender<RecoveryResult>,
    rs_cache: Arc<ReedSolomonCache>,
    exit: Arc<AtomicBool>,
) {
    while !exit.load(Ordering::Relaxed) {
        let job = match job_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(j) => j,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        };

        let slot = job.slot;
        let fec_set_index = job.fec_set_index;
        let recovered = run_recovery(slot, fec_set_index, job.shreds, &rs_cache, "pool");

        if result_tx
            .send(RecoveryResult {
                slot,
                fec_set_index,
                recovered,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Internal helper used by both the worker pool and the inline fast-path.
///
/// Uses `merkle::recover` directly — it's `pub` in the jito-solana 2.2 fork
/// (`eric/v2.2-merkle-recovery`) and takes/returns `merkle::Shred`, so no
/// conversion wrapping is needed.
fn run_recovery(
    slot: Slot,
    fec_set_index: u32,
    shreds: Vec<Shred>,
    rs_cache: &ReedSolomonCache,
    site: &'static str,
) -> Vec<Shred> {
    match merkle::recover(shreds, rs_cache) {
        Ok(iter) => iter
            .filter_map(|r| match r {
                Ok(s) => Some(s),
                Err(e) => {
                    debug!(
                        slot,
                        fec_set_index,
                        site,
                        error = %e,
                        "shred-decoder: failed to recover individual shred"
                    );
                    None
                }
            })
            .collect(),
        Err(e) => {
            debug!(
                slot,
                fec_set_index,
                site,
                error = %e,
                "shred-decoder: FEC recovery failed"
            );
            Vec::new()
        }
    }
}
