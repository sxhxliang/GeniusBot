//! Artifact authorization, durable tasks and their result outbox.
use crate::host::HostInner;
use crate::{AgentHost, RunJob};
use gns_core::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub(crate) struct CoordinationRun {
    pub agent: AgentId,
    pub origin: ConversationRef,
    pub message: Option<EntryId>,
    pub task: Option<String>,
    pub attempt: u64,
}

#[derive(Debug, Default)]
pub(crate) struct CoordinationRuntime {
    pub runs: Mutex<HashMap<RunId, CoordinationRun>>,
    scheduled: Mutex<HashSet<(String, u64)>>,
    delivery_lock: tokio::sync::Mutex<()>,
}

impl HostInner {
    pub(crate) fn artifact_principals(&self, agent: &AgentId) -> Vec<String> {
        let mut keys = vec![ConversationRef::Agent(agent.clone()).grant_key()];
        keys.extend(
            self.all_groups()
                .iter()
                .filter(|g| g.config().member_ids.contains(agent))
                .map(|g| ConversationRef::Group(g.id.clone()).grant_key()),
        );
        keys
    }
    pub(crate) fn check_artifact(&self, agent: &AgentId, id: &str) -> Result<ArtifactRef, HostError> {
        if self.agent(agent).is_none() || !self.coordination.authorized(id, &self.artifact_principals(agent))? {
            return Err(HostError::invalid("artifact access denied"));
        }
        self.coordination.artifact(id)
    }
    pub(crate) async fn prepare_files(&self, agent: &AgentId, files: Vec<String>, ids: Vec<String>) -> Result<Vec<ArtifactRef>, HostError> {
        if files.len() + ids.len() > self.config.artifact_max_files {
            return Err(HostError::invalid("too many attached files"));
        }
        let handle = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        let mut refs: Vec<ArtifactRef> = ids.iter().map(|id| self.check_artifact(agent, id)).collect::<Result<_, _>>()?;
        // Validate every source before creating any snapshots.
        let paths = files
            .iter()
            .map(|path| {
                gns_tools::sandbox_path(path, &handle.workspace_dir, &[&handle.workspace_dir, &handle.data_dir])
                    .map_err(|e| HostError::invalid(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let store = self.coordination.clone();
        let owner = agent.clone();
        let limit = self.config.artifact_max_bytes;
        let new =
            tokio::task::spawn_blocking(move || paths.iter().map(|p| store.snapshot(&owner, p, limit)).collect::<Result<Vec<_>, _>>())
                .await
                .map_err(HostError::storage)??;
        refs.extend(new);
        let mut seen = HashSet::new();
        refs.retain(|a| seen.insert(a.id.clone()));
        Ok(refs)
    }
    pub(crate) async fn fetch_file(&self, agent: &AgentId, id: &str) -> Result<String, HostError> {
        self.check_artifact(agent, id)?;
        let workspace = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?.workspace_dir.clone();
        let store = self.coordination.clone();
        let id = id.to_owned();
        let path = tokio::task::spawn_blocking(move || store.fetch(&id, &workspace)).await.map_err(HostError::storage)??;
        Ok(path.display().to_string())
    }
    pub(crate) fn task_for_run(&self, run: &RunId) -> Option<TaskRecord> {
        let id = self.coordination_runtime.runs.lock().ok()?.get(run)?.task.clone()?;
        self.coordination.task(&id).ok()
    }
    fn executor_task(&self, agent: &AgentId, run: &RunId, id: &str) -> Result<TaskRecord, HostError> {
        let task = self.coordination.task(id)?;
        task.check_executor(agent)?;
        let runs = self.coordination_runtime.runs.lock().map_err(HostError::storage)?;
        let context = runs.get(run).ok_or_else(|| HostError::invalid("task operation requires an active task run"))?;
        if &context.agent != agent || context.task.as_deref() != Some(id) || context.attempt != task.attempt {
            return Err(HostError::invalid("stale or unrelated task run"));
        }
        Ok(task)
    }
    pub(crate) fn task_access(&self, agent: &AgentId, id: &str) -> Result<TaskRecord, HostError> {
        let t = self.coordination.task(id)?;
        let member = match &t.origin {
            ConversationRef::Group(g) => self.group(g).is_some_and(|g| g.config().member_ids.contains(agent)),
            _ => false,
        };
        if &t.requester != agent && &t.executor != agent && !member {
            return Err(HostError::invalid("task access denied"));
        }
        Ok(t)
    }
    pub(crate) fn task_deliveries(&self, task: &TaskRecord, wake: bool) -> Vec<TaskDelivery> {
        vec![task.origin.clone()]
            .into_iter()
            .enumerate()
            .map(|(index, target)| TaskDelivery {
                id: format!("task:{}:{}:{}", task.id, task.revision, target.grant_key()),
                target,
                task: task.clone(),
                wake_agent: (wake && index == 0).then(|| task.requester.clone()),
                projected: false,
                wake_started: false,
                wake_attempts: 0,
                retry_after: 0,
                done: false,
                error: None,
            })
            .collect()
    }
    fn commit_task_update(&self, mut task: TaskRecord, status: TaskStatus, summary: String, wake: bool) -> Result<TaskRecord, HostError> {
        let rev = task.revision;
        task.revision += 1;
        task.updated_at = gns_core::text::now_ms();
        task.status = status;
        if status != TaskStatus::Blocked {
            task.background_resume_allowed = false;
        }
        task.summary = summary;
        self.coordination.commit_task(Some(rev), &task, &self.task_deliveries(&task, wake), None)?;
        self.emit(HostEvent::TaskUpdated { task: Box::new(task.clone()) });
        Ok(task)
    }
    pub(crate) async fn create_task(&self, from: &AgentId, run: &RunId, request: DelegateTaskRequest) -> Result<TaskRecord, HostError> {
        if request.task.trim().is_empty() {
            return Err(HostError::invalid("task instruction is required"));
        }
        if self.task_for_run(run).is_some() {
            return Err(HostError::invalid("nested persistent delegation is not supported; use RunSubagent for local help"));
        }
        let executor = self.resolve_agent(&request.target_id).ok_or_else(|| HostError::AgentNotFound(request.target_id.clone()))?;
        if &executor.id == from {
            return Err(HostError::invalid("delegate to another agent"));
        }
        let key = format!("{from}:{run}:{}", serde_json::to_string(&request)?);
        if let Some(task) = self.coordination.task_by_key(&key)? {
            return Ok(task);
        }
        let context = self.coordination_runtime.runs.lock().map_err(HostError::storage)?.get(run).cloned();
        let origin = context.as_ref().map(|c| c.origin.clone()).unwrap_or_else(|| ConversationRef::Agent(from.clone()));
        if let ConversationRef::Group(g) = &origin {
            let group = self.group(g).ok_or_else(|| HostError::GroupNotFound(g.to_string()))?;
            if !group.config().member_ids.contains(from) || !group.config().member_ids.contains(&executor.id) {
                return Err(HostError::invalid("both requester and executor must belong to the task's group"));
            }
        }
        let inputs = self.prepare_files(from, request.files, request.artifact_ids).await?;
        let now = gns_core::text::now_ms();
        let task = TaskRecord {
            id: EntryId::new().to_string(),
            requester: from.clone(),
            executor: executor.id.clone(),
            origin,
            origin_message: context.and_then(|c| c.message),
            instruction: request.task.trim().to_owned(),
            require_files: request.require_files,
            inputs,
            artifacts: Vec::new(),
            status: TaskStatus::Queued,
            summary: "Task queued".to_owned(),
            verification: String::new(),
            revision: 0,
            attempt: 0,
            background_resume_allowed: false,
            created_at: now,
            updated_at: now,
            submission: None,
        };
        self.coordination.commit_task(None, &task, &self.task_deliveries(&task, false), Some(&key))?;
        self.emit(HostEvent::TaskUpdated { task: Box::new(task.clone()) });
        Ok(task)
    }
    pub(crate) fn update_task_progress(
        &self,
        agent: &AgentId,
        run: &RunId,
        id: &str,
        status: TaskStatus,
        summary: String,
    ) -> Result<TaskRecord, HostError> {
        let task = self.executor_task(agent, run, id)?;
        if task.status != TaskStatus::Running || !matches!(status, TaskStatus::Running | TaskStatus::Blocked) {
            return Err(HostError::invalid("only a running task can report progress or become blocked"));
        }
        if summary.trim().is_empty() {
            return Err(HostError::invalid("a progress summary or blocking reason is required"));
        }
        self.commit_task_update(task, status, summary, status == TaskStatus::Blocked)
    }
    pub(crate) async fn finish_task(
        &self,
        agent: &AgentId,
        run: &RunId,
        id: &str,
        result: CompleteTaskRequest,
    ) -> Result<TaskRecord, HostError> {
        let mut task = self.executor_task(agent, run, id)?;
        if task.status.terminal() {
            if task.submission.as_ref() == Some(&result) {
                return Ok(task);
            }
            return Err(HostError::invalid("task is already terminal; conflicting completion rejected"));
        }
        if task.status != TaskStatus::Running || !matches!(result.status, TaskStatus::Completed | TaskStatus::Failed) {
            return Err(HostError::invalid("a running task can complete as completed or failed"));
        }
        if result.summary.trim().is_empty() {
            return Err(HostError::invalid("a result summary is required"));
        }
        if result.status == TaskStatus::Completed && task.require_files && result.files.is_empty() && result.artifact_ids.is_empty() {
            return Err(HostError::invalid("this task requires actual output files; submit paths or artifact_ids"));
        }
        task.artifacts = self.prepare_files(agent, result.files.clone(), result.artifact_ids.clone()).await?;
        task.verification = result.verification.clone();
        task.submission = Some(result.clone());
        // Revision check below rejects cancellation/resume races while snapshots were being copied.
        self.commit_task_update(task, result.status, result.summary, true)
    }
    pub(crate) fn register_coordination_run(&self, agent: &AgentId, run: &RunId, job: &RunJob) -> Result<bool, HostError> {
        let mut origin = job.options.group_id.clone().map(ConversationRef::Group).unwrap_or_else(|| ConversationRef::Agent(agent.clone()));
        // Room turns have no message_id in the member's private transcript.
        // Keep the triggering room message as the task's reply anchor instead.
        let origin_message = match (&origin, &job.message_id) {
            (ConversationRef::Group(id), None) => self.group(id).and_then(|g| {
                g.db.all().ok()?.into_iter().rev().find_map(|entry| match entry {
                    TranscriptEntry::Message { id, role: Role::User, .. } => Some(id),
                    _ => None,
                })
            }),
            _ => job.message_id.clone(),
        };
        if let Some(id) = &job.options.task_id {
            let task = self.coordination.task(id)?;
            if !task_participants_available(self, &task) {
                if !task.status.terminal() {
                    self.commit_task_update(
                        task,
                        TaskStatus::Failed,
                        "Original conversation or a participant is no longer available".to_owned(),
                        false,
                    )?;
                }
                return Ok(false);
            }
            if task.executor != *agent
                || task.attempt != job.options.task_attempt
                || !(matches!(task.status, TaskStatus::Queued | TaskStatus::Running)
                    || (task.status == TaskStatus::Blocked && task.background_resume_allowed))
            {
                return Ok(false);
            }
            origin = task.origin.clone();
            if task.status != TaskStatus::Running {
                self.commit_task_update(task, TaskStatus::Running, "Task running".to_owned(), false)?;
            }
        }
        self.coordination_runtime.runs.lock().map_err(HostError::storage)?.insert(
            run.clone(),
            CoordinationRun {
                agent: agent.clone(),
                origin,
                message: origin_message,
                task: job.options.task_id.clone(),
                attempt: job.options.task_attempt,
            },
        );
        Ok(true)
    }
    pub(crate) fn settle_task_run(&self, run: &RunId, result: &RunResult) {
        let Some(mut task) = self.task_for_run(run) else { return };
        if task.status != TaskStatus::Running {
            return;
        }
        let (state, reason) = if result.aborted || result.superseded {
            (TaskStatus::Blocked, "Task interrupted; resume it from the original conversation".to_owned())
        } else if let Some(error) = &result.error {
            (TaskStatus::Failed, error.clone())
        } else {
            task.background_resume_allowed = true;
            (TaskStatus::Blocked, "Executor ended without submitting a result; resume to finish or explain the blocker".to_owned())
        };
        if let Err(e) = self.commit_task_update(task, state, reason, true) {
            tracing::warn!(%e, "settling task failed");
        }
    }
    pub(crate) fn task_background_wake(&self, agent: &AgentId, context: Option<&TaskRunContext>, prompt: String) -> Result<(), HostError> {
        let Some(context) = context else { return self.wake_agent(agent, prompt) };
        let task = self.coordination.task(&context.task_id)?;
        if task.status.terminal() || self.shutdown_token().is_cancelled() {
            return Ok(());
        }
        if context.attempt != task.attempt || task.executor != *agent {
            return Ok(());
        }
        if task.status == TaskStatus::Blocked && !task.background_resume_allowed {
            return Ok(());
        }
        let mut options = RunOptions::agent_wake();
        options.task_id = Some(task.id.clone());
        options.task_attempt = task.attempt;
        let h = self.agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_string()))?;
        h.enqueue(RunJob::hidden(format!("Task {} background result:\n{prompt}", task.id), options))
    }

    pub(crate) fn recover_tasks(&self) -> Result<(), HostError> {
        for t in self.coordination.tasks()? {
            if t.status == TaskStatus::Running {
                self.commit_task_update(
                    t,
                    TaskStatus::Blocked,
                    "Host restarted during execution; resume explicitly to avoid repeating commands".to_owned(),
                    false,
                )?;
            }
        }
        for mut e in self.coordination.deliveries()? {
            e.wake_started = false;
            self.coordination.save_delivery(&e)?;
        }
        Ok(())
    }
    pub(crate) fn cancel_or_resume_task(&self, id: &str, resume: Option<String>) -> Result<TaskRecord, HostError> {
        let mut t = self.coordination.task(id)?;
        if t.status.terminal() {
            return Err(HostError::invalid("task is already terminal"));
        }
        if let Some(note) = resume {
            if t.status != TaskStatus::Blocked {
                return Err(HostError::invalid("only blocked tasks can be resumed"));
            }
            if self.agent(&t.executor).is_none() {
                return Err(HostError::AgentNotFound(t.executor.to_string()));
            }
            t.attempt += 1;
            if !note.trim().is_empty() {
                t.instruction.push_str(&format!("\n\nUser resumption instructions: {}", note.trim()));
            }
            return self.commit_task_update(t, TaskStatus::Queued, "Task resumed".to_owned(), false);
        }
        let t = self.commit_task_update(t, TaskStatus::Cancelled, "Cancelled by user".to_owned(), true)?;
        let runs = self.coordination_runtime.runs.lock().map_err(HostError::storage)?;
        for (run, ctx) in runs.iter().filter(|(_, c)| c.task.as_deref() == Some(id)) {
            let _ = ctx;
            if let Some(token) = self.run_cancel_token(run) {
                token.cancel();
            }
        }
        Ok(t)
    }
    pub(crate) fn executor_deleted(&self, agent: &AgentId) -> Result<(), HostError> {
        for t in self.coordination.tasks()? {
            if &t.executor == agent && !t.status.terminal() {
                self.commit_task_update(t, TaskStatus::Failed, "Executor was deleted".to_owned(), true)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn spawn_coordination_worker(host: Arc<HostInner>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let mut jobs = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = host.shutdown_token().cancelled_owned() => break,
                _ = jobs.join_next(), if !jobs.is_empty() => continue,
                _ = tick.tick() => {}
            }
            if let Err(e) = dispatch_tasks(&host, &mut jobs) {
                tracing::warn!(%e, "task dispatch failed");
            }
            if let Err(e) = dispatch_results(&host, &mut jobs).await {
                tracing::warn!(%e, "task delivery failed");
            }
        }
        jobs.shutdown().await;
    })
}

fn dispatch_tasks(host: &Arc<HostInner>, jobs: &mut tokio::task::JoinSet<()>) -> Result<(), HostError> {
    for task in host.coordination.tasks()?.into_iter().filter(|t| t.status == TaskStatus::Queued) {
        if !task_participants_available(host, &task) {
            host.commit_task_update(
                task,
                TaskStatus::Failed,
                "Original conversation or a participant is no longer available".to_owned(),
                false,
            )?;
            continue;
        }
        let Some(agent) = host.agent(&task.executor) else {
            host.commit_task_update(task, TaskStatus::Failed, "Executor no longer exists".to_owned(), true)?;
            continue;
        };
        let key = (task.id.clone(), task.attempt);
        if !host.coordination_runtime.scheduled.lock().map_err(HostError::storage)?.insert(key.clone()) {
            continue;
        }
        let mut options = RunOptions::agent_wake();
        options.task_id = Some(task.id.clone());
        options.task_attempt = task.attempt;
        let prompt = format!(
            "[delegated task {}] From agent {}.\n{}\n{}\nPrevious status: {}\nWork in your workspace. Use CompleteTask to submit the full files and report, or UpdateTask blocked with a concrete reason. The host delivers to the original conversation.",
            task.id,
            task.requester,
            task.instruction,
            artifact_prompt(&task.inputs),
            task.summary
        );
        let host = host.clone();
        jobs.spawn(async move {
            if let Err(e) = agent.run(RunJob::hidden(prompt, options)).await
                && let Ok(t) = host.coordination.task(&task.id)
                && !t.status.terminal()
                && t.attempt == task.attempt
            {
                let _ = host.commit_task_update(t, TaskStatus::Blocked, e.to_string(), true);
            }
            if let Ok(mut set) = host.coordination_runtime.scheduled.lock() {
                set.remove(&key);
            }
        });
    }
    Ok(())
}

async fn dispatch_results(host: &Arc<HostInner>, jobs: &mut tokio::task::JoinSet<()>) -> Result<(), HostError> {
    let _guard = host.coordination_runtime.delivery_lock.lock().await;
    for mut event in host.coordination.deliveries()? {
        if event.wake_started || event.retry_after > gns_core::text::now_ms() {
            continue;
        }
        if !event.projected {
            if let Err(error) = project_task_card(host, &event) {
                if matches!(error, HostError::AgentNotFound(_) | HostError::GroupNotFound(_)) {
                    event.error = Some(format!("delivery target deleted: {error}"));
                    event.done = true;
                    host.coordination.save_delivery(&event)?;
                    continue;
                }
                return Err(error);
            }
            event.projected = true;
            host.coordination.save_delivery(&event)?;
        }
        let Some(agent_id) = event.wake_agent.clone() else {
            event.done = true;
            host.coordination.save_delivery(&event)?;
            continue;
        };
        if host.coordination.task(&event.task.id)?.revision != event.task.revision {
            event.done = true;
            event.error = Some("Superseded by a newer task update".to_owned());
            host.coordination.save_delivery(&event)?;
            continue;
        }
        let Some(agent) = host.agent(&agent_id) else {
            event.done = true;
            event.error = Some("requester deleted".to_owned());
            host.coordination.save_delivery(&event)?;
            continue;
        };
        let text = format!(
            "[task result {}] {:?}: {}\nVerification reported by executor: {}{}\nThe task status and downloadable files are already visible to the user in the original conversation. Review the artifacts if useful and explain the result; do not retype file contents or send duplicate attachments. If blocked, await user input rather than automatically resuming.",
            event.task.id,
            event.task.status,
            event.task.summary,
            event.task.verification,
            artifact_prompt(&event.task.artifacts)
        );
        let mut options = RunOptions::agent_wake();
        if let ConversationRef::Group(g) = &event.target {
            let Some(group) = host.group(g) else {
                event.done = true;
                event.error = Some("Original group was deleted".to_owned());
                host.coordination.save_delivery(&event)?;
                continue;
            };
            if !group.config().member_ids.contains(&agent_id) {
                event.error = Some("requester is no longer a group member".to_owned());
                event.done = true;
                host.coordination.save_delivery(&event)?;
                continue;
            }
            let member =
                gns_core::groups::GroupMember { id: agent.id.clone(), name: agent.name(), description: agent.profile().description };
            let peers: Vec<_> = group
                .config()
                .member_ids
                .iter()
                .filter(|id| **id != agent_id)
                .filter_map(|id| host.agent(id))
                .map(|h| gns_core::groups::GroupMember { id: h.id.clone(), name: h.name(), description: h.profile().description })
                .collect();
            options = RunOptions::group_turn(
                g.clone(),
                gns_core::groups::build_group_member_system_prompt(&member, &group.description(), &peers),
                Lane::Agent,
            );
        }
        event.wake_started = true;
        event.wake_attempts += 1;
        host.coordination.save_delivery(&event)?;
        let host = host.clone();
        jobs.spawn(async move {
            let result = agent.run(RunJob::hidden(text, options)).await;
            match result {
                Ok(r) => {
                    if let ConversationRef::Group(g) = &event.target
                        && let Some(group) = host.group(g)
                    {
                        for m in r.room_messages {
                            let _ = crate::groups::post_message_to_room(
                                &host,
                                &group,
                                gns_core::groups::Speaker::Member { id: agent.id.clone(), name: agent.name() },
                                m,
                            );
                        }
                    }
                    event.error = if r.aborted || r.superseded { Some("Requester review interrupted".to_owned()) } else { r.error };
                }
                Err(e) => event.error = Some(e.to_string()),
            }
            event.wake_started = false;
            event.done = event.error.is_none() || event.wake_attempts >= 3;
            event.retry_after = gns_core::text::now_ms() + 2_000;
            if event.done && event.error.is_some() {
                host.emit(HostEvent::Error {
                    agent_id: Some(agent.id.clone()),
                    run_id: None,
                    message: format!(
                        "Task {} was delivered, but requester review failed after {} attempts: {}",
                        event.task.id,
                        event.wake_attempts,
                        event.error.as_deref().unwrap_or_default()
                    ),
                });
            }
            if let Err(e) = host.coordination.save_delivery(&event) {
                tracing::warn!(%e, "recording result wake failed");
            }
        });
    }
    Ok(())
}

fn task_participants_available(host: &HostInner, task: &TaskRecord) -> bool {
    if host.agent(&task.requester).is_none() || host.agent(&task.executor).is_none() {
        return false;
    }
    match &task.origin {
        ConversationRef::Agent(a) => host.agent(a).is_some(),
        ConversationRef::Group(g) => host.group(g).is_some_and(|g| {
            let members = g.config().member_ids;
            members.contains(&task.requester) && members.contains(&task.executor)
        }),
    }
}

fn project_task_card(host: &HostInner, event: &TaskDelivery) -> Result<(), HostError> {
    let id = EntryId::from(format!("task-card:{}", event.task.id));
    let entry = TranscriptEntry::SendMessage {
        id: id.clone(),
        message: event.task.result_message(),
        timestamp_ms: event.task.updated_at,
        run_id: RunId::from(format!("task:{}", event.task.id)),
        group_id: match &event.target {
            ConversationRef::Group(g) => Some(g.clone()),
            _ => None,
        },
        author: Some(AgentRef {
            id: event.task.executor.clone(),
            name: host.agent(&event.task.executor).map(|h| h.name()).unwrap_or_else(|| "Deleted executor".to_owned()),
        }),
        reply_to: event.task.origin_message.clone(),
        reactions: Vec::new(),
        responded_value: None,
        widget_skipped: false,
        widget_dismissed: false,
    };
    let (db, agent, event_agent) = match &event.target {
        ConversationRef::Agent(a) => {
            let h = host.agent(a).ok_or_else(|| HostError::AgentNotFound(a.to_string()))?;
            (h.db.clone(), Some(h), a.clone())
        }
        ConversationRef::Group(g) => {
            let h = host.group(g).ok_or_else(|| HostError::GroupNotFound(g.to_string()))?;
            (h.db.clone(), None, AgentId::from(g.as_str()))
        }
    };
    let old = db.entry(&id)?;
    if let Some(TranscriptEntry::SendMessage { message, .. }) = &old
        && message.task.as_ref().is_some_and(|t| t.revision >= event.task.revision)
    {
        return Ok(());
    }
    if old.is_some() {
        if let Some(h) = agent {
            h.update(&entry)?;
        } else {
            db.update_entry(&entry)?;
        }
        host.emit(HostEvent::EntryUpdated { agent_id: event_agent, entry });
    } else {
        if let Some(h) = agent {
            h.append(&entry)?;
        } else {
            db.append_entry(&entry)?;
        }
        host.emit(HostEvent::EntryAppended { agent_id: event_agent, entry });
    }
    Ok(())
}

impl AgentHost {
    /// Snapshot files inside this agent's roots. Publishing alone does not grant another agent access.
    pub async fn publish_artifacts(&self, agent: &str, files: Vec<String>) -> Result<Vec<ArtifactRef>, HostError> {
        let agent = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.prepare_files(&agent.id, files, Vec::new()).await
    }
    /// Send artifact references to a teammate or group, granting the destination access.
    pub async fn send_artifacts(&self, from: &str, target: &str, text: &str, artifact_ids: Vec<String>) -> Result<String, HostError> {
        let agent = self.inner.resolve_agent(from).ok_or_else(|| HostError::AgentNotFound(from.to_owned()))?;
        let artifacts = self.inner.prepare_files(&agent.id, Vec::new(), artifact_ids).await?;
        self.inner.messaging.send_with_artifacts(&self.inner, &agent.id, target, text, Vec::new(), artifacts, false).await
    }
    pub fn list_tasks(&self) -> Result<Vec<TaskRecord>, HostError> {
        self.inner.coordination.tasks()
    }
    pub fn task(&self, id: &str) -> Result<TaskRecord, HostError> {
        self.inner.coordination.task(id)
    }
    pub async fn delegate_task(&self, requester: &str, request: DelegateTaskRequest) -> Result<TaskRecord, HostError> {
        let agent = self.inner.resolve_agent(requester).ok_or_else(|| HostError::AgentNotFound(requester.to_owned()))?;
        self.inner.create_task(&agent.id, &RunId::new(), request).await
    }
    /// Delegate from a group on behalf of a member. The task card and review stay in that group.
    pub async fn delegate_group_task(&self, group: &str, requester: &str, request: DelegateTaskRequest) -> Result<TaskRecord, HostError> {
        let agent = self.inner.resolve_agent(requester).ok_or_else(|| HostError::AgentNotFound(requester.to_owned()))?;
        let group = self.inner.resolve_group(group).ok_or_else(|| HostError::GroupNotFound(group.to_owned()))?;
        let run = RunId::new();
        self.inner.coordination_runtime.runs.lock().map_err(HostError::storage)?.insert(
            run.clone(),
            CoordinationRun {
                agent: agent.id.clone(),
                origin: ConversationRef::Group(group.id.clone()),
                message: None,
                task: None,
                attempt: 0,
            },
        );
        let result = self.inner.create_task(&agent.id, &run, request).await;
        self.inner.coordination_runtime.runs.lock().map_err(HostError::storage)?.remove(&run);
        result
    }
    pub fn cancel_task(&self, id: &str) -> Result<TaskRecord, HostError> {
        self.inner.cancel_or_resume_task(id, None)
    }
    pub fn resume_task(&self, id: &str, instructions: String) -> Result<TaskRecord, HostError> {
        self.inner.cancel_or_resume_task(id, Some(instructions))
    }
    pub fn artifact(&self, id: &str) -> Result<ArtifactRef, HostError> {
        self.inner.coordination.artifact(id)
    }
    /// Owner-level download. HTTP callers must pass the server's owner authentication.
    pub async fn artifact_path(&self, id: &str) -> Result<std::path::PathBuf, HostError> {
        let store = self.inner.coordination.clone();
        let id = id.to_owned();
        tokio::task::spawn_blocking(move || store.verified_path(&id)).await.map_err(HostError::storage)?
    }
    pub async fn fetch_artifact(&self, agent: &str, id: &str) -> Result<String, HostError> {
        let agent = self.inner.resolve_agent(agent).ok_or_else(|| HostError::AgentNotFound(agent.to_owned()))?;
        self.inner.fetch_file(&agent.id, id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentHostConfig;
    use gns_llm::MockLlm;

    #[tokio::test]
    async fn completion_is_idempotent_and_cancelled_or_stale_runs_cannot_submit() {
        let root = tempfile::tempdir().unwrap();
        let mut config = AgentHostConfig::new(root.path());
        config.kickstart_new_agents = false;
        config.memory_extraction = false;
        let host = AgentHost::open(config, Arc::new(MockLlm::new())).await.unwrap();
        let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
        let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
        let run = RunId::new();
        let t = host
            .inner
            .create_task(
                &a.id,
                &run,
                DelegateTaskRequest { target_id: b.id.to_string(), task: "report".into(), require_files: true, ..Default::default() },
            )
            .await
            .unwrap();
        let t = host.inner.commit_task_update(t, TaskStatus::Running, "running".into(), false).unwrap();
        host.inner.coordination_runtime.runs.lock().unwrap().insert(
            run.clone(),
            CoordinationRun { agent: b.id.clone(), origin: t.origin.clone(), message: None, task: Some(t.id.clone()), attempt: 0 },
        );
        let source = host.inner.agent(&b.id).unwrap().workspace_dir.join("result.txt");
        std::fs::write(&source, "full result").unwrap();
        let submission = CompleteTaskRequest {
            status: TaskStatus::Completed,
            summary: "done".into(),
            verification: "checked".into(),
            files: vec!["result.txt".into()],
            artifact_ids: vec![],
        };
        assert!(host.inner.finish_task(&a.id, &run, &t.id, submission.clone()).await.is_err());
        let result = host.inner.finish_task(&b.id, &run, &t.id, submission.clone()).await.unwrap();
        std::fs::remove_file(&source).unwrap();
        let repeated = host.inner.finish_task(&b.id, &run, &t.id, submission.clone()).await.unwrap();
        assert_eq!(repeated, result);
        assert_eq!(std::fs::read_dir(root.path().join("artifacts")).unwrap().count(), 1);
        let mut conflict = submission.clone();
        conflict.summary = "different".into();
        assert!(host.inner.finish_task(&b.id, &run, &t.id, conflict).await.is_err());

        let t = host
            .inner
            .create_task(
                &a.id,
                &RunId::new(),
                DelegateTaskRequest { target_id: b.id.to_string(), task: "second report".into(), ..Default::default() },
            )
            .await
            .unwrap();
        let mut t = host.inner.commit_task_update(t, TaskStatus::Running, "running".into(), false).unwrap();
        host.inner.coordination_runtime.runs.lock().unwrap().get_mut(&run).unwrap().task = Some(t.id.clone());
        let context = TaskRunContext { task_id: t.id.clone(), attempt: 0, run_id: run.clone() };
        t.background_resume_allowed = true;
        let t = host.inner.commit_task_update(t, TaskStatus::Blocked, "waiting for shell".into(), false).unwrap();
        host.resume_task(&t.id, "retry".into()).unwrap();
        assert!(host.inner.finish_task(&b.id, &run, &t.id, submission.clone()).await.is_err());
        host.inner.task_background_wake(&b.id, Some(&context), "obsolete completion".into()).unwrap();
        assert_eq!(host.task(&t.id).unwrap().attempt, 1);
        host.cancel_task(&t.id).unwrap();
        assert!(host.inner.finish_task(&b.id, &run, &t.id, submission).await.is_err());
        host.inner.task_background_wake(&b.id, Some(&context), "cancelled completion".into()).unwrap();
        assert_eq!(host.task(&t.id).unwrap().status, TaskStatus::Cancelled);
        host.shutdown().await;
    }
}
