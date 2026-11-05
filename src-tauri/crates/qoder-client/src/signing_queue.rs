//! Bounded admission and cancellation for CPU-bound signing. Only one job enters
//! Tokio's blocking pool at a time; the remaining admitted jobs wait asynchronously.
use std::{fmt, sync::Arc, time::Duration};
use tokio::sync::{oneshot, Mutex, OwnedSemaphorePermit, Semaphore};

pub struct Queue {
    slots: Arc<Semaphore>,
    turn: Arc<Mutex<()>>,
}
pub struct Reservation {
    slot: OwnedSemaphorePermit,
    turn: Arc<Mutex<()>>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Busy,
    Timeout,
    Unavailable,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "Qoder signing queue is full",
            Self::Timeout => "Qoder signing deadline exceeded",
            Self::Unavailable => "Qoder signing task is unavailable",
        })
    }
}
impl std::error::Error for Error {}

impl Queue {
    /// Capacity includes both running work and queued reservations.
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(capacity)),
            turn: Arc::new(Mutex::new(())),
        }
    }
    /// Refuse overload immediately, before retaining another body or starting UMID.
    pub fn try_reserve(&self) -> Result<Reservation, Error> {
        Ok(Reservation {
            slot: self
                .slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::Busy)?,
            turn: self.turn.clone(),
        })
    }
}
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}
impl Reservation {
    pub async fn run<T, F>(self, limit: Duration, work: F) -> Result<T, Error>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        tokio::time::timeout(limit, async move {
            let turn = self.turn.lock_owned().await;
            let (tx, rx) = oneshot::channel();
            let task = tokio::task::spawn_blocking(move || {
                // The running job owns admission even if its caller times out.
                // WASM fuel bounds work that has already started and cannot abort.
                let _slot = self.slot;
                let _turn = turn;
                if !tx.is_closed() {
                    let _ = tx.send(work());
                }
            });
            // Abort work still queued in Tokio's blocking pool on cancellation.
            // The closed receiver also makes a dequeued job skip its closure.
            let _abort = AbortOnDrop(task.abort_handle());
            rx.await.map_err(|_| Error::Unavailable)
        })
        .await
        .map_err(|_| Error::Timeout)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    async fn released(queue: &Queue) -> Reservation {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(slot) = queue.try_reserve() {
                    return slot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn queued_timeouts_and_cancellation_drop_bodies_without_running() {
        let queue = Queue::new(2);
        let (started, running) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let first = queue.try_reserve().unwrap();
        let active = tokio::spawn(first.run(Duration::from_secs(5), move || {
            let _ = started.send(());
            wait.recv_timeout(Duration::from_secs(3)).unwrap();
        }));
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        let second = queue.try_reserve().unwrap();
        assert!(matches!(queue.try_reserve(), Err(Error::Busy)));
        let payload = Arc::new(vec![0; 1024]);
        let weak = Arc::downgrade(&payload);
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        assert_eq!(
            second
                .run(Duration::from_millis(20), move || {
                    called.fetch_add(1, Ordering::SeqCst);
                    drop(payload);
                })
                .await,
            Err(Error::Timeout)
        );
        assert!(weak.upgrade().is_none());

        let payload = Arc::new(vec![0; 1024]);
        let weak = Arc::downgrade(&payload);
        let called = calls.clone();
        let mut cancelled = Box::pin(queue.try_reserve().unwrap().run(
            Duration::from_secs(5),
            move || {
                called.fetch_add(1, Ordering::SeqCst);
                drop(payload);
            },
        ));
        assert!(futures::poll!(&mut cancelled).is_pending());
        drop(cancelled);
        assert!(weak.upgrade().is_none());
        assert!(queue.try_reserve().is_ok());
        release.send(()).unwrap();
        active.await.unwrap().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancelled_running_work_holds_its_slot_until_it_finishes() {
        let queue = Queue::new(1);
        let (started, running) = oneshot::channel();
        let (release, wait) = mpsc::channel();
        let active = tokio::spawn(queue.try_reserve().unwrap().run(
            Duration::from_secs(5),
            move || {
                let _ = started.send(());
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
            },
        ));
        tokio::time::timeout(Duration::from_secs(2), running)
            .await
            .unwrap()
            .unwrap();
        active.abort();
        assert!(active.await.unwrap_err().is_cancelled());
        assert!(matches!(queue.try_reserve(), Err(Error::Busy)));
        release.send(()).unwrap();
        assert_eq!(
            released(&queue)
                .await
                .run(Duration::from_secs(1), || 42)
                .await,
            Ok(42)
        );
    }

    #[test]
    fn a_timed_out_job_in_tokios_blocking_queue_is_never_executed() {
        // Saturate the blocking pool so the signer has been spawned but cannot start.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let queue = Queue::new(1);
            let (started, running) = oneshot::channel();
            let (release, wait) = mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                let _ = started.send(());
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
            });
            tokio::time::timeout(Duration::from_secs(2), running)
                .await
                .unwrap()
                .unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let called = calls.clone();
            assert_eq!(
                queue
                    .try_reserve()
                    .unwrap()
                    .run(Duration::from_millis(20), move || {
                        called.fetch_add(1, Ordering::SeqCst);
                    })
                    .await,
                Err(Error::Timeout)
            );
            release.send(()).unwrap();
            blocker.await.unwrap();
            let next = released(&queue).await;
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(next.run(Duration::from_secs(1), || 7).await, Ok(7));
        });
    }
}
