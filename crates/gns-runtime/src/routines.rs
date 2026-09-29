//! Routine service, the background scheduler and the event-fire pipeline.
//!
//! * [`RoutineServiceImpl`] is the [`RoutineService`] over each agent's
//!   `automations/` folder (see `gns_store::AutomationStore`) plus the
//!   next-run derivation, run bookkeeping and event matching.
//! * [`spawn_scheduler`] ticks every `scheduler_tick`, fires due schedules
//!   through [`fire_routine`] and runs memory dreams.
//! * [`fire_event`] is the local stand-in for the original's backend event
//!   delivery (`automation-event-fires.ts`): matching routines are woken
//!   after a 750 ms debounce, at most 25 events per wake, the rest of the
//!   batch's fire ids recorded as `coalescedRunIds`.

use crate::agent::RunJob;
use crate::host::HostInner;
use crate::schedule::{ZoneChoice, earliest_next_run_at, format_in_zone, is_valid_schedule};
use gns_core::routine::*;
use gns_core::text::now_ms;
use gns_core::*;
use gns_store::{AgentDirLayout, AutomationStore};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Detail recorded when a run is cancelled by a higher-lane turn.
pub const INTERRUPTED_RUN_DETAIL: &str = "Interrupted before it finished.";

type RunKey = (AgentId, String);

/// Pending event fires for one routine (`FireBatch` in the original).
#[derive(Debug)]
struct EventBatch {
    record: AutomationRecord,
    /// `(event, fire id)` in arrival order.
    items: Vec<(EventRecord, String)>,
    flushing: bool,
    timer_armed: bool,
    flush_immediately: bool,
}

/// [`RoutineService`] implementation over `automation.json` / `runs.json` files.
#[derive(Debug)]
pub struct RoutineServiceImpl {
    layout: AgentDirLayout,
    time_zone: Option<String>,
    fallback_zone: ZoneChoice,
    running: Mutex<HashSet<RunKey>>,
    batches: Mutex<HashMap<RunKey, EventBatch>>,
}

impl RoutineServiceImpl {
    /// `time_zone` is the user's IANA zone; schedules without a pinned zone
    /// follow it, else the machine's local time.
    pub fn new(layout: AgentDirLayout, time_zone: Option<&str>) -> Self {
        Self {
            layout,
            time_zone: time_zone.map(str::trim).filter(|z| !z.is_empty()).map(str::to_owned),
            fallback_zone: ZoneChoice::Local,
            running: Mutex::new(HashSet::new()),
            batches: Mutex::new(HashMap::new()),
        }
    }

    /// Test hook: the zone used when neither the schedule nor the host pins
    /// one (`ZoneChoice::Utc` makes next-run times deterministic).
    pub fn with_fallback_zone(mut self, zone: ZoneChoice) -> Self {
        self.fallback_zone = zone;
        self
    }

    /// The user's zone, when configured.
    pub fn time_zone(&self) -> Option<&str> {
        self.time_zone.as_deref()
    }

    fn store(&self, agent: &AgentId) -> AutomationStore {
        AutomationStore::new(self.layout.automations_dir(agent))
    }

    fn not_found(id: &str) -> HostError {
        HostError::invalid(format!("no routine with folder {id}"))
    }

    /// Normalise and validate a spec (`clampAutomationName`,
    /// `normalizeAutomationPrompt`, `normalizeSpecTrigger`) and additionally
    /// reject cron schedules the scheduler could never fire.
    fn normalize(&self, spec: &AutomationSpec) -> Result<(String, String, Trigger), HostError> {
        let name = clamp_automation_name(&spec.name);
        if name.is_empty() {
            return Err(HostError::invalid("routine name is required"));
        }
        let prompt = normalize_automation_prompt(&spec.prompt);
        if prompt.is_empty() {
            return Err(HostError::invalid("routine prompt is required"));
        }
        let trigger = normalize_spec_trigger(&spec.trigger).ok_or_else(|| HostError::invalid("the routine trigger is invalid"))?;
        for schedule in trigger.cron_schedules() {
            if !is_valid_schedule(schedule) {
                return Err(HostError::invalid(format!(
                    "invalid schedule \"{schedule}\": expected 5 cron fields (minute hour day-of-month month day-of-week), a @hourly-style alias or \"@every N(s|m|h|d)\", optionally prefixed with CRON_TZ=<IANA zone>"
                )));
            }
        }
        Ok((name, prompt, trigger))
    }

