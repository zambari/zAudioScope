//! A background thread that keeps only the newest job and the newest result.
//!
//! The UI thread `submit`s work and `take`s whatever finished, never waiting.
//! Submitting while a job is still queued replaces it (stale audio is worth
//! nothing), and an uncollected result is likewise replaced by a newer one.
//! Readings lag by however long a job takes, usually under a frame.

use std::sync::{Arc, Condvar, Mutex};

struct Slot<J, R> {
    job: Option<J>,
    result: Option<R>,
    closed: bool,
}

type Link<J, R> = Arc<(Mutex<Slot<J, R>>, Condvar)>;

pub struct Latest<J, R> {
    link: Link<J, R>,
}

impl<J: Send + 'static, R: Send + 'static> Latest<J, R> {
    /// Start a thread named `name` running `work` on each job.
    pub fn spawn(name: &str, mut work: impl FnMut(J) -> R + Send + 'static) -> Self {
        let link: Link<J, R> = Arc::new((
            Mutex::new(Slot {
                job: None,
                result: None,
                closed: false,
            }),
            Condvar::new(),
        ));
        let theirs = link.clone();
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let (lock, cv) = &*theirs;
                loop {
                    let job = {
                        let mut slot = lock.lock().unwrap();
                        loop {
                            if slot.closed {
                                return;
                            }
                            if let Some(j) = slot.job.take() {
                                break j;
                            }
                            slot = cv.wait(slot).unwrap();
                        }
                    };
                    let r = work(job);
                    lock.lock().unwrap().result = Some(r);
                }
            })
            .expect("spawn worker thread");
        Self { link }
    }

    /// Queue `job`, replacing one that hasn't started yet.
    pub fn submit(&self, job: J) {
        let (lock, cv) = &*self.link;
        lock.lock().unwrap().job = Some(job);
        cv.notify_one();
    }

    /// The newest finished result, if one arrived since the last call.
    pub fn take(&self) -> Option<R> {
        self.link.0.lock().unwrap().result.take()
    }
}

impl<J, R> Drop for Latest<J, R> {
    fn drop(&mut self) {
        let (lock, cv) = &*self.link;
        if let Ok(mut slot) = lock.lock() {
            slot.closed = true;
            slot.job = None;
        }
        cv.notify_one();
    }
}
