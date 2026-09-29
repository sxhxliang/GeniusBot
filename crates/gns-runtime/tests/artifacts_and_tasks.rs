use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const SORTS: &str = include_str!("fixtures/optimized_sorts.py");

fn config(root: &std::path::Path) -> AgentHostConfig {
    let mut c = AgentHostConfig::new(root);
    c.kickstart_new_agents = false;
    c.memory_extraction = false;
    c.episode_interval = 1000;
    c.dream_after_idle = None;
    c
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(8), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

fn task_id(req: &LlmRequest) -> Option<&str> {
    req.system.split("## Delegated task ").nth(1)?.lines().next()
}

fn complete_model() -> MockLlm {
    MockLlm::new().with_responder(|req| {
        if let Some(id) = task_id(req) {
            assert!(!req.tools.iter().any(|t| t.name == "SendMessage" || t.name == "DelegateTask"));
            assert_eq!(req.options.required_tool, None, "task turns must not force an unavailable SendMessage");
            return LlmResponse::tool_call("CompleteTask", json!({
                "task_id": id, "status":"completed", "summary":"All three algorithms and their tests are submitted.",
                "verification":"Test program included; independently inspect before accepting the claims.", "files":["optimized_sorts.py"]
            }));
        }
        if req.messages.iter().any(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("[task result"))) {
            // The consumer sees references, not the file body.
            assert!(!format!("{:?}", req.messages).contains("def test_all_sorts"));
        }
        LlmResponse::text("Nothing else to add.")
    })
}

#[tokio::test]
async fn chat_with_an_old_task_id_cannot_submit_but_resumed_task_can() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockLlm::new().with_responder(|req| {
        if let Some(id) = task_id(req) {
            assert!(req.tools.iter().any(|t| t.name == "UpdateTask"));
            assert!(req.tools.iter().any(|t| t.name == "CompleteTask"));
            assert!(!req.tools.iter().any(|t| t.name == "SendMessage"));
            let resumed = req.messages.iter().any(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("ready to resume")));
            return if resumed {
                LlmResponse::tool_call("CompleteTask", json!({"task_id":id,"status":"completed","summary":"done"}))
            } else {
                LlmResponse::tool_call("UpdateTask", json!({"task_id":id,"status":"blocked","summary":"Need input"}))
            };
        }
        assert!(!req.tools.iter().any(|t| matches!(t.name.as_str(), "CompleteTask" | "UpdateTask")));
        assert!(req.tools.iter().any(|t| t.name == "GetTask"));
        if let Some(LlmMessage::User { text, .. }) = req.messages.last()
            && let Some(rest) = text.split("continue old task ").nth(1)
        {
            let id = rest.split_whitespace().next().unwrap();
            // Even a model that ignores the tool list cannot submit from chat.
            return LlmResponse::tool_call("CompleteTask", json!({"task_id":id,"status":"failed","summary":"give up"}));
        }
        if let Some(LlmMessage::ToolResults(results)) = req.messages.last()
            && let Some(result) = results.iter().find(|r| r.name == "CompleteTask")
        {
            assert!(result.content.contains("requires the executor's active delegated task run"), "{}", result.content);
            return LlmResponse::tool_call("SendMessage", json!({"type":"text","content":"Resume the blocked task from its task card."}));
        }
        LlmResponse::text("done")
    });
    let host = AgentHost::open(config(dir.path()), Arc::new(model)).await.unwrap();
    let planner = host.create_agent(AgentSpec::new("Planner", "")).await.unwrap();
    let coder = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    let task = host
        .delegate_task(
            planner.id.as_str(),
            DelegateTaskRequest { target_id: coder.id.to_string(), task: "report".into(), require_files: false, ..Default::default() },
        )
        .await
        .unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Blocked).await;
    let before = host.task(&task.id).unwrap();
    let chat = host.send_user_message(coder.id.as_str(), format!("continue old task {}", task.id), vec![]).await.unwrap();
    assert_eq!(chat.sent_message_count, 1);
    assert_eq!(host.task(&task.id).unwrap().revision, before.revision);
    assert_eq!(host.task(&task.id).unwrap().status, TaskStatus::Blocked);
    host.resume_task(&task.id, "ready to resume".into()).unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Completed).await;
    host.shutdown().await;
}