    /// `earliestNextRunAt`: next fire time (None when paused, event-only or unparsable).
    pub fn next_run_at(&self, record: &AutomationRecord) -> Option<i64> {
        if !record.is_enabled {
            return None;
        }
        // Slots strictly after the last run (or creation). A slot missed
        // while the host was down fires once, late; it is never replayed
        // because `begin_run` moves `last_run_at` to now before the run.
        let anchor = automation_anchor(record.created_at, record.last_run_at);
        earliest_next_run_at(&record.trigger, anchor, self.time_zone.as_deref(), &self.fallback_zone)
    }

    fn with_next(&self, mut record: AutomationRecord) -> AutomationRecord {
        record.next_run_at = self.next_run_at(&record);
        record
    }

    /// `formatTimestamp` in the user's zone.
    pub fn format_time(&self, ms: i64) -> String {
        format_in_zone(ms, self.time_zone.as_deref(), &self.fallback_zone)
    }

    /// One routine with its next run time.
    pub fn get(&self, agent: &AgentId, id: &str) -> Result<Option<AutomationRecord>, HostError> {
        Ok(self.store(agent).get(id)?.map(|r| self.with_next(r)))
    }

    /// `listDefinitions`: every routine, oldest first, next run filled in.
    pub fn list_definitions(&self, agent: &AgentId) -> Result<Vec<AutomationRecord>, HostError> {
        Ok(self.store(agent).list()?.into_iter().map(|r| self.with_next(r)).collect())
    }

    fn is_running(&self, key: &RunKey) -> bool {
        self.running.lock().map(|r| r.contains(key)).unwrap_or(true)
    }

    /// Routines due at `now` across `agents` (in-flight ones are skipped).
    pub fn due(&self, agents: &[AgentId], now: i64) -> Vec<(AgentId, AutomationRecord)> {
        let mut out = Vec::new();
        for agent in agents {
            let Ok(records) = self.list_definitions(agent) else { continue };
            for record in records {
                if self.is_running(&(agent.clone(), record.id.clone())) {
                    continue;
                }
                if record.next_run_at.is_some_and(|t| t <= now) {
                    out.push((agent.clone(), record));
                }
            }
        }
        out
    }

    /// Enabled routines (across `agents`) with a listener matching `event`.
    pub fn matching_event_routines(&self, agents: &[AgentId], event: &EventRecord) -> Vec<(AgentId, AutomationRecord)> {
        let mut out = Vec::new();
        for agent in agents {
            let Ok(records) = self.store(agent).list() else { continue };
            for record in records {
                if record.is_enabled && trigger_matches_event(&record.trigger, event, EventMatchOptions::default()) {
                    out.push((agent.clone(), record));
                }
            }
        }
        out
    }

    /// Adapter for the older `fire_webhook(name, payload)` API: the payload
    /// object (or `{"payload": <value>}`) becomes the event and `name` its
    /// `source` unless the payload already carries one. Only the listener
    /// sources (`slack`, `github`, `microsoftTeams`, `linear`, `sentry`,
    /// `pagerduty`) can match a trigger.
    pub fn webhook_to_event(name: &str, payload: serde_json::Value) -> EventRecord {
        let mut event = match payload {
            serde_json::Value::Object(map) => map,
            other => EventRecord::from_iter([("payload".to_owned(), other)]),
        };
        event.entry("source".to_owned()).or_insert_with(|| serde_json::Value::String(name.trim().to_owned()));
        event
    }

    /// Mark a routine as running, move `lastRunAt` to now, and insert the
    /// `running` entry into `runs.json`. Returns the run id. A routine that
    /// is already in flight is refused (duplicate schedule/manual fires are dropped).
    pub fn begin_run(&self, agent: &AgentId, record: &AutomationRecord, trigger: &str) -> Result<String, HostError> {
        self.begin_run_with(agent, &record.id, AutomationRunTrigger::parse(trigger), None, None, None)
    }

