//! Third batch: subagents, memory extraction and dreaming, background shell
//! jobs, webhook triggers, mention injection, start-of-turn ack.

use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

fn send_message(text: &str) -> LlmResponse {
    LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type": "text", "content": text}))
}

fn last_user(req: &LlmRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find_map(|m| match m {
            LlmMessage::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn is_tool_results(req: &LlmRequest) -> bool {
    matches!(req.messages.last(), Some(LlmMessage::ToolResults(_)))
}

async fn open_host(mock: MockLlm, tweak: impl FnOnce(&mut AgentHostConfig)) -> (AgentHost, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.scheduler_tick = Duration::from_millis(200);
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    tweak(&mut config);
    let host = AgentHost::open(config, Arc::new(mock)).await.unwrap();
    (host, dir)
}

async fn wait_for<F: Fn(&HostEvent) -> bool>(rx: &mut broadcast::Receiver<HostEvent>, pred: F, timeout: Duration) -> Option<HostEvent> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) if pred(&event) => return Some(event),
            Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        }
    }
}

#[tokio::test]
async fn subagent_runs_in_workspace_and_reports_back() {
    let mock = MockLlm::new().with_responder(|req| {
        if req.system.contains("running as the generalPurpose subagent") {
            if is_tool_results(req) {
                return LlmResponse::text("REPORT: notes.txt contains 'alpha'.");
            }
            // The subagent must not see social tools.
            assert!(
                req.tools.iter().all(|t| [SHELL_TOOL_NAME, AWAIT_SHELL_TOOL_NAME, READ_TOOL_NAME].contains(&t.name.as_str())),
                "{:?}",
                req.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
            return LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf alpha > notes.txt && cat notes.txt"}));
        }
        if is_tool_results(req) {
            let results = match req.messages.last() {
                Some(LlmMessage::ToolResults(r)) => r.clone(),
                _ => vec![],
            };
            if let Some(sub) = results.iter().find(|r| r.name == RUN_SUBAGENT_TOOL_NAME) {
                return send_message(&format!("Subagent said: {}", sub.content));
            }
            return LlmResponse::text("done");
        }
        if last_user(req).contains("delegate") {
            return send_message("On it.").and_tool_call(
                RUN_SUBAGENT_TOOL_NAME,
                json!({"task": "Write notes.txt containing alpha and read it back.", "label": "notes"}),
            );
        }
        LlmResponse::text("idle")
    });
    let (host, dir) = open_host(mock.clone(), |_| {}).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Boss", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "please delegate the notes job", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    assert!(dir.path().join("agents").join(agent.id.as_str()).join("workspace").join("notes.txt").exists());
    let mut started = false;
    let mut ended = None;
    while let Ok(e) = rx.try_recv() {
        match e {
            HostEvent::SubagentStarted { label, .. } if label == "notes" => started = true,
            HostEvent::SubagentEnded { steps, aborted, .. } => ended = Some((steps, aborted)),
            _ => {}
        }
    }
    assert!(started);
    assert_eq!(ended, Some((2, false)));
    let final_msg = host.transcript(agent.id.as_str(), 50).unwrap().into_iter().rev().find_map(|e| match e {
        TranscriptEntry::SendMessage { message, .. } => message.content,
        _ => None,
    });
    assert!(final_msg.unwrap().contains("REPORT: notes.txt contains 'alpha'"));
    // Subagent tool calls are not persisted into the parent's transcript.
    let transcript = host.transcript(agent.id.as_str(), 50).unwrap();
    assert!(!transcript.iter().any(|e| matches!(e, TranscriptEntry::ToolCall { name, .. } if name == SHELL_TOOL_NAME)));
    host.shutdown().await;
}

#[tokio::test]
async fn readonly_subagent_only_gets_read() {
    let mock = MockLlm::new().with_responder(|req| {
        if req.system.contains("running as the generalPurpose subagent") {
            let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(names, vec![READ_TOOL_NAME]);
            assert!(req.system.contains("Operate in readonly mode"));
            return LlmResponse::text("nothing to do");
        }
        if is_tool_results(req) {
            return LlmResponse::text("done");
        }
        send_message("ok").and_tool_call(RUN_SUBAGENT_TOOL_NAME, json!({"task": "look around", "readonly": true}))
    });
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let agent = host.create_agent(AgentSpec::new("Boss", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "audit the workspace please", vec![]).await.unwrap();
    assert!(mock.requests().iter().any(|r| r.system.contains("running as the generalPurpose subagent")));
    host.shutdown().await;
}

#[tokio::test]
async fn start_of_turn_ack_reminds_after_silent_tool_calls() {
    // Tools run (never refused); after more than one silent call the model is
    // reminded to acknowledge the user, as in the original middleware.
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "echo hi"})),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "echo hi again"})),
        send_message("Checking now.").and_tool_call(SHELL_TOOL_NAME, json!({"command": "echo hi"})),
        send_message("It printed hi."),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "run echo hi for me", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    let requests = mock.requests();
    assert!(
        !matches!(requests[1].messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("without first acknowledging")),
        "one silent call is under the threshold"
    );
    assert!(
        matches!(requests[2].messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("without first acknowledging")),
        "{:?}",
        requests[2].messages.last()
    );
    let mut executed = 0;
    while let Ok(e) = rx.try_recv() {
        match e {
            HostEvent::ToolCall { name, status: ToolCallStatus::Finished, .. } if name == SHELL_TOOL_NAME => executed += 1,
            HostEvent::ToolCall { name, status: ToolCallStatus::Failed, .. } if name == SHELL_TOOL_NAME => {
                panic!("tools must not be refused")
            }
            _ => {}
        }
    }
    assert_eq!(executed, 3);
    // Reminders are injected into the model context only, never persisted.
    let transcript = host.transcript(agent.id.as_str(), 50).unwrap();
    assert!(!transcript.iter().any(
        |e| matches!(e, TranscriptEntry::Message { role: Role::System, content, .. } if content.contains("without first acknowledging"))
    ));
    // Silence-allowed turns (routines) get no reminders at all.
    let mock2 = MockLlm::scripted(vec![LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "true"})), LlmResponse::text("quiet")]);
    let (host2, _dir2) = open_host(mock2.clone(), |_| {}).await;
    let agent2 = host2.create_agent(AgentSpec::new("B", "")).await.unwrap();
    host2.enqueue(agent2.id.as_str(), gns_runtime::RunJob::hidden("[routine] check", RunOptions::automation())).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock2.call_count(), 2);
    assert!(matches!(mock2.requests()[1].messages.last(), Some(LlmMessage::ToolResults(r)) if r[0].content.contains("Exit code: 0")));
    host.shutdown().await;
    host2.shutdown().await;
}

