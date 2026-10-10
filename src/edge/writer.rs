//! The book's writer: commits the changes each input makes to the [`Store`], in the order the
//! book made them, behind the hot path. An inbound message whose handling changed the book is
//! held in flight ([`Hold`]) until the batch with its changes has committed, so a stop before
//! then has its session redeliver it; nothing on the hot path waits for the database.
//!
//! Batches queue on one channel, unbounded in type but bounded in practice by the holds: a
//! session stops reading 256 messages past its oldest held one, so a database that's down or slow
//! slows the counterparties, not the gateway's memory. While the oldest uncommitted batch has
//! waited longer than [`MAX_COMMIT_WAIT`], the gateway rejects new orders (see
//! [`Book::set_unavailable`](crate::book::Book::set_unavailable)).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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

/// How long the oldest uncommitted batch may wait before new orders are rejected as "gateway
/// unavailable". Age, not a count: a burst queues hundreds of batches that commit within
/// milliseconds, and the holds stop each session at 256 messages, so a count can't tell a burst
/// from a database that's failing or lagging. A healthy commit takes milliseconds; a second
/// means the database is down or far behind, and booking more would only grow what a crash
/// makes clients resend. Cancels, replaces and fills keep flowing, each held until it commits.
pub const MAX_COMMIT_WAIT: Duration = Duration::from_secs(1);

/// The writer's wait after a failed write, doubling to [`RETRY_MAX`]: soon enough for a blip,
/// without hammering a database that's down.
const RETRY_FIRST: Duration = Duration::from_millis(10);
const RETRY_MAX: Duration = Duration::from_secs(1);

// A database that keeps failing has the gateway unavailable between retries, too.
const _: () = assert!(RETRY_MAX.as_millis() <= MAX_COMMIT_WAIT.as_millis());

/// One input's changes, and what waits on their commit.
#[derive(Debug)]
struct Batch {
    changes: Changes,
    /// The inbound message the changes came from, released once they've committed.
    hold: Option<Hold>,
    /// Told once the changes have committed.
    done: Option<oneshot::Sender<()>>,
    queued: Instant,
}

/// Queues batches for the writer task: see the [module documentation](self). Cloned, it queues on
/// the same task.
#[derive(Debug, Clone)]
pub struct Writer {
    queue: mpsc::UnboundedSender<Batch>,
    uncommitted: Arc<AtomicUsize>,
    /// The next `undelivered` row's ID.
    next_row: Arc<AtomicU64>,
    /// Set once the writer task has been cancelled: see [`Supervised`].
    stopping: Arc<AtomicBool>,
    /// When the oldest batch not yet committed was queued, as nanoseconds after `start` plus
    /// one; zero when there's none.
    oldest: Arc<AtomicU64>,
    start: Instant,
    max_wait: Duration,
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
        let stopping = Arc::new(AtomicBool::new(false));
        let (oldest, start) = (Arc::new(AtomicU64::new(0)), Instant::now());
        let task = Task { batches, waiting: Vec::with_capacity(MAX_BATCHES_PER_WRITE) };
        let ages = Ages { oldest: oldest.clone(), start };
        let future = Box::pin(run(store, task, uncommitted.clone(), ages));
        let writing = tokio::spawn(Supervised { future, polling: false, stopping: stopping.clone() });
        let next_row = Arc::new(AtomicU64::new(next_row));
        let writer = Self { queue, uncommitted, next_row, stopping, oldest, start, max_wait: MAX_COMMIT_WAIT };
        (writer, writing)
    }

    /// Rejects new orders once a batch has waited `max_wait` rather than [`MAX_COMMIT_WAIT`].
    #[cfg(test)]
    pub(crate) fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Queues `changes` to commit after everything queued before them, holding `hold` until they
    /// have.
    pub(crate) fn queue(&self, changes: Changes, hold: Option<Hold>) {
        self.send(Batch { changes, hold, done: None, queued: Instant::now() });
    }

    /// Queues the deletion of undelivered row `row`: its report reached the client's session, or
    /// was lost.
    pub(crate) fn delivered(&self, row: u64) {
        self.queue(Changes { delivered: vec![row], ..Changes::default() }, None);
    }

    /// Completes once everything queued so far has committed.
    pub(crate) async fn committed(&self) {
        let (done, committed) = oneshot::channel();
        self.send(Batch { changes: Changes::default(), hold: None, done: Some(done), queued: Instant::now() });
        if committed.await.is_err() {
            self.gone(None);
            // Never as if committed.
            std::future::pending::<()>().await;
        }
    }

    /// A new `undelivered` row's ID.
    pub(crate) fn next_row(&self) -> u64 {
        self.next_row.fetch_add(1, Ordering::Relaxed)
    }

    /// Whether the database is too far behind to take new orders: the oldest batch not yet
    /// committed has waited longer than the maximum.
    pub(crate) fn unavailable(&self) -> bool {
        let oldest = self.oldest.load(Ordering::Acquire);
        oldest != 0 && self.start.elapsed() > Duration::from_nanos(oldest - 1) + self.max_wait
    }

    /// Batches queued and not yet committed.
    pub fn uncommitted(&self) -> usize {
        self.uncommitted.load(Ordering::Acquire)
    }

    fn send(&self, batch: Batch) {
        self.uncommitted.fetch_add(1, Ordering::AcqRel);
        if let Err(refused) = self.queue.send(batch) {
            self.uncommitted.fetch_sub(1, Ordering::AcqRel);
            self.gone(refused.0.hold);
        }
    }

    /// The writer task is gone, and `hold`'s message would be acknowledged with its changes never
    /// written. While the gateway is being torn down (its task cancelled), the hold is forgotten,
    /// never released, so the message is redelivered after a restart. Otherwise the task died
    /// some other way: stop, as for a panic inside the book.
    fn gone(&self, hold: Option<Hold>) {
        if !self.stopping.load(Ordering::Acquire) {
            error!("the book's writer has stopped; stopping rather than acknowledge what isn't written");
            std::process::abort();
        }
        std::mem::forget(hold);
    }
}

