//! Fetch task: moves rows from a driver stream into a spool, only as far as
//! the current demand. While demand is satisfied the task waits and the
//! stream is not polled, which pauses the query on the server.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gq_driver::{CancelHandle, DriverError, ErrorKind, RowStream};
use gq_results::{Spool, Status};
use tokio::sync::{mpsc, watch};

use crate::tab::TabShared;
use crate::{Event, Outcome, QueryId, TabId};

/// What to do when the user stops a paused result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopPolicy {
    /// Cancel on the server (safe: autocommit, or inside a savepoint).
    Cancel,
    /// Read and discard the rest (inside a transaction with no savepoints,
    /// where cancelling would abort the transaction).
    Drain,
}

/// Shared control for one result's fetch task.
#[derive(Clone)]
pub(crate) struct FetchCtl {
    pub query: QueryId,
    pub demand: Arc<watch::Sender<u64>>,
    pub discard: Arc<AtomicBool>,
    pub policy: StopPolicy,
    pub spool: Arc<Spool>,
}

impl FetchCtl {
    pub fn is_open(&self) -> bool {
        self.spool.progress().status == Status::Open
    }

    /// Stop fetching: keep what is spooled, end the stream per policy.
    pub async fn stop(&self, cancel: Option<Arc<dyn CancelHandle>>) {
        if !self.is_open() {
            return;
        }
        self.discard.store(true, Ordering::SeqCst);
        self.demand.send_replace(u64::MAX);
        if self.policy == StopPolicy::Cancel {
            if let Some(c) = cancel {
                let _ = c.cancel().await;
            }
        }
    }
}

pub(crate) struct FetchEnd {
    pub complete: bool,
    pub error: Option<DriverError>,
}

pub(crate) struct FetchTask {
    pub tab: TabId,
    pub query: QueryId,
    pub index: usize,
    pub stream: Box<dyn RowStream>,
    pub ctl: FetchCtl,
    pub demand: watch::Receiver<u64>,
    pub events: mpsc::UnboundedSender<Event>,
    pub started: Instant,
    pub shared: Arc<TabShared>,
    /// Fired once the statement has finished executing (first rows, end, or error).
    pub first_done: Option<tokio::sync::oneshot::Sender<()>>,
}

impl FetchTask {
    fn emit_progress(&self, paused: bool) {
        let _ = self.events.send(Event::Fetch {
            tab: self.tab,
            query: self.query,
            progress: self.ctl.spool.progress(),
            paused,
        });
    }

    fn first_done(&mut self, outcome: Outcome) {
        self.shared.set_running(false);
        if let Some(tx) = self.first_done.take() {
            let _ = tx.send(());
        }
        let _ = self.events.send(Event::StatementFinished {
            tab: self.tab,
            query: self.query,
            index: self.index,
            elapsed: self.started.elapsed(),
            outcome,
        });
    }

    pub async fn run(mut self) -> FetchEnd {
        let spool = self.ctl.spool.clone();
        let mut first = true;
        loop {
            // Wait until there is demand (or we are draining/discarding).
            let mut reported_pause = false;
            loop {
                let want = *self.demand.borrow_and_update();
                if self.ctl.discard.load(Ordering::SeqCst) || spool.rows() < want {
                    break;
                }
                if !reported_pause {
                    self.emit_progress(true);
                    reported_pause = true;
                }
                if self.demand.changed().await.is_err() {
                    // The tab went away: stop as if the user did.
                    self.ctl.discard.store(true, Ordering::SeqCst);
                    break;
                }
            }
            let discard = self.ctl.discard.load(Ordering::SeqCst);
            let want = *self.demand.borrow();
            let max = if discard {
                65_536
            } else if first {
                want.min(1000)
            } else {
                want.saturating_sub(spool.rows()).clamp(256, 8192)
            } as usize;
            let deadline = Duration::from_millis(if first { 100 } else { 250 });
            match self.stream.next_batch(max, deadline).await {
                Ok(Some(b)) => {
                    if !discard {
                        if let Err(e) = spool.append(&b) {
                            let msg = format!("could not write result to disk: {e}");
                            spool.finish(Status::Stopped(msg.clone()));
                            if first {
                                self.first_done(Outcome::Failed(DriverError::new(ErrorKind::Query, msg.clone())));
                            }
                            self.emit_progress(false);
                            return FetchEnd { complete: false, error: Some(DriverError::new(ErrorKind::Query, msg)) };
                        }
                    }
                    if first {
                        first = false;
                        self.first_done(Outcome::Rows);
                    }
                    if !discard {
                        self.emit_progress(false);
                    }
                }
                Ok(None) => {
                    if discard {
                        // Drained after a stop: the result is not complete for the user.
                        spool.finish(Status::Stopped("closed".into()));
                    } else {
                        spool.finish(Status::Complete);
                    }
                    if first {
                        self.first_done(Outcome::Rows);
                    }
                    self.emit_progress(false);
                    return FetchEnd { complete: !discard, error: None };
                }
                Err(e) => {
                    let msg = if discard {
                        "closed".to_string()
                    } else if e.kind == ErrorKind::Cancelled {
                        "cancelled".to_string()
                    } else {
                        e.message.clone()
                    };
                    spool.finish(Status::Stopped(msg));
                    if first {
                        let outcome = if discard || e.kind == ErrorKind::Cancelled {
                            Outcome::Cancelled
                        } else {
                            Outcome::Failed(e.clone())
                        };
                        self.first_done(outcome);
                    }
                    self.emit_progress(false);
                    return FetchEnd { complete: false, error: Some(e) };
                }
            }
        }
    }
}