#[tokio::test]
async fn mentioned_teammates_are_injected_as_context() {
    let mock = MockLlm::new().with_responder(|req| if is_tool_results(req) { LlmResponse::text("done") } else { send_message("ok") });
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let alice = host.create_agent(AgentSpec::new("Alice", "")).await.unwrap();
    let bob = host.create_agent(AgentSpec::new("Bob Builder", "builds things")).await.unwrap();
    host.send_user_message(alice.id.as_str(), "can you ask @bob about the roadmap?", vec![]).await.unwrap();
    let req = mock.requests()[0].clone();
    let note = req.messages.iter().find_map(|m| match m {
        LlmMessage::User { text, .. } if text.contains("[Agents mentioned in this message") => Some(text.clone()),
        _ => None,
    });
    let note = note.expect("mention context injected");
    assert!(note.contains(&format!("Bob Builder (id: {})", bob.id)));
    assert!(!note.contains("Alice"), "self is never listed");
    // Without a mention nothing is injected; an email address is not a mention.
    host.send_user_message(alice.id.as_str(), "mail me at me@bob.com", vec![]).await.unwrap();
    let req = mock.requests().last().unwrap().clone();
    let notes = req.messages.iter().filter(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("[Agents mentioned"))).count();
    assert_eq!(notes, 1, "only the earlier note remains in history");
    host.shutdown().await;
}