#[tokio::test]
async fn task_runs_shell_and_delivers_the_created_file() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockLlm::new().with_responder(|req| {
        let Some(id) = task_id(req) else { return LlmResponse::text("done") };
        if let Some(LlmMessage::ToolResults(results)) = req.messages.last() {
            let output = results.iter().find(|r| r.name == "Shell").unwrap();
            assert!(output.content.starts_with("Exit code: 0"), "{}", output.content);
            assert!(output.content.contains("Hello from task"), "{}", output.content);
            return LlmResponse::tool_call("CompleteTask", json!({"task_id":id,"status":"completed","summary":"File created and checked","verification":"Shell exited 0 and printed Hello from task","files":["hello.txt"]}));
        }
        let command = if cfg!(windows) {
            "Set-Content -LiteralPath hello.txt -Value 'Hello from task' -Encoding UTF8; Get-Content -LiteralPath hello.txt"
        } else {
            "printf 'Hello from task\\n' > hello.txt; cat hello.txt"
        };
        LlmResponse::tool_call("Shell", json!({"command":command}))
    });
    let host = AgentHost::open(config(dir.path()), Arc::new(model)).await.unwrap();
    let planner = host.create_agent(AgentSpec::new("Planner", "")).await.unwrap();
    let coder = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    let task = host
        .delegate_task(
            planner.id.as_str(),
            DelegateTaskRequest {
                target_id: coder.id.to_string(),
                task: "Create a file, run a command and submit the file".into(),
                require_files: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Completed).await;
    let completed = host.task(&task.id).unwrap();
    assert_eq!(completed.artifacts.len(), 1);
    let content = std::fs::read_to_string(host.artifact_path(&completed.artifacts[0].id).await.unwrap()).unwrap();
    assert_eq!(content.trim_start_matches('\u{feff}').trim(), "Hello from task");
    eventually(|| host.transcript(planner.id.as_str(), 100).unwrap().iter().any(|e| matches!(e,
        TranscriptEntry::SendMessage { message, .. } if message.task.as_ref().is_some_and(|t| t.id == task.id && t.status == TaskStatus::Completed)
    ))).await;
    host.shutdown().await;
}

#[tokio::test]
async fn complete_file_is_delivered_without_retyping_and_survives_source_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let model = complete_model();
    let host = AgentHost::open(config(dir.path()), Arc::new(model.clone())).await.unwrap();
    let planner = host.create_agent(AgentSpec::new("Planner", "delegate and review")).await.unwrap();
    let coder = host.create_agent(AgentSpec::new("Coder", "write files")).await.unwrap();
    let path = dir.path().join("agents").join(coder.id.as_str()).join("workspace/optimized_sorts.py");
    std::fs::write(&path, SORTS).unwrap();
    assert!(SORTS.chars().count() > 8000);
    assert_eq!(SORTS.lines().count(), 298);
    let task = host
        .delegate_task(
            planner.id.as_str(),
            DelegateTaskRequest {
                target_id: coder.id.to_string(),
                task: "Deliver all algorithms including tests and benchmark code".into(),
                require_files: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Completed).await;
    eventually(|| {
        host.transcript(planner.id.as_str(), 100).unwrap().iter().any(|e| {
            matches!(e,
        TranscriptEntry::SendMessage { message, .. } if message.task.as_ref().is_some_and(|t| t.status == TaskStatus::Completed))
        })
    })
    .await;
    let completed = host.task(&task.id).unwrap();
    assert_eq!(completed.artifacts.len(), 1);
    let artifact = &completed.artifacts[0];
    assert_eq!(std::fs::read(host.artifact_path(&artifact.id).await.unwrap()).unwrap(), SORTS.as_bytes());
    let local = host.fetch_artifact(planner.id.as_str(), &artifact.id).await.unwrap();
    assert_eq!(std::fs::read(&local).unwrap(), SORTS.as_bytes());
    assert_eq!(host.fetch_artifact(planner.id.as_str(), &artifact.id).await.unwrap(), local);
    let stranger = host.create_agent(AgentSpec::new("Other", "")).await.unwrap();
    assert!(host.fetch_artifact(stranger.id.as_str(), &artifact.id).await.is_err());
    std::fs::remove_file(path).unwrap();
    host.delete_agent(coder.id.as_str()).await.unwrap();
    assert_eq!(std::fs::read(host.artifact_path(&artifact.id).await.unwrap()).unwrap(), SORTS.as_bytes());
    let entries = host.transcript(planner.id.as_str(), 100).unwrap();
    assert_eq!(entries.iter().filter(|e| matches!(e, TranscriptEntry::SendMessage { message, .. } if message.task.is_some())).count(), 1);
    assert!(model.requests().iter().all(|r| !r.tools.is_empty()));
    host.shutdown().await;
    let restarted = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    assert_eq!(restarted.task(&task.id).unwrap().status, TaskStatus::Completed);
    assert_eq!(std::fs::read(restarted.artifact_path(&artifact.id).await.unwrap()).unwrap(), SORTS.as_bytes());
    restarted.shutdown().await;
}

