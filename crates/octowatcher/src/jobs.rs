//! Runs the engine's jobs for the Rune shell. Blocking work gets a thread; async work (timers,
//! notification callbacks) is polled on the UI thread whenever something wakes it. Finished jobs
//! come back as engine events through `finished`.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake};
use std::time::Duration;

use octowatcher_core::engine::{Event, Job, JobKey, Sleep, Work};

type WakeFn = Arc<dyn Fn() + Send + Sync>;

/// Asks the UI thread to call [`Jobs::finished`]. Wakes are coalesced until the UI takes them.
#[derive(Clone)]
pub struct Waker {
    pending: Arc<AtomicBool>,
    wake: Arc<Mutex<Option<WakeFn>>>,
}

impl Waker {
    fn new() -> Self {
        Self {
            pending: Arc::new(AtomicBool::new(false)),
            wake: Arc::default(),
        }
    }

    pub fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(wake) = self.wake.lock().unwrap().clone()
        {
            wake();
        }
    }

    fn take(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

impl Wake for Waker {
    fn wake(self: Arc<Self>) {
        Waker::wake(&self);
    }
}

type LocalFuture = Pin<Box<dyn Future<Output = Event>>>;

pub struct Jobs {
    /// The id each running job was started under; a replaced job's answer is dropped.
    running: HashMap<JobKey, u64>,
    local: Vec<(JobKey, u64, LocalFuture)>,
    next: u64,
    done_tx: mpsc::Sender<(JobKey, u64, Event)>,
    done_rx: mpsc::Receiver<(JobKey, u64, Event)>,
    waker: Waker,
    /// Tests record started jobs instead of running gh, timers or notifications.
    pub record_only: bool,
    pub started: Vec<JobKey>,
}

impl Default for Jobs {
    fn default() -> Self {
        let (done_tx, done_rx) = mpsc::channel();
        Self {
            running: HashMap::new(),
            local: Vec::new(),
            next: 0,
            done_tx,
            done_rx,
            waker: Waker::new(),
            record_only: false,
            started: Vec::new(),
        }
    }
}

impl Jobs {
    /// Connects the wake-up that runs [`Jobs::finished`] on the UI thread.
    pub fn connect(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.waker.wake.lock().unwrap() = Some(wake);
        self.waker.pending.store(false, Ordering::Release);
        self.waker.wake();
    }

    /// Starts `job`, replacing any running job with its key.
    pub fn start(&mut self, job: Job) {
        let key = job.key();
        if self.record_only {
            self.started.push(key);
            return;
        }
        let id = self.next;
        self.next += 1;
        self.running.insert(key, id);
        self.local.retain(|(running, _, _)| *running != key);
        let waker = self.waker.clone();
        let sleep: Sleep = Rc::new(move |duration| Box::pin(sleep(duration, waker.clone())));
        match job.run(sleep) {
            Work::Blocking(work) => {
                let done = self.done_tx.clone();
                let waker = self.waker.clone();
                std::thread::Builder::new()
                    .name(format!("octowatcher-{key:?}").to_lowercase())
                    .spawn(move || {
                        done.send((key, id, work().into())).ok();
                        waker.wake();
                    })
                    .expect("could not start a worker thread");
            }
            Work::Async(future) => {
                self.local.push((key, id, future));
                self.waker.wake();
            }
        }
    }

    /// Dropping a job cancels it; a blocking one runs on, and its answer is dropped.
    pub fn cancel(&mut self, key: JobKey) {
        self.running.remove(&key);
        self.local.retain(|(running, _, _)| *running != key);
    }

    /// Events from jobs that finished since the last call, oldest first.
    pub fn finished(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if self.waker.take() {
            let waker = std::task::Waker::from(Arc::new(self.waker.clone()));
            let mut cx = Context::from_waker(&waker);
            let mut ix = 0;
            while ix < self.local.len() {
                if let Poll::Ready(event) = self.local[ix].2.as_mut().poll(&mut cx) {
                    let (key, id, _) = self.local.remove(ix);
                    if self.running.get(&key) == Some(&id) {
                        self.running.remove(&key);
                        events.push(event);
                    }
                } else {
                    ix += 1;
                }
            }
        }
        while let Ok((key, id, event)) = self.done_rx.try_recv() {
            if self.running.get(&key) == Some(&id) {
                self.running.remove(&key);
                events.push(event);
            }
        }
        events
    }
}

use std::rc::Rc;

/// A timer that wakes the UI thread when it's due. The thread holds only the waker, so a
/// cancelled timer costs one sleeping thread until its deadline.
fn sleep(duration: Duration, waker: Waker) -> impl Future<Output = ()> {
    let due = Arc::new(AtomicBool::new(false));
    let mut started = false;
    std::future::poll_fn(move |_| {
        if due.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        if !started {
            started = true;
            let (due, waker) = (due.clone(), waker.clone());
            std::thread::Builder::new()
                .name("octowatcher-timer".into())
                .spawn(move || {
                    std::thread::sleep(duration);
                    due.store(true, Ordering::Release);
                    waker.wake();
                })
                .expect("could not start a timer thread");
        }
        Poll::Pending
    })
}