/// The writer task's future. A panic inside it leaves `polling` set when tokio drops it, which
/// stops the process, since its batches' holds would otherwise be released unwritten. Dropped
/// otherwise, the task was cancelled (the service dropped, or the runtime shut down): `stopping`
/// is set, so writers forget the holds of what they can no longer queue, and [`Task`] forgets
/// those it had.
struct Supervised<F> {
    future: Pin<Box<F>>,
    polling: bool,
    stopping: Arc<AtomicBool>,
}

impl<F: Future<Output = ()>> Future for Supervised<F> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.polling = true;
        let poll = this.future.as_mut().poll(cx);
        this.polling = false;
        poll
    }
}

impl<F> Drop for Supervised<F> {
    fn drop(&mut self) {
        if self.polling {
            error!("the book's writer panicked; stopping rather than acknowledge what isn't written");
            std::process::abort();
        }
        // Before the future, and with it the queue's receiver, is dropped.
        self.stopping.store(true, Ordering::Release);
    }
}

/// Where the writer task says how old its oldest batch is: see [`Writer::unavailable`].
struct Ages {
    oldest: Arc<AtomicU64>,
    start: Instant,
}

impl Ages {
    /// `queued` is the oldest batch's time; `None` once it's committed.
    fn set(&self, queued: Option<Instant>) {
        let nanos = queued.map_or(0, |queued| {
            let since = queued.saturating_duration_since(self.start).as_nanos();
            u64::try_from(since).unwrap_or(u64::MAX - 1) + 1
        });
        self.oldest.store(nanos, Ordering::Release);
    }
}

/// What the writer task holds: its queue, and the holds and waiters of the batches it's writing.
struct Task {
    batches: mpsc::UnboundedReceiver<Batch>,
    waiting: Vec<(Option<Hold>, Option<oneshot::Sender<()>>)>,
}

impl Drop for Task {
    /// Cancelled with batches unwritten: their holds are forgotten, not released.
    fn drop(&mut self) {
        self.batches.close();
        let queued = std::iter::from_fn(|| self.batches.try_recv().ok()).map(|batch| batch.hold);
        for hold in self.waiting.drain(..).map(|(hold, _)| hold).chain(queued) {
            std::mem::forget(hold);
        }
    }
}

/// The writer task: takes what's queued, up to [`MAX_BATCHES_PER_WRITE`], writes it in one
/// transaction, retrying until it commits, then lets go of its holds and says it's done.
async fn run(store: Store, mut task: Task, uncommitted: Arc<AtomicUsize>, ages: Ages) {
    // Reused from one write to the next.
    let mut changes = Vec::with_capacity(MAX_BATCHES_PER_WRITE);
    while let Some(first) = task.batches.recv().await {
        // The oldest not committed: those queued behind it are younger. Between a commit and the
        // next receive it reads as none, for as long as taking the next batch takes.
        ages.set(Some(first.queued));
        let mut next = Some(first);
        while let Some(Batch { changes: batch, hold, done, queued: _ }) = next {
            changes.push(batch);
            task.waiting.push((hold, done));
            next = if changes.len() < MAX_BATCHES_PER_WRITE { task.batches.try_recv().ok() } else { None };
        }
        let written = changes.len();
        // Rows changed by many batches are written once: statements, not the commit, are most of
        // a large write's time.
        let merged = Changes::merge(changes.drain(..));
        write(&store, std::slice::from_ref(&merged), written).await;
        ages.set(None);
        for (hold, done) in task.waiting.drain(..) {
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

/// Writes `changes`, merged from `batches` batches, in one transaction, retrying until it commits. Every row is an upsert or a
/// delete by key, so writing them again after a failure writes each once.
async fn write(store: &Store, changes: &[Changes], batches: usize) {
    let mut wait = RETRY_FIRST;
    let mut failures: u32 = 0;
    while let Err(e) = store.write(changes).await {
        failures = failures.saturating_add(1);
        error!(
            batches,
            failures,
            retry_in = ?wait,
            "cannot commit the book's changes; their messages stay held: {e}"
        );
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(RETRY_MAX);
    }
    if failures > 0 {
        info!(batches, failures, "committed the book's changes after failing");
    }
}