#[tokio::test]
async fn background_shell_wakes_agent_with_result() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = last_user(req);
        if is_tool_results(req) {
            return LlmResponse::text("done");
        }
        if last.contains("[A background command just completed]") {
            assert!(last.contains("Exit code: 0"), "{last}");
            assert!(last.contains("Full output:"), "{last}");
            return send_message("Your job finished.");
        }
        send_message("Starting it in the background.")
            .and_tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 0.3; echo hello-from-bg", "block_until_ms": 0}))
    });
    let (host, dir) = open_host(mock.clone(), |_| {}).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Bg", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "run the slow thing in the background", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1, "the user turn returned before the job ended");
    let started = wait_for(&mut rx, |e| matches!(e, HostEvent::BackgroundJobStarted { .. }), Duration::from_secs(2)).await;
    assert!(started.is_some());
    let finished = wait_for(&mut rx, |e| matches!(e, HostEvent::BackgroundJobFinished { exit_code: 0, .. }), Duration::from_secs(5)).await;
    assert!(finished.is_some());
    let woke = wait_for(
        &mut rx,
        |e| matches!(e, HostEvent::RunStarted { source: RunSource::Background, lane: Lane::Automation, .. }),
        Duration::from_secs(3),
    )
    .await;
    assert!(woke.is_some());
    let delivered = wait_for(
        &mut rx,
        |e| matches!(e, HostEvent::SendMessage { message, .. } if message.content.as_deref() == Some("Your job finished.")),
        Duration::from_secs(3),
    )
    .await;
    assert!(delivered.is_some());
    let terminals = dir.path().join("agents").join(agent.id.as_str()).join("workspace").join(".gns").join("terminals");
    assert!(std::fs::read_dir(terminals).unwrap().count() >= 1);
    host.shutdown().await;
}

#[tokio::test]
async fn webhook_routine_fires_with_payload_and_filter() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = last_user(req);
        if is_tool_results(req) {
            return LlmResponse::text("done");
        }
        if last.contains("[routine]") {
            assert!(last.contains("<github_event>"), "{last}");
            assert!(last.contains("What woke you:"), "{last}");
            assert!(last.contains("data from an outside sender"));
            return send_message("A pull request was opened.");
        }
        send_message("ok").and_tool_call(
            UPDATE_STATE_TOOL_NAME,
            json!({"target":"routine","action":"create","name":"PR watch","prompt":"Tell the user about new pull requests.",
                   "trigger":{"type":"github","repo":"o/n","events":["pr-opened"]}}),
        )
    });
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Watcher", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "watch pull requests on o/n", vec![]).await.unwrap();
    let routines = host.routines(agent.id.as_str()).unwrap();
    assert_eq!(routines.len(), 1);
    assert!(matches!(&routines[0].trigger, gns_core::routine::Trigger::Github { repo, .. } if repo == "o/n"));
    assert!(routines[0].next_run_at.is_none());
    // Filtered out: other repo.
    assert_eq!(host.fire_webhook("github", json!({"repo":"o/other","kind":"pr-opened","actor":"shihua","title":"x"})).unwrap(), 0);
    // Unknown event kind.
    assert_eq!(host.fire_webhook("github", json!({"repo":"o/n","kind":"issue-closed","actor":"shihua","title":"x"})).unwrap(), 0);
    // Match.
    assert_eq!(host.fire_webhook("github", json!({"repo":"o/n","kind":"pr-opened","actor":"shihua","title":"x"})).unwrap(), 1);
    let delivered = wait_for(
        &mut rx,
        |e| matches!(e, HostEvent::SendMessage { message, .. } if message.content.as_deref() == Some("A pull request was opened.")),
        Duration::from_secs(5),
    )
    .await;
    assert!(delivered.is_some());
    tokio::time::sleep(Duration::from_millis(300)).await;
    let record = &host.routines(agent.id.as_str()).unwrap()[0];
    assert_eq!(record.runs[0].trigger, "event");
    assert_eq!(record.runs[0].status, gns_core::routine::AutomationRunStatus::Ok, "runs: {:?}", record.runs);
    host.shutdown().await;
}

