use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::JoinHandle,
};

use crossbeam_channel::{bounded, Receiver, Sender};
use solana_ledger::shred::{merkle::Shred, ReedSolomonCache};
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
}

impl RecoveryPool {
    pub fn new(workers: usize) -> Self {
        let workers = workers.max(1);
        let queue_depth = (workers * 4).max(8);
        let (job_tx, job_rx) = bounded::<RecoveryJob>(queue_depth);
        let (result_tx, result_rx) = bounded::<RecoveryResult>(queue_depth);
        let exit = Arc::new(AtomicBool::new(false));
        let rs_cache = Arc::new(ReedSolomonCache::default());

        let mut handles = Vec::with_capacity(workers);
        for worker_id in 0..workers {
            let job_rx = job_rx.clone();
            let result_tx = result_tx.clone();
            let rs_cache = rs_cache.clone();
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
        }
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
        let recovered = match solana_ledger::shred::merkle::recover(job.shreds, &rs_cache) {
            Ok(recovered) => recovered
                .into_iter()
                .filter_map(|r| match r {
                    Ok(s) => Some(s),
                    Err(e) => {
                        debug!(
                            slot,
                            fec_set_index,
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
                    error = %e,
                    "shred-decoder: FEC recovery failed"
                );
                Vec::new()
            }
        };

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