    /// [`RoutineServiceImpl::begin_run`] with the event summary and fire ids of an event wake.
    pub fn begin_run_with(
        &self,
        agent: &AgentId,
        routine_id: &str,
        trigger: AutomationRunTrigger,
        event_summary: Option<&str>,
        run_id: Option<String>,
        coalesced_run_ids: Option<&[String]>,
    ) -> Result<String, HostError> {
        let key = (agent.clone(), routine_id.to_owned());
        {
            let mut running = self.running.lock().map_err(|_| HostError::Other("routine registry poisoned".into()))?;
            if !running.insert(key.clone()) {
                return Err(HostError::invalid(format!("routine {routine_id} is already running")));
            }
        }
        let store = self.store(agent);
        let now = now_ms();
        let started = store
            .record_run(routine_id, now)
            .and_then(|r| r.ok_or_else(|| Self::not_found(routine_id)))
            .and_then(|_| store.begin_run(routine_id, trigger, now, event_summary, run_id, coalesced_run_ids))
            .and_then(|r| r.ok_or_else(|| Self::not_found(routine_id)));
        match started {
            Ok(run) => Ok(run.id),
            Err(e) => {
                self.release(&key);
                Err(e)
            }
        }
    }

    fn release(&self, key: &RunKey) {
        if let Ok(mut r) = self.running.lock() {
            r.remove(key);
        }
    }

    /// Record the run outcome and release the routine.
    pub fn finish_run(&self, agent: &AgentId, routine_id: &str, run_id: &str, ok: bool, detail: Option<String>) {
        let status = if ok { AutomationRunStatus::Ok } else { AutomationRunStatus::Error };
        if let Err(e) = self.store(agent).finish_run(routine_id, run_id, status, now_ms(), detail.as_deref()) {
            tracing::warn!(agent = %agent, routine = routine_id, error = %e, "could not record routine run outcome");
        }
        self.release(&(agent.clone(), routine_id.to_owned()));
    }

    /// `markNoticeRaised` for every notice the wake prompt is about to carry.
    pub fn mark_notices_raised(&self, agent: &AgentId, record: &AutomationRecord) {
        let store = self.store(agent);
        for notice in record.notices_to_raise() {
            if let Err(e) = store.mark_notice_raised(&record.id, notice.id) {
                tracing::warn!(agent = %agent, routine = %record.id, notice = notice.id, error = %e, "could not mark routine notice");
            }
        }
    }

    /// The routines section of the system prompt (definitions by creation
    /// time, at most `AUTOMATION_UI_LIMIT`).
    pub fn render_section(&self, agent: &AgentId, time_zone: Option<&str>) -> String {
        let mut records = self.list_definitions(agent).unwrap_or_default();
        records.truncate(AUTOMATION_UI_LIMIT);
        render_automations_system_prompt(&records, &self.layout.automations_dir(agent).display().to_string(), time_zone)
    }

    /// The `<automation_status>` reminder for a turn (`None` without routines).
    /// `firing_automation_id` hides that routine's own in-flight run.
    pub fn status_reminder(&self, agent: &AgentId, firing_automation_id: Option<&str>) -> Option<String> {
        let mut records = self.list_definitions(agent).ok()?;
        records.truncate(AUTOMATION_UI_LIMIT);
        render_automation_status_reminder(&records, self.time_zone.as_deref(), firing_automation_id)
    }

    fn take_batch_items(&self, key: &RunKey) -> Option<(AutomationRecord, Vec<(EventRecord, String)>)> {
        let mut batches = self.batches.lock().ok()?;
        let batch = batches.get_mut(key)?;
        batch.timer_armed = false;
        if batch.items.is_empty() || batch.flushing {
            return None;
        }
        batch.flushing = true;
        let take = batch.items.len().min(MAX_EVENTS_IN_AUTOMATION_WAKE);
        let items: Vec<(EventRecord, String)> = batch.items.drain(..take).collect();
        Some((batch.record.clone(), items))
    }