#[tokio::test]
async fn memory_extraction_records_facts_after_a_turn() {
    let mock = MockLlm::new().with_responder(|req| {
        if req.system.contains("You maintain the long-term memory") {
            assert!(req.messages.iter().any(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("User: I switched to pnpm"))));
            return LlmResponse::text("profile: The user uses pnpm\nnote: Migrating a monorepo this week\nremove: The user uses npm");
        }
        if is_tool_results(req) {
            return LlmResponse::text("done");
        }
        send_message("Noted!")
    });
    let (host, _dir) = open_host(mock.clone(), |c| {
        c.memory_extraction = true;
        c.disable_memory_freeze = true;
    })
    .await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Mem", "")).await.unwrap();
    // Seed a fact the extraction will supersede.
    let seeded = host.memory_write(agent.id.as_str(), "The user uses npm", gns_core::memory::MemoryTier::Profile).unwrap();
    assert!(seeded.starts_with("Remembered in your memory"), "{seeded}");
    host.send_user_message(agent.id.as_str(), "I switched to pnpm for everything, by the way", vec![]).await.unwrap();
    let written =
        wait_for(&mut rx, |e| matches!(e, HostEvent::MemoryWritten { fact, .. } if fact == "The user uses pnpm"), Duration::from_secs(3))
            .await;
    assert!(written.is_some());
    tokio::time::sleep(Duration::from_millis(200)).await;
    let recall = host.memory_recall(agent.id.as_str()).unwrap();
    let profile: Vec<&str> = recall.agent.profile.iter().map(|r| r.content.as_str()).collect();
    assert_eq!(profile, vec!["The user uses pnpm"]);
    assert!(recall.agent.recent.iter().any(|r| r.content == "[note] Migrating a monorepo this week"));
    assert!(host.system_prompt(agent.id.as_str()).unwrap().contains("The user uses pnpm"));
    host.shutdown().await;
}

#[tokio::test]
async fn dreaming_promotes_and_prunes() {
    let mock =
        MockLlm::new().with_responder(|req| {
            if req.system.starts_with("You consolidate an assistant's memory") {
                assert!(req.messages.iter().any(
                    |m| matches!(m, LlmMessage::User { text, .. } if text.contains("Recent log:") && text.contains("Prefers dark mode"))
                ));
                return LlmResponse::text(
                    "profile: The user prefers dark mode everywhere\nremove: Tried light mode once\nremove: Prefers dark mode",
                );
            }
            LlmResponse::text("idle")
        });
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Dreamer", "")).await.unwrap();
    for fact in ["Prefers dark mode", "Tried light mode once", "Uses a mechanical keyboard"] {
        host.memory_write(agent.id.as_str(), fact, gns_core::memory::MemoryTier::Log).unwrap();
    }
    let (added, removed) = host.dream(agent.id.as_str()).await.unwrap();
    assert_eq!((added, removed), (1, 2));
    let recall = host.memory_recall(agent.id.as_str()).unwrap();
    assert_eq!(recall.agent.profile.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), vec!["The user prefers dark mode everywhere"]);
    assert_eq!(recall.agent.recent.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), vec!["Uses a mechanical keyboard"]);
    let dreamed = wait_for(&mut rx, |e| matches!(e, HostEvent::MemoryDreamed { added: 1, removed: 2, .. }), Duration::from_secs(1)).await;
    assert!(dreamed.is_some());
    host.shutdown().await;
}
