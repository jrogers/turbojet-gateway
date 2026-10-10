//! The book's writer: commits the changes each input makes to the [`Store`], in the order the
//! book made them, behind the hot path. An inbound message whose handling changed the book is
//! held in flight ([`Hold`]) until the batch with its changes has committed, so a stop before
//! then has its session redeliver it; nothing on the hot path waits for the database.
//!
//! Batches queue on one channel, unbounded in type but bounded in practice by the holds: a
//! session stops reading 256 messages past its oldest held one, so a database that's down or slow
//! slows the counterparties, not the gateway's memory. Past [`MAX_UNCOMMITTED`] batches the
//! gateway rejects new orders (see [`Book::set_unavailable`](crate::book::Book::set_unavailable)).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, info};
use turbojet::Hold;

use crate::book::Changes;
use crate::store::Store;

/// Batches written in one transaction, at most. A commit costs an fsync whatever it holds, so the
/// writer takes all that's queued; this bounds one transaction's size and the time its holds wait
/// on it. A thousand inputs' rows is a few hundred kilobytes.
pub const MAX_BATCHES_PER_WRITE: usize = 1_000;

/// Uncommitted batches past which new orders are rejected as "gateway unavailable". A healthy
/// writer is a transaction or two behind; ten writes' worth means the database is failing or far
/// slower than the flow, and booking more would only grow what a crash makes clients resend.
/// Cancels, replaces and fills keep flowing, each held until it commits.
pub const MAX_UNCOMMITTED: usize = 10 * MAX_BATCHES_PER_WRITE;

/// The writer's wait after a failed write, doubling to [`RETRY_MAX`]: soon enough for a blip,
/// without hammering a database that's down.
const RETRY_FIRST: Duration = Duration::from_millis(10);
const RETRY_MAX: Duration = Duration::from_secs(1);

const _: () = assert!(MAX_BATCHES_PER_WRITE <= MAX_UNCOMMITTED);

/// One input's changes, and what waits on their commit.
#[derive(Debug)]
struct Batch {
    changes: Changes,
    /// The inbound message the changes came from, released once they've committed.
    hold: Option<Hold>,
    /// Told once the changes have committed.
    done: Option<oneshot::Sender<()>>,
}

/// Queues batches for the writer task: see the [module documentation](self). Cloned, it queues on
/// the same task.
#[derive(Debug, Clone)]
pub struct Writer {
    queue: mpsc::UnboundedSender<Batch>,
    uncommitted: Arc<AtomicUsize>,
    /// The next `undelivered` row's ID.
    next_row: Arc<AtomicU64>,
    limit: usize,
}

impl Writer {
    /// Starts the writer task on `store`, numbering undelivered rows from `next_row`, past every
    /// row the store has (see [`Snapshot::next_undelivered_id`]). The task ends once every
    /// `Writer` is dropped and the queue is written.
    ///
    /// [`Snapshot::next_undelivered_id`]: crate::book::Snapshot::next_undelivered_id
    pub fn start(store: Store, next_row: u64) -> (Self, JoinHandle<()>) {
        let (queue, batches) = mpsc::unbounded_channel();
        let uncommitted = Arc::new(AtomicUsize::new(0));
        let writing = tokio::spawn(run(store, batches, uncommitted.clone()));
        let writer = Self { queue, uncommitted, next_row: Arc::new(AtomicU64::new(next_row)), limit: MAX_UNCOMMITTED };
        (writer, writing)
    }

    /// Rejects new orders past `limit` uncommitted batches rather than [`MAX_UNCOMMITTED`].
    #[cfg(test)]
    pub(crate) fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Queues `changes` to commit after everything queued before them, holding `hold` until they
    /// have.
    pub(crate) fn queue(&self, changes: Changes, hold: Option<Hold>) {
        self.send(Batch { changes, hold, done: None });
    }

    /// Queues the deletion of undelivered row `row`: its report reached the client's session, or
    /// was lost.
    pub(crate) fn delivered(&self, row: u64) {
        self.queue(Changes { delivered: vec![row], ..Changes::default() }, None);
    }

    /// Completes once everything queued so far has committed.
    pub(crate) async fn committed(&self) {
        let (done, committed) = oneshot::channel();
        self.send(Batch { changes: Changes::default(), hold: None, done: Some(done) });
        if committed.await.is_err() {
            error!("the book's writer stopped before committing");
        }
    }

    /// A new `undelivered` row's ID.
    pub(crate) fn next_row(&self) -> u64 {
        self.next_row.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether the database is too far behind to take new orders.
    pub(crate) fn unavailable(&self) -> bool {
        self.uncommitted() >= self.limit
    }

    /// Batches queued and not yet committed.
    pub fn uncommitted(&self) -> usize {
        self.uncommitted.load(Ordering::Acquire)
    }

    fn send(&self, batch: Batch) {
        self.uncommitted.fetch_add(1, Ordering::AcqRel);
        // Gone only if its task was aborted (the service dropped) or panicked: the batch is lost,
        // its hold released. Not a panic here, on a session's task.
        if self.queue.send(batch).is_err() {
            self.uncommitted.fetch_sub(1, Ordering::AcqRel);
            error!("the book's writer has stopped; a change is not written");
        }
    }
}

/// The writer task: takes what's queued, up to [`MAX_BATCHES_PER_WRITE`], writes it in one
/// transaction, retrying until it commits, then lets go of its holds and says it's done.
async fn run(store: Store, mut batches: mpsc::UnboundedReceiver<Batch>, uncommitted: Arc<AtomicUsize>) {
    // Reused from one write to the next.
    let mut changes = Vec::with_capacity(MAX_BATCHES_PER_WRITE);
    let mut waiting = Vec::with_capacity(MAX_BATCHES_PER_WRITE);
    while let Some(first) = batches.recv().await {
        let mut next = Some(first);
        while let Some(Batch { changes: batch, hold, done }) = next {
            changes.push(batch);
            waiting.push((hold, done));
            next = if changes.len() < MAX_BATCHES_PER_WRITE { batches.try_recv().ok() } else { None };
        }
        write(&store, &changes).await;
        let written = changes.len();
        changes.clear();
        for (hold, done) in waiting.drain(..) {
            drop(hold);
            if let Some(done) = done {
                // The caller may have stopped waiting.
                let _ = done.send(());
            }
        }
        let before = uncommitted.fetch_sub(written, Ordering::AcqRel);
        debug_assert!(before >= written, "committed more batches than were queued");
    }
    info!("the book's writer stopped: nothing left to write");
}

/// Writes `changes` in one transaction, retrying until it commits. Every row is an upsert or a
/// delete by key, so writing them again after a failure writes each once.
async fn write(store: &Store, changes: &[Changes]) {
    let mut wait = RETRY_FIRST;
    let mut failures: u32 = 0;
    while let Err(e) = store.write(changes).await {
        failures = failures.saturating_add(1);
        error!(
            batches = changes.len(),
            failures,
            retry_in = ?wait,
            "cannot commit the book's changes; their messages stay held: {e}"
        );
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
    if failures > 0 {
        info!(batches = changes.len(), failures, "committed the book's changes after failing");
    }
}
