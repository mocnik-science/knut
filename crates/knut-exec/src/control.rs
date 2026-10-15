//! Progress and cancellation of a running script (used by `knut web`).
//!
//! Scans report the number of blocks they will read and every finished block; outputs
//! report which statement is running. Both cost one atomic operation per block, which is
//! negligible next to decoding the block. Cancelling sets a flag that scans and set
//! iterations check; the run then stops with E902.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Default)]
pub struct Control {
    cancel: AtomicBool,
    done: AtomicU64,
    total: AtomicU64,
    step: AtomicUsize,
    steps: AtomicUsize,
    label: Mutex<String>,
}

/// A snapshot of the progress.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Progress {
    /// Overall fraction in [0, 1] if known: finished outputs plus the share of blocks read
    /// in the current one.
    pub fraction: Option<f64>,
    /// Output being computed (1-based) and number of outputs.
    pub step: usize,
    pub steps: usize,
    /// Blocks read and to read in the current output.
    pub blocks_done: u64,
    pub blocks_total: u64,
    pub label: String,
}

impl Control {
    pub fn new() -> std::sync::Arc<Control> {
        std::sync::Arc::new(Control::default())
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Output `step` (0-based) of `steps` starts.
    pub(crate) fn begin_step(&self, step: usize, steps: usize, label: &str) {
        self.done.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.steps.store(steps, Ordering::Relaxed);
        self.step.store(step, Ordering::Relaxed);
        if let Ok(mut l) = self.label.lock() {
            *l = label.to_string();
        }
    }

    pub(crate) fn add_blocks(&self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn block_done(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn progress(&self) -> Progress {
        let (done, total) = (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        let (step, steps) = (self.step.load(Ordering::Relaxed), self.steps.load(Ordering::Relaxed));
        let inner = if total > 0 { Some(done.min(total) as f64 / total as f64) } else { None };
        let fraction = (steps > 0).then(|| (step as f64 + inner.unwrap_or(0.0)) / steps as f64).filter(|_| total > 0 || step > 0);
        Progress {
            fraction,
            step: (step + 1).min(steps.max(1)),
            steps,
            blocks_done: done,
            blocks_total: total,
            label: self.label.lock().map(|l| l.clone()).unwrap_or_default(),
        }
    }
}