#[tokio::test]
async fn missing_outputs_cannot_complete_and_blocked_task_can_resume_or_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockLlm::new().with_responder(|req| {
        if let Some(id) = task_id(req) {
            if matches!(req.messages.last(), Some(LlmMessage::ToolResults(r)) if r.iter().any(|r| r.content.contains("requires actual output files"))) {
                return LlmResponse::tool_call("UpdateTask", json!({"task_id":id,"status":"blocked","summary":"Need the source file"}));
            }
            return LlmResponse::tool_call("CompleteTask", json!({"task_id":id,"status":"completed","summary":"done"}));
        }
        LlmResponse::text("idle")
    });
    let host = AgentHost::open(config(dir.path()), Arc::new(model)).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let task = host
        .delegate_task(
            a.id.as_str(),
            DelegateTaskRequest { target_id: b.id.to_string(), task: "make a file".into(), require_files: true, ..Default::default() },
        )
        .await
        .unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Blocked).await;
    assert!(host.task(&task.id).unwrap().artifacts.is_empty());
    let resumed = host.resume_task(&task.id, "Try again".into()).unwrap();
    assert_eq!(resumed.attempt, 1);
    let cancelled = host.cancel_task(&task.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(host.resume_task(&task.id, String::new()).is_err());
    host.shutdown().await;
}

#[tokio::test]
async fn ordinary_a2a_file_is_fetchable_but_no_body_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockLlm::new().with_responder(|req| {
        let target = req.system.split("Teammate ").nth(1).and_then(|s| s.split_whitespace().next());
        if let Some(target) = target {
            if !req.messages.iter().any(|m| matches!(m, LlmMessage::ToolResults(_))) {
                return LlmResponse::tool_call("SendMessage", json!({"type":"text","content":"Sharing the file."}));
            }
            if !req.messages.iter().any(|m| matches!(m,LlmMessage::ToolResults(r) if r.iter().any(|r|r.name=="SendToAgent"))) {
                return LlmResponse::tool_call(
                    "SendToAgent",
                    json!({"target_id":target,"message":"Please inspect this file","files":["data.bin"]}),
                );
            }
        }
        LlmResponse::text("idle")
    });
    let host = AgentHost::open(config(dir.path()), Arc::new(model)).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", format!("Teammate {}", b.id))).await.unwrap();
    let bytes: Vec<u8> = (0..20000).map(|n| (n % 256) as u8).collect();
    std::fs::write(dir.path().join("agents").join(a.id.as_str()).join("workspace/data.bin"), &bytes).unwrap();
    host.send_user_message(a.id.as_str(), "Share data.bin", vec![]).await.unwrap();
    let mut refs = Vec::new();
    eventually(|| {
        refs = host
            .transcript(b.id.as_str(), 100)
            .unwrap()
            .iter()
            .flat_map(|e| match e {
                TranscriptEntry::Message { artifacts, .. } => artifacts.clone(),
                _ => vec![],
            })
            .collect();
        !refs.is_empty()
    })
    .await;
    assert_eq!(std::fs::read(host.fetch_artifact(b.id.as_str(), &refs[0].id).await.unwrap()).unwrap(), bytes);
    host.shutdown().await;
}