    /// Returns whether more items are waiting (and should flush immediately).
    fn finish_batch(&self, key: &RunKey) -> bool {
        let Ok(mut batches) = self.batches.lock() else { return false };
        let Some(batch) = batches.get_mut(key) else { return false };
        batch.flushing = false;
        if batch.items.is_empty() {
            batches.remove(key);
            false
        } else {
            batch.flush_immediately = true;
            true
        }
    }
}

impl RoutineService for RoutineServiceImpl {
    fn list(&self, agent: &AgentId) -> Result<Vec<AutomationRecord>, HostError> {
        // `list()`: soonest next run first, then oldest first.
        let mut records = self.list_definitions(agent)?;
        records.sort_by_key(|r| (r.next_run_at.unwrap_or(i64::MAX), r.created_at));
        Ok(records)
    }

    fn get(&self, agent: &AgentId, id: &str) -> Result<Option<AutomationRecord>, HostError> {
        RoutineServiceImpl::get(self, agent, id)
    }

    fn create(&self, agent: &AgentId, spec: AutomationSpec) -> Result<AutomationRecord, HostError> {
        let (name, prompt, trigger) = self.normalize(&spec)?;
        let config = AutomationConfig {
            name,
            prompt,
            trigger,
            is_enabled: spec.is_enabled.unwrap_or(true),
            created_at: now_ms(),
            last_run_at: None,
            raised_notices: Vec::new(),
        };
        Ok(self.with_next(self.store(agent).create(config)?))
    }

    fn update(&self, agent: &AgentId, id: &str, spec: AutomationSpec) -> Result<AutomationRecord, HostError> {
        let (name, prompt, trigger) = self.normalize(&spec)?;
        let store = self.store(agent);
        let existing = store.get(id)?.ok_or_else(|| Self::not_found(id))?;
        let config = AutomationConfig {
            name,
            prompt,
            trigger,
            is_enabled: spec.is_enabled.unwrap_or(existing.is_enabled),
            created_at: existing.created_at,
            last_run_at: existing.last_run_at,
            raised_notices: existing.raised_notices,
        };
        store.save(id, &config)?;
        Ok(self.with_next(store.to_record(id, config)))
    }

    fn set_enabled(&self, agent: &AgentId, id: &str, enabled: bool) -> Result<AutomationRecord, HostError> {
        let record = self.store(agent).set_enabled(id, enabled)?.ok_or_else(|| Self::not_found(id))?;
        Ok(self.with_next(record))
    }

    fn delete(&self, agent: &AgentId, id: &str) -> Result<(), HostError> {
        if !self.store(agent).delete(id)? {
            return Err(Self::not_found(id));
        }
        Ok(())
    }
}

/// Run one routine wake to completion: `beginRun` (with `lastRunAt` moved
/// first), the `RoutineFired` event, owed notices marked, the hidden
/// `[routine]` turn on the automation lane, then `finishRun` with
/// `Interrupted before it finished.` on abort or the turn's error text.
pub(crate) async fn fire_routine(
    host: &Arc<HostInner>,
    agent_id: &AgentId,
    record: AutomationRecord,
    trigger: AutomationRunTrigger,
    events: Vec<EventRecord>,
    fire_ids: Vec<String>,
) -> Result<RunResult, HostError> {
    let handle = host.agent(agent_id).ok_or_else(|| HostError::AgentNotFound(agent_id.to_string()))?;
    // Re-read so a definition edited since the fire was queued is what runs.
    let current = host.routines.get(agent_id, &record.id).ok().flatten().unwrap_or(record);
    let summary = if events.is_empty() { None } else { Some(describe_trigger_event_batch(&events)) };
    let (run_id, coalesced): (Option<String>, Option<Vec<String>>) = match fire_ids.split_first() {
        Some((first, rest)) => (Some(first.clone()), if rest.is_empty() { None } else { Some(rest.to_vec()) }),
        None => (None, None),
    };
    let run_id = host.routines.begin_run_with(agent_id, &current.id, trigger, summary.as_deref(), run_id, coalesced.as_deref())?;
    host.emit(HostEvent::RoutineFired { agent_id: agent_id.clone(), automation_id: current.id.clone(), name: current.name.clone() });
    host.routines.mark_notices_raised(agent_id, &current);
    let wake = AutomationWake { trigger, events, time_zone: host.routines.time_zone().map(str::to_owned), fired_at_ms: now_ms() };
    let prompt = build_automation_wake_prompt(&current, &wake);
    let result = handle.run(RunJob::hidden(prompt, RunOptions::automation())).await;
    let (ok, detail) = match &result {
        Ok(r) if r.error.is_some() => (false, r.error.clone()),
        Ok(r) if r.aborted => (false, Some(INTERRUPTED_RUN_DETAIL.to_owned())),
        Ok(_) => (true, None),
        Err(e) => (false, Some(e.to_string())),
    };
    host.routines.finish_run(agent_id, &current.id, &run_id, ok, detail);
    result
}

