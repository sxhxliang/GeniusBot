//! The per-agent actor: the original's `SandRunScheduler` queue for one
//! agent plus the interrupt rules its enqueuers apply.
//!
//! * Three FIFO lanes; `user` drains before `agent` before `background`
//!   (`Lane::Automation`). Within the user lane a plain user turn is
//!   preferred over a queued group-member turn (`takeNextUserTask`).
//! * One turn runs at a time per agent. The scheduler itself never cancels
//!   anything: a new user message interrupts the active run the way
//!   `dispatchUserTurn` does (always for a room turn; for a 1:1 run once it
//!   has dispatched, or earlier when both sends are plain text that a later
//!   turn can recover by prepending), and a priority peer message interrupts
//!   non-user work (`steerRecipientForPriorityPeer`).
//! * A queued user turn whose send epoch is older than the newest user send
//!   is skipped as `superseded` when the newer turn can carry its text.
//! * A watchdog trips when a user turn has waited behind the active run for
//!   `RUN_WATCHDOG_MS`: the active run is interrupted and, after
//!   `RUN_WATCHDOG_GRACE_MS`, escaped so the queue keeps moving.
//! * `SendToAgent` never waits on another agent: it only pushes into that
//!   agent's mailbox, so cross-agent sends cannot deadlock.

use crate::agent::{AgentCommand, AgentHandle, RunJob};
use crate::host::HostInner;
use gns_core::{HostEvent, Lane, RunId, RunResult, RunSource};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A user turn waiting behind an active run this long trips the watchdog (`RUN_WATCHDOG_DEFAULT_MS`).
pub const RUN_WATCHDOG_MS: u64 = 120_000;
/// After the trip, the interrupted run gets this long before it is escaped (`RUN_WATCHDOG_GRACE_DEFAULT_MS`).
pub const RUN_WATCHDOG_GRACE_MS: u64 = 30_000;

struct Item {
    job: RunJob,
    reply: oneshot::Sender<RunResult>,
    enqueued_at: Instant,
}

struct Active {
    lane: Lane,
    source: RunSource,
    /// The active job is a plain user send a later turn could recover by prepending.
    recovery_shaped: bool,
    run_id: RunId,
    cancel: CancellationToken,
    started_at: Instant,
    generation: u64,
    interrupted: bool,
    task: JoinHandle<()>,
}

#[derive(Default)]
struct Queue {
    user: VecDeque<Item>,
    agent: VecDeque<Item>,
    background: VecDeque<Item>,
}

impl Queue {
    fn push(&mut self, item: Item) {
        match item.job.options.lane {
            Lane::User => self.user.push_back(item),
            Lane::Agent => self.agent.push_back(item),
            Lane::Automation => self.background.push_back(item),
        }
    }
    /// `takeNextUserTask`: the first user task that is not a group-member turn, else the head.
    fn take_next_user(&mut self) -> Option<Item> {
        if self.user.is_empty() {
            return None;
        }
        let preferred = self.user.iter().position(|i| i.job.options.source != RunSource::Group).unwrap_or(0);
        self.user.remove(preferred)
    }
    fn pop(&mut self) -> Option<Item> {
        self.take_next_user().or_else(|| self.agent.pop_front()).or_else(|| self.background.pop_front())
    }
    fn drain_all(&mut self) -> Vec<Item> {
        self.user.drain(..).chain(self.agent.drain(..)).chain(self.background.drain(..)).collect()
    }
}

pub(crate) fn spawn_actor(host: Arc<HostInner>, handle: Arc<AgentHandle>, rx: mpsc::UnboundedReceiver<AgentCommand>) -> JoinHandle<()> {
    tokio::spawn(actor_loop(host, handle, rx))
}