#[tokio::test]
async fn group_task_keeps_structured_files_and_checks_current_membership() {
    let dir = tempfile::tempdir().unwrap();
    let target = Arc::new(std::sync::Mutex::new(String::new()));
    let responder_target = target.clone();
    let model = MockLlm::new().with_responder(move |req| {
        if let Some(id) = task_id(req) {
            return LlmResponse::tool_call(
                "CompleteTask",
                json!({"task_id":id,"status":"completed","summary":"Group report ready","files":["report.txt"]}),
            );
        }
        if req.system.contains("You are Planner, one participant")
            && !req.messages.iter().any(|m| matches!(m,LlmMessage::ToolResults(r) if r.iter().any(|r|r.name=="DelegateTask")))
        {
            return LlmResponse::tool_call(
                "DelegateTask",
                json!({"target_id":*responder_target.lock().unwrap(),"task":"Deliver the full report to this room","require_files":true}),
            );
        }
        if matches!(req.messages.last(), Some(LlmMessage::ToolResults(_))) {
            return LlmResponse::text("done");
        }
        LlmResponse::tool_call("SendMessage", json!({"type":"text","content":"(pass)"}))
    });
    let host = AgentHost::open(config(dir.path()), Arc::new(model)).await.unwrap();
    let planner = host.create_agent(AgentSpec::new("Planner", "")).await.unwrap();
    let coder = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    let reader = host.create_agent(AgentSpec::new("Reader", "")).await.unwrap();
    *target.lock().unwrap() = coder.id.to_string();
    std::fs::write(dir.path().join("agents").join(coder.id.as_str()).join("workspace/report.txt"), b"complete room report").unwrap();
    let group = host
        .create_group(GroupSpec {
            name: "Review".into(),
            description: "".into(),
            members: vec![planner.id.clone(), coder.id.clone(), reader.id.clone()],
        })
        .await
        .unwrap();
    host.post_to_group_as_user(group.id.as_str(), "@Planner delegate the report").await.unwrap();
    eventually(|| host.list_tasks().unwrap().iter().any(|t| t.status == TaskStatus::Completed)).await;
    let t = host.list_tasks().unwrap().pop().unwrap();
    assert_eq!(t.origin, ConversationRef::Group(group.id.clone()));
    assert!(t.origin_message.is_some(), "remember the originating room message");
    eventually(|| {
        host.group_history(group.id.as_str()).unwrap().iter().any(|m| m.task.as_ref().is_some_and(|t| t.status == TaskStatus::Completed))
    })
    .await;
    let history = host.group_history(group.id.as_str()).unwrap();
    let card = history.iter().find(|m| m.task.is_some()).unwrap();
    assert_eq!(card.artifacts.len(), 1);
    assert!(!card.content.contains("FetchArtifact"));
    let id = &card.artifacts[0].id;
    assert_eq!(std::fs::read(host.fetch_artifact(reader.id.as_str(), id).await.unwrap()).unwrap(), b"complete room report");
    host.set_group_members(group.id.as_str(), vec![planner.id.clone(), coder.id.clone()]).unwrap();
    assert!(host.fetch_artifact(reader.id.as_str(), id).await.is_err());
    assert!(
        !host
            .transcript(planner.id.as_str(), 100)
            .unwrap()
            .iter()
            .any(|e| matches!(e,TranscriptEntry::SendMessage{message,group_id:None,..} if message.task.is_some()))
    );
    host.shutdown().await;
}

#[tokio::test]
async fn committed_result_replays_after_restart_without_duplicate_cards() {
    use gns_runtime::gns_store::CoordinationStore;
    let dir = tempfile::tempdir().unwrap();
    let host = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    host.shutdown().await;
    let store = CoordinationStore::open(dir.path()).unwrap();
    let task = TaskRecord {
        id: "recovery-task".into(),
        requester: a.id.clone(),
        executor: b.id.clone(),
        origin: ConversationRef::Agent(a.id.clone()),
        origin_message: None,
        instruction: "work".into(),
        require_files: false,
        inputs: vec![],
        artifacts: vec![],
        status: TaskStatus::Completed,
        summary: "Committed before crash".into(),
        verification: "".into(),
        revision: 1,
        attempt: 0,
        background_resume_allowed: false,
        created_at: 1,
        updated_at: 2,
        submission: None,
    };
    let mut delivery = TaskDelivery {
        id: "recovery-event".into(),
        task: task.clone(),
        target: task.origin.clone(),
        wake_agent: None,
        projected: false,
        wake_started: false,
        wake_attempts: 0,
        retry_after: 0,
        done: false,
        error: None,
    };
    store.commit_task(None, &task, &[delivery.clone()], Some("recovery-key")).unwrap();
    let host = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    eventually(|| store.delivery(&delivery.id).unwrap().done).await;
    assert_eq!(
        host.transcript(a.id.as_str(), 100)
            .unwrap()
            .iter()
            .filter(|e| matches!(e,TranscriptEntry::SendMessage{message,..} if message.task.is_some()))
            .count(),
        1
    );
    host.shutdown().await;
    // Crash after projection but before marking the outbox event: replay the same durable event.
    delivery.projected = false;
    delivery.done = false;
    store.save_delivery(&delivery).unwrap();
    let host = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    eventually(|| store.delivery(&delivery.id).unwrap().done).await;
    assert_eq!(
        host.transcript(a.id.as_str(), 100)
            .unwrap()
            .iter()
            .filter(|e| matches!(e,TranscriptEntry::SendMessage{message,..} if message.task.is_some()))
            .count(),
        1
    );
    host.shutdown().await;
    // Crash after projection, before requester wake. Restart must deliver that wake too.
    delivery.projected = true;
    delivery.wake_agent = Some(a.id.clone());
    delivery.wake_started = true;
    delivery.done = false;
    store.save_delivery(&delivery).unwrap();
    let model = MockLlm::new();
    let host = AgentHost::open(config(dir.path()), Arc::new(model.clone())).await.unwrap();
    eventually(|| store.delivery(&delivery.id).unwrap().done).await;
    assert!(!model.requests().is_empty());
    assert_eq!(
        host.transcript(a.id.as_str(), 100)
            .unwrap()
            .iter()
            .filter(|e| matches!(e,TranscriptEntry::SendMessage{message,..} if message.task.is_some()))
            .count(),
        1
    );
    host.shutdown().await;
}