/// Deliver an outside event: every enabled routine (of any agent) with a
/// matching listener is queued for a debounced event wake. Returns how many
/// routines matched.
pub(crate) fn fire_event(host: &Arc<HostInner>, event: EventRecord) -> usize {
    let agents = host.agent_ids();
    let matches = host.routines.matching_event_routines(&agents, &event);
    for (agent_id, record) in &matches {
        enqueue_event_fire(host, agent_id.clone(), record.clone(), event.clone());
    }
    matches.len()
}

/// `enqueueEventAutomationFire`: append to the routine's batch, shed overflow, arm the flush.
fn enqueue_event_fire(host: &Arc<HostInner>, agent_id: AgentId, record: AutomationRecord, event: EventRecord) {
    let key: RunKey = (agent_id.clone(), record.id.clone());
    let fire_id = RunId::new().0;
    {
        let Ok(mut batches) = host.routines.batches.lock() else { return };
        let batch = batches.entry(key.clone()).or_insert_with(|| EventBatch {
            record: record.clone(),
            items: Vec::new(),
            flushing: false,
            timer_armed: false,
            flush_immediately: false,
        });
        batch.record = record;
        batch.items.push((event, fire_id));
        let excess = batch.items.len().saturating_sub(MAX_QUEUED_EVENT_FIRES_PER_AUTOMATION);
        if excess > 0 {
            let dropped: Vec<String> = batch.items.drain(..excess).map(|(_, id)| id).collect();
            tracing::warn!(agent = %agent_id, routine = %key.1, dropped = dropped.len(), "event batch overflow; oldest fires dropped");
        }
    }
    schedule_batch_flush(host, key);
}