async fn actor_loop(host: Arc<HostInner>, handle: Arc<AgentHandle>, mut rx: mpsc::UnboundedReceiver<AgentCommand>) {
    let mut queue = Queue::default();
    let mut active: Option<Active> = None;
    let mut zombies: Vec<JoinHandle<()>> = Vec::new();
    let mut generation: u64 = 0;
    // Watchdog: `Some(deadline)` while a user turn waits behind the active run.
    let mut watchdog: Option<Instant> = None;
    let mut grace: Option<(Instant, u64)> = None;

    loop {
        zombies.retain(|z| !z.is_finished());
        // Pump.
        while active.is_none() {
            let Some(item) = queue.pop() else { break };
            if is_superseded(&handle, &item.job) {
                tracing::debug!(agent = %handle.id, epoch = item.job.epoch, "user turn superseded by a newer send");
                let _ = item.reply.send(RunResult { superseded: true, ..Default::default() });
                continue;
            }
            generation += 1;
            let cancel = host.shutdown_token().child_token();
            let run_id = RunId::new();
            let lane = item.job.options.lane;
            let source = item.job.options.source;
            let recovery_shaped = source == RunSource::User && item.job.carries_recovery && item.job.reply_context.is_none();
            handle.dispatched.store(false, Ordering::SeqCst);
            let host2 = host.clone();
            let handle2 = handle.clone();
            let run_id2 = run_id.clone();
            let cancel2 = cancel.clone();
            let job = item.job;
            let reply = item.reply;
            let task = tokio::spawn(async move {
                let result = crate::runner::run_job(host2, handle2, run_id2, job, cancel2).await;
                let _ = reply.send(result);
            });
            active = Some(Active {
                lane,
                source,
                recovery_shaped,
                run_id,
                cancel,
                started_at: Instant::now(),
                generation,
                interrupted: false,
                task,
            });
            tracing::debug!(agent = %handle.id, generation, "turn started");
        }
        arm_watchdog(&queue, active.as_ref(), &mut watchdog, grace.is_some());

        let watchdog_sleep = async {
            match (watchdog, grace) {
                (_, Some((at, _))) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                (Some(at), None) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
                (None, None) => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            cmd = rx.recv() => {
                match cmd {
                    None | Some(AgentCommand::Shutdown) => {
                        if let Some(a) = &active {
                            a.cancel.cancel();
                        }
                        if let Some(a) = active.take() {
                            let _ = a.task.await;
                        }
                        for item in queue.drain_all() {
                            let _ = item.reply.send(RunResult { aborted: true, ..Default::default() });
                        }
                        break;
                    }
                    Some(AgentCommand::Run { job, reply }) => {
                        let job = *job;
                        let options = &job.options;
                        if options.source == RunSource::User
                            && let Some(a) = active.as_mut()
                        {
                            // `dispatchUserTurn`: interrupt a room turn always; a
                            // 1:1 run once dispatched, or before that only when
                            // both sends are recoverable by prepending.
                            let reason = if a.source == RunSource::Group {
                                handle.dm_preempted_group.store(true, Ordering::SeqCst);
                                Some("superseded by a direct user message")
                            } else if handle.dispatched.load(Ordering::SeqCst) || (job.carries_recovery && a.recovery_shaped) {
                                handle.dm_preempted_wake.store(true, Ordering::SeqCst);
                                Some("superseded by a new user message")
                            } else {
                                None
                            };
                            if let Some(reason) = reason {
                                a.interrupted = true;
                                a.cancel.cancel();
                                host.emit(HostEvent::Interrupted { agent_id: handle.id.clone(), run_id: a.run_id.clone(), reason: reason.to_owned() });
                            }
                        }
                        queue.push(Item { job, reply, enqueued_at: Instant::now() });
                    }
                    Some(AgentCommand::Interrupt { reason, only_if_lane_below }) => {
                        if let Some(a) = active.as_mut()
                            && a.lane < only_if_lane_below
                        {
                            if a.source == RunSource::Group {
                                handle.dm_preempted_group.store(true, Ordering::SeqCst);
                            } else {
                                handle.dm_preempted_wake.store(true, Ordering::SeqCst);
                            }
                            a.interrupted = true;
                            a.cancel.cancel();
                            host.emit(HostEvent::Interrupted { agent_id: handle.id.clone(), run_id: a.run_id.clone(), reason });
                        }
                    }
                }
            }
            _ = wait_active(&mut active) => {
                active = None;
                watchdog = None;
                grace = None;
                handle.set_active_lane(None);
            }
            _ = watchdog_sleep => {
                match grace.take() {
                    Some((_, generation_at_trip)) => {
                        // Escape: the interrupted run did not unwind in time; let
                        // it finish detached and move the queue on.
                        if let Some(a) = active.take_if(|a| a.generation == generation_at_trip) {
                            tracing::warn!(agent = %handle.id, run = %a.run_id, "run-queue watchdog: escaping a wedged run");
                            zombies.push(a.task);
                            handle.set_active_lane(None);
                        }
                        watchdog = None;
                    }
                    None => {
                        watchdog = None;
                        if let Some(a) = active.as_mut() {
                            let waited = queue.user.front().map(|h| Instant::now().duration_since(h.enqueued_at.max(a.started_at)));
                            if waited.is_some_and(|w| w >= Duration::from_millis(RUN_WATCHDOG_MS)) {
                                tracing::warn!(agent = %handle.id, run = %a.run_id, "run-queue watchdog: releasing a wedged predecessor");
                                a.interrupted = true;
                                a.cancel.cancel();
                                host.emit(HostEvent::Interrupted {
                                    agent_id: handle.id.clone(),
                                    run_id: a.run_id.clone(),
                                    reason: "run-queue watchdog: releasing a wedged predecessor".to_owned(),
                                });
                                grace = Some((Instant::now() + Duration::from_millis(RUN_WATCHDOG_GRACE_MS), a.generation));
                            }
                        }
                    }
                }
            }
        }
    }
    for z in zombies {
        let _ = z.await;
    }
    tracing::debug!(agent = %handle.id, "actor stopped");
}

/// `runTurn`: a queued user turn older than the newest send is skipped when
/// the newer turn can recover it by prepending (plain text, no reply
/// context, newer than the last recovery break, and the newest send is a
/// recovery-carrying one).
fn is_superseded(handle: &AgentHandle, job: &RunJob) -> bool {
    if job.options.source != RunSource::User || job.message_id.is_none() {
        return false;
    }
    let current = handle.current_turn_epoch();
    if job.epoch == current {
        return false;
    }
    job.images.is_empty()
        && job.reply_context.is_none()
        && job.carries_recovery
        && job.epoch > handle.recovery_break_epoch.load(Ordering::SeqCst)
        && handle.latest_recovery_epoch.load(Ordering::SeqCst) == current
}

fn arm_watchdog(queue: &Queue, active: Option<&Active>, watchdog: &mut Option<Instant>, in_grace: bool) {
    if in_grace {
        return;
    }
    match (queue.user.front(), active) {
        (Some(head), Some(a)) if watchdog.is_none() => {
            let base = head.enqueued_at.max(a.started_at);
            *watchdog = Some(base + Duration::from_millis(RUN_WATCHDOG_MS));
        }
        (None, _) | (_, None) => *watchdog = None,
        _ => {}
    }
}

async fn wait_active(active: &mut Option<Active>) {
    match active.as_mut() {
        Some(a) => {
            let _ = (&mut a.task).await;
        }
        None => std::future::pending::<()>().await,
    }
}