#[tokio::test]
async fn ordinary_group_files_survive_history_and_outside_sources_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let host = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let group = host
        .create_group(GroupSpec { name: "Files".into(), description: "".into(), members: vec![a.id.clone(), b.id.clone()] })
        .await
        .unwrap();
    let workspace = dir.path().join("agents").join(a.id.as_str()).join("workspace");
    std::fs::write(workspace.join("data.bin"), [0, 128, 255, 0]).unwrap();
    std::fs::write(dir.path().join("outside.txt"), "private").unwrap();
    assert!(host.publish_artifacts(a.id.as_str(), vec!["../../../outside.txt".into()]).await.is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), workspace.join("escape")).unwrap();
        assert!(host.publish_artifacts(a.id.as_str(), vec!["escape".into()]).await.is_err());
    }
    let files = host.publish_artifacts(a.id.as_str(), vec!["data.bin".into()]).await.unwrap();
    assert!(host.fetch_artifact(b.id.as_str(), &files[0].id).await.is_err());
    host.send_artifacts(a.id.as_str(), group.id.as_str(), "inspect the binary", vec![files[0].id.clone()]).await.unwrap();
    let history = host.group_history(group.id.as_str()).unwrap();
    assert_eq!(history[0].artifacts, files);
    assert_eq!(std::fs::read(host.fetch_artifact(b.id.as_str(), &files[0].id).await.unwrap()).unwrap(), [0, 128, 255, 0]);
    host.shutdown().await;
}

#[tokio::test]
async fn omitted_completion_gets_one_reminder_then_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let model = MockLlm::new().with_responder(|_| LlmResponse::text("I am done"));
    let host = AgentHost::open(config(dir.path()), Arc::new(model.clone())).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let task = host
        .delegate_task(
            a.id.as_str(),
            DelegateTaskRequest { target_id: b.id.to_string(), task: "write a file".into(), require_files: true, ..Default::default() },
        )
        .await
        .unwrap();
    eventually(|| host.task(&task.id).unwrap().status == TaskStatus::Blocked).await;
    assert_eq!(model.requests().iter().filter(|r| task_id(r).is_some()).count(), 2);
    assert!(host.task(&task.id).unwrap().summary.contains("without submitting"));
    host.shutdown().await;
}

#[tokio::test]
async fn restart_blocks_running_work_and_deleted_origin_does_not_reroute() {
    use gns_runtime::gns_store::CoordinationStore;
    let dir = tempfile::tempdir().unwrap();
    let host = AgentHost::open(config(dir.path()), Arc::new(MockLlm::new())).await.unwrap();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let task = host
        .delegate_task(a.id.as_str(), DelegateTaskRequest { target_id: b.id.to_string(), task: "work".into(), ..Default::default() })
        .await
        .unwrap();
    host.shutdown().await;
    let store = CoordinationStore::open(dir.path()).unwrap();
    let mut t = store.task(&task.id).unwrap();
    let revision = t.revision;
    t.revision += 1;
    t.status = TaskStatus::Running;
    t.background_resume_allowed = false;
    store.commit_task(Some(revision), &t, &[], None).unwrap();
    let model = MockLlm::new();
    let host = AgentHost::open(config(dir.path()), Arc::new(model.clone())).await.unwrap();
    assert_eq!(host.task(&t.id).unwrap().status, TaskStatus::Blocked);
    assert!(host.task(&t.id).unwrap().summary.contains("restarted"));
    assert!(model.requests().iter().all(|r| task_id(r).is_none()));
    host.delete_agent(a.id.as_str()).await.unwrap();
    host.resume_task(&t.id, "resume".into()).unwrap();
    eventually(|| host.task(&t.id).unwrap().status == TaskStatus::Failed).await;
    assert!(
        !host
            .transcript(b.id.as_str(), 100)
            .unwrap()
            .iter()
            .any(|e| matches!(e, TranscriptEntry::SendMessage { message, .. } if message.task.is_some()))
    );
    host.shutdown().await;
}