/// `scheduleEventBatchFlush`: one timer per batch; 750 ms, or immediately
/// when a previous flush left items behind.
fn schedule_batch_flush(host: &Arc<HostInner>, key: RunKey) {
    let wait = {
        let Ok(mut batches) = host.routines.batches.lock() else { return };
        let Some(batch) = batches.get_mut(&key) else { return };
        if batch.items.is_empty() || batch.flushing || batch.timer_armed {
            return;
        }
        batch.timer_armed = true;
        let wait = if batch.flush_immediately { 0 } else { EVENT_FIRE_DEBOUNCE_MS };
        batch.flush_immediately = false;
        wait
    };
    let host = host.clone();
    tokio::spawn(async move {
        if wait > 0 {
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        flush_event_batch(&host, key).await;
    });
}

/// `flushEventBatch`: wake the routine with up to 25 queued events.
async fn flush_event_batch(host: &Arc<HostInner>, key: RunKey) {
    let Some((record, items)) = host.routines.take_batch_items(&key) else { return };
    let (events, fire_ids): (Vec<EventRecord>, Vec<String>) = items.into_iter().unzip();
    let name = record.name.clone();
    if let Err(e) = fire_routine(host, &key.0, record, AutomationRunTrigger::Event, events, fire_ids).await {
        host.emit(HostEvent::Error {
            agent_id: Some(key.0.clone()),
            run_id: None,
            message: format!("event wake dispatch failed for \"{name}\" ({}): {e}", key.1),
        });
    }
    if host.routines.finish_batch(&key) {
        schedule_batch_flush(host, key);
    }
}

/// Background loop that fires due routines (and idle memory dreams).
pub(crate) fn spawn_scheduler(host: Arc<HostInner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let tick = host.config.scheduler_tick;
        let shutdown = host.shutdown_token();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = tokio::time::sleep(tick) => {}
            }
            let agents: Vec<AgentId> = host.agent_ids();
            if let Some(idle) = host.config.dream_after_idle {
                for agent_id in &agents {
                    if host.should_dream(agent_id, idle) {
                        let host2 = host.clone();
                        let id = agent_id.clone();
                        tokio::spawn(async move {
                            let _ = host2.dream(&id).await;
                        });
                    }
                }
            }
            for (agent_id, record) in host.routines.due(&agents, now_ms()) {
                let host2 = host.clone();
                tokio::spawn(async move {
                    let routine_id = record.id.clone();
                    if let Err(e) = fire_routine(&host2, &agent_id, record, AutomationRunTrigger::Schedule, Vec::new(), Vec::new()).await {
                        host2.emit(HostEvent::Error {
                            agent_id: Some(agent_id.clone()),
                            run_id: None,
                            message: format!("routine {routine_id}: {e}"),
                        });
                    }
                });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn service(dir: &std::path::Path) -> RoutineServiceImpl {
        RoutineServiceImpl::new(AgentDirLayout::new(dir), None).with_fallback_zone(ZoneChoice::Utc)
    }

    fn record(schedule: &str, last_run_at: Option<i64>, created_at: i64) -> AutomationRecord {
        let config = AutomationConfig {
            name: "r".into(),
            prompt: "p".into(),
            trigger: Trigger::cron(schedule),
            is_enabled: true,
            created_at,
            last_run_at,
            raised_notices: vec![],
        };
        AutomationRecord::from_config("r", config, None, vec![], "")
    }

    #[test]
    fn missed_slot_fires_once_late_and_long_intervals_are_due() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let now = now_ms();
        // Last ran two days ago on an hourly schedule: the missed slot is due
        // now (it fires once, late); `begin_run` then moves the anchor forward.
        let stale = record("@every 1h", Some(now - 2 * 86_400_000), now - 3 * 86_400_000);
        let next = service.next_run_at(&stale).unwrap();
        assert!(next <= now, "next={next} now={now}");
        // A routine that ran a minute ago is simply scheduled an hour later.
        let fresh = record("@every 1h", Some(now - 60_000), now - 86_400_000);
        assert_eq!(service.next_run_at(&fresh).unwrap(), now - 60_000 + 3_600_000);
        // Regression: an interval longer than five minutes must become due.
        let long = record("@every 30m", Some(now - 31 * 60_000), now - 86_400_000);
        assert!(service.next_run_at(&long).unwrap() <= now);
        // Paused routines never fire; listener-only routines have no next run.
        let mut paused = fresh.clone();
        paused.is_enabled = false;
        assert!(service.next_run_at(&paused).is_none());
        let listener = parse_stored_trigger(&json!({"type":"slack","channel":"#eng","match":{"kind":"mention"}})).unwrap();
        let mut event_only = fresh.clone();
        event_only.trigger = listener;
        assert!(service.next_run_at(&event_only).is_none());
    }

    #[test]
    fn service_crud_runs_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let agent = AgentId::new();
        let spec = |name: &str, trigger: Trigger| AutomationSpec {
            name: name.into(),
            prompt: " Do the thing. ".into(),
            trigger,
            is_enabled: None,
        };
        assert!(service.create(&agent, spec("x", Trigger::cron("0 9 * *"))).is_err(), "unparsable schedule rejected");
        assert!(service.create(&agent, spec("  ", Trigger::cron("@hourly"))).is_err(), "empty name rejected");
        let created = service.create(&agent, spec("  Morning   digest ", Trigger::cron(" 0 9 * * 1-5 "))).unwrap();
        assert_eq!(
            (created.id.as_str(), created.name.as_str(), created.prompt.as_str()),
            ("morning-digest", "Morning digest", "Do the thing.")
        );
        assert_eq!(created.schedule.as_deref(), Some("0 9 * * 1-5"));
        assert_eq!(describe_trigger_for_reply(&created), "Weekdays at 9:00 AM");
        assert!(created.next_run_at.is_some());
        // Pause only flips the flag; resume does not touch lastRunAt.
        let paused = service.set_enabled(&agent, "morning-digest", false).unwrap();
        assert!(!paused.is_enabled && paused.next_run_at.is_none() && paused.last_run_at.is_none());
        let resumed = service.set_enabled(&agent, "morning-digest", true).unwrap();
        assert!(resumed.is_enabled && resumed.last_run_at.is_none());
        // Runs: lastRunAt moves before the run; duplicates in flight are refused.
        let run_id = service.begin_run(&agent, &resumed, "manual").unwrap();
        let mid = service.get(&agent, "morning-digest").unwrap().unwrap();
        assert!(mid.last_run_at.is_some());
        assert_eq!(mid.runs[0].status, AutomationRunStatus::Running);
        assert_eq!(mid.runs[0].trigger, "manual");
        assert!(service.begin_run(&agent, &resumed, "schedule").is_err());
        assert!(service.due(std::slice::from_ref(&agent), i64::MAX).is_empty(), "in-flight routines are not due");
        service.finish_run(&agent, "morning-digest", &run_id, false, Some(INTERRUPTED_RUN_DETAIL.to_owned()));
        let done = service.get(&agent, "morning-digest").unwrap().unwrap();
        assert_eq!((done.runs[0].status, done.runs[0].detail.as_deref()), (AutomationRunStatus::Error, Some(INTERRUPTED_RUN_DETAIL)));
        assert_eq!(service.due(std::slice::from_ref(&agent), i64::MAX).len(), 1);
        // Update keeps createdAt / lastRunAt / raisedNotices and the flag when unspecified.
        let listener = parse_stored_trigger(&json!({"type":"github","repo":"o/n","events":["pr-opened"]})).unwrap();
        service.set_enabled(&agent, "morning-digest", false).unwrap();
        let updated = service.update(&agent, "morning-digest", spec("Renamed", listener.clone())).unwrap();
        assert_eq!(
            (updated.name.as_str(), updated.is_enabled, updated.created_at, updated.last_run_at),
            ("Renamed", false, done.created_at, done.last_run_at)
        );
        assert_eq!(updated.schedule, None);
        assert_eq!(updated.trigger_description, "When a PR opens in o/n");
        assert_eq!(updated.runs.len(), 1, "history kept");
        // Event matching and the webhook adapter.
        service.set_enabled(&agent, "morning-digest", true).unwrap();
        let event = RoutineServiceImpl::webhook_to_event("github", json!({"repo":"o/n","kind":"pr-opened","actor":"ann","title":"t"}));
        assert_eq!(service.matching_event_routines(std::slice::from_ref(&agent), &event).len(), 1);
        let other = RoutineServiceImpl::webhook_to_event("github", json!({"repo":"o/n","kind":"pr-merged"}));
        assert!(service.matching_event_routines(std::slice::from_ref(&agent), &other).is_empty());
        let unknown = RoutineServiceImpl::webhook_to_event("gitlab", json!({"repo":"o/n","kind":"pr-opened"}));
        assert!(service.matching_event_routines(std::slice::from_ref(&agent), &unknown).is_empty());
        // Prompt section and status reminder.
        let section = service.render_section(&agent, Some("UTC"));
        assert!(section.ends_with("Current routines:\n- Renamed [enabled] — When a PR opens in o/n; folder morning-digest"), "{section}");
        let status = service.status_reminder(&agent, None).unwrap();
        assert!(
            status.starts_with("<system_reminder>\n<automation_status>\n")
                && status.contains("- Renamed (folder morning-digest): last run "),
            "{status}"
        );
        assert!(service.status_reminder(&AgentId::new(), None).is_none());
        service.delete(&agent, "morning-digest").unwrap();
        assert!(service.delete(&agent, "morning-digest").is_err());
    }
}
