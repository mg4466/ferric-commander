//! Background I/O: the message contract between the UI thread and a worker.
//!
//! The worker owns nothing the UI needs to borrow. It receives an owned
//! [`FileTask`], talks back over an `mpsc` channel, and can be stopped with a
//! [`WorkerControl::Cancel`] on a second channel. That is the whole threading
//! design: no `Arc<Mutex<..>>`, no shared counters, no locks in the render path.
//!
//! Progress is deliberately *coalesced*. A 64 KB chunk on a 5 GB file would
//! otherwise produce ~80,000 messages, and the UI only drains 100 per frame, so
//! the gauge would lag further behind the longer the copy ran. The worker emits
//! at most one `Progress` every 50 ms (and always once when a file completes),
//! carrying absolute counters, so a dropped intermediate message never loses
//! information.

pub mod worker;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

pub use worker::spawn_worker;

/// How often a running worker may publish a progress update.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);

/// Which filesystem operation a task performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOperation {
    Copy,
    Move,
    Delete,
}

impl IoOperation {
    /// Title-case form, for dialog headings and the progress modal.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Move => "Move",
            Self::Delete => "Delete",
        }
    }

    /// Present-participle form, for "Copying file.iso".
    pub fn gerund(self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Delete => "Deleting",
        }
    }
}

/// What to do when a destination path already exists.
///
/// The decision is taken once, in the confirmation dialog, and then applied
/// uniformly to every collision the task meets — including collisions nested
/// inside directories, which the user never saw listed. Prompting per nested
/// file would be unusable, and silently overwriting was ruled out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Overwrite,
    Skip,
}

/// A fully-resolved operation. Everything the worker needs is in here: paths
/// are absolute, conflicts have been acknowledged, and no further decisions
/// will be requested from the user.
#[derive(Debug, Clone)]
pub struct FileTask {
    pub op: IoOperation,
    /// Absolute source paths. The worker never re-reads the panel for these.
    pub sources: Vec<PathBuf>,
    /// Destination directory; `None` for delete.
    pub destination: Option<PathBuf>,
    pub policy: ConflictPolicy,
}

/// Messages from the worker to the UI.
#[derive(Debug, Clone)]
pub enum TaskUpdate {
    /// Emitted once, after the pre-flight scan, before any I/O happens.
    Started {
        total_items: usize,
        total_bytes: u64,
    },
    Progress {
        current_file: String,
        items_processed: usize,
        bytes_processed: u64,
    },
    /// One item failed but the task continues.
    NonFatalError { path: PathBuf, error: String },
    /// Terminal: the task reached the end of its list.
    ///
    /// `skipped_errors` are genuine failures; `skipped` are destinations the
    /// user chose not to overwrite. Keeping them apart means the error report
    /// is not polluted with expected outcomes.
    Completed {
        skipped_errors: Vec<(PathBuf, String)>,
        skipped: Vec<PathBuf>,
    },
    /// Terminal: the task stopped early (cancellation, or a fatal condition).
    /// Carries the counters so the UI can say how much of the work survived.
    Aborted {
        reason: String,
        items_done: usize,
        bytes_done: u64,
    },
}

/// Commands the UI can send into a running worker.
#[derive(Debug, Clone, Copy)]
pub enum WorkerControl {
    Cancel,
}

impl TaskUpdate {
    /// True when the worker has said its last word and the handle can go.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskUpdate::Completed { .. } | TaskUpdate::Aborted { .. }
        )
    }
}

/// The UI's view of the running task: its channel, what it is doing, and how far
/// it has got. The worker never touches this; `App` updates it from messages.
#[derive(Debug)]
pub struct ActiveTaskHandle {
    pub rx: Receiver<TaskUpdate>,
    pub cancel_tx: Sender<WorkerControl>,
    pub op: IoOperation,
    pub phase: TaskPhase,
    pub current_file: String,
    pub items_done: usize,
    pub total_items: usize,
    pub bytes_done: u64,
    pub total_bytes: u64,
    /// Failures reported so far, mirrored here so the status line can show a
    /// running count without waiting for the final error report.
    pub error_count: usize,
    pub skipped_count: usize,
    /// The most recent failure, shown live in the progress dialog. The complete
    /// list only exists in the worker until `Completed` arrives, so mirroring
    /// the last one is what lets the UI admit something went wrong mid-task.
    pub last_error: Option<String>,
}

/// Where the worker is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPhase {
    /// Pre-flight scan: totals are not known yet.
    Scanning,
    /// Doing I/O; totals are known.
    Running,
    /// A cancel has been requested; the worker has not acknowledged it yet.
    Cancelling,
}

impl ActiveTaskHandle {
    /// Completion in the range 0.0..=1.0.
    ///
    /// Byte-weighted when the size is known (which is what makes a bar move
    /// smoothly across a few huge files) and item-weighted when it is not (a
    /// delete, or a tree of empty files).
    pub fn fraction(&self) -> f64 {
        if self.total_bytes > 0 {
            (self.bytes_done as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
        } else if self.total_items > 0 {
            (self.items_done as f64 / self.total_items as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Ask the worker to stop. Idempotent: pressing Esc twice is harmless.
    pub fn request_cancel(&mut self) {
        if self.phase != TaskPhase::Cancelling {
            // A disconnected receiver means the worker already finished; the
            // send failure is the answer, not an error worth reporting.
            let _ = self.cancel_tx.send(WorkerControl::Cancel);
            self.phase = TaskPhase::Cancelling;
        }
    }
}
