//! End-to-end behaviour with a scripted LLM and a temporary root.

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

async fn open_host(mock: MockLlm) -> (AgentHost, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.scheduler_tick = Duration::from_millis(200);
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config, Arc::new(mock)).await.unwrap();
    (host, dir)
}

#[tokio::test]
async fn relative_host_root_gives_shell_an_absolute_workspace() {
    let current_dir = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir_in(&current_dir).unwrap();
    let relative_root = dir.path().strip_prefix(&current_dir).unwrap();
    let mut config = AgentHostConfig::new(relative_root);
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let mock = MockLlm::scripted(vec![
        send_message("Checking the workspace."),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "pwd"})),
        LlmResponse::text("done"),
    ]);
    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    let agent = host.create_agent(AgentSpec::new("Coder", "checks paths")).await.unwrap();
    let workspace = host.agent_handle(agent.id.as_str()).unwrap().workspace_dir.clone();
    assert!(workspace.is_absolute());
    assert_eq!(workspace, dir.path().join("agents").join(agent.id.as_str()).join("workspace"));

    host.send_user_message(agent.id.as_str(), "check the workspace", vec![]).await.unwrap();
    let requests = mock.requests();
    assert!(requests.iter().any(|request| request.messages.iter().any(|message| {
        matches!(message, LlmMessage::ToolResults(results) if results.iter().any(|result| {
            result.content.contains(&format!("Current directory: {}", workspace.display()))
                && result.content.contains(&format!("\n{}\n", workspace.display()))
                && result.content.contains("Exit code: 0")
        }))
    })));
}

async fn collect_until<F: Fn(&HostEvent) -> bool>(rx: &mut broadcast::Receiver<HostEvent>, pred: F, timeout: Duration) -> Vec<HostEvent> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                let done = pred(&event);
                out.push(event);
                if done {
                    break;
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            _ => break,
        }
    }
    out
}

#[tokio::test]
async fn user_turn_runs_tool_then_send_message() {
    let mock = MockLlm::scripted(vec![
        send_message("On it."),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf \"print('hi')\\n\" > hello.py"})),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "cat hello.py"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type": "text", "content": "Wrote hello.py"})),
        LlmResponse::text("done"),
    ]);
    let (host, dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Coder", "writes files")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "write hello.py", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    assert!(!result.aborted);
    assert_eq!(result.steps, 4);
    assert!(dir.path().join("agents").join(agent.id.as_str()).join("workspace").join("hello.py").exists());

    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    let sent: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::SendMessage { message, .. } => message.content.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(sent, vec!["On it.", "Wrote hello.py"]);

    // Shell saw the file content and the transcript has the tool calls.
    let requests = mock.requests();
    let last = requests.last().unwrap();
    assert!(matches!(&last.messages.last().unwrap(), LlmMessage::ToolResults(r) if r.iter().any(|t| t.content.contains("print('hi')"))));
    assert!(last.system.contains("SendMessage is your only voice"));
    assert!(last.system.contains("Your agent name is \"Coder\""));
    let transcript = host.transcript(agent.id.as_str(), 50).unwrap();
    assert!(transcript.iter().filter(|e| matches!(e, TranscriptEntry::ToolCall { .. })).count() >= 4);
    host.shutdown().await;
}

#[tokio::test]
async fn silent_visible_turn_gets_one_reminder() {
    let mock = MockLlm::scripted(vec![LlmResponse::text("thinking only"), send_message("here you go")]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "hello there", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    assert_eq!(mock.call_count(), 3, "text, REPLY_NUDGE turn with SendMessage, closing step");
    let second = &mock.requests()[1];
    assert!(matches!(second.messages.last().unwrap(), LlmMessage::User { text, .. } if text.contains("left the user without the result")));
    host.shutdown().await;
}

#[tokio::test]
async fn json_message_text_is_recovered_by_a_native_send_message_call() {
    // Replay Coder's failure: the model prints SendMessage arguments even
    // though the native tool is available. A nudge must require that tool.
    let raw = r#"{"type":"text","content":"好的，我来写一个 ChatGPT 登录页面。"}"#;
    let mock = MockLlm::scripted(vec![
        LlmResponse::text(raw),
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text"})), // invalid: still owes delivery
        send_message("好的，我来写一个 ChatGPT 登录页面。"),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "写一个 ChatGPT 登录页面", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    assert!(result.error.is_none());
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    for request in &requests[..3] {
        assert_eq!(request.options.required_tool.as_deref(), Some(SEND_MESSAGE_TOOL_NAME));
    }
    assert_eq!(requests[3].options.required_tool, None, "release tool choice after successful delivery");
    assert!(
        matches!(requests[1].messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("left the user without the result"))
    );
    let entries = host.transcript(agent.id.as_str(), 50).unwrap();
    assert!(entries.iter().any(|e| matches!(e, TranscriptEntry::AssistantText { content, .. } if content == raw)));
    assert_eq!(entries.iter().filter(|e| matches!(e, TranscriptEntry::SendMessage { .. })).count(), 1);
    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    assert_eq!(events.iter().filter(|e| matches!(e, HostEvent::SendMessage { .. })).count(), 1);
    host.shutdown().await;
}

#[tokio::test]
async fn native_reply_requirement_respects_ack_opt_out_and_quiet_turns() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    config.enforce_start_of_turn_ack = false;
    let mock = MockLlm::scripted(vec![LlmResponse::text("private"), send_message("hello"), LlmResponse::text("done")]);
    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    let agent = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "hello", vec![]).await.unwrap();
    assert_eq!(mock.requests()[0].options.required_tool, None);
    assert_eq!(mock.requests()[1].options.required_tool.as_deref(), Some(SEND_MESSAGE_TOOL_NAME));
    assert_eq!(mock.requests()[2].options.required_tool, None);

    let mut rx = host.subscribe();
    host.enqueue(agent.id.as_str(), gns_runtime::RunJob::hidden("nothing to report", RunOptions::automation())).unwrap();
    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    assert!(
        events.iter().any(|e| matches!(e, HostEvent::TurnEnded { result, .. } if result.error.is_none() && result.sent_message_count == 0))
    );
    assert_eq!(mock.requests().last().unwrap().options.required_tool, None);
    assert_eq!(mock.call_count(), 4, "quiet turns need no reply nudge");
    host.shutdown().await;
}

#[tokio::test]
async fn path_escape_is_denied_and_reported_to_model() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(READ_TOOL_NAME, json!({"path": "/etc/hosts"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type": "text", "content": "tried"})),
        LlmResponse::text("ok"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "read /etc/hosts please", vec![]).await.unwrap();
    let last = mock.requests().last().unwrap().clone();
    assert!(matches!(last.messages.last().unwrap(), LlmMessage::ToolResults(r) if r[0].content.contains("outside your directories")));
    host.shutdown().await;
}

#[tokio::test]
async fn a2a_wakes_recipient_hidden_and_allows_silence() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = req
            .messages
            .last()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if last.contains("[agent]") {
            // Recipient: stays silent (no SendMessage, no reply).
            LlmResponse::text("noted, nothing to add")
        } else if last.contains("tell Bob") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"pinging Bob"}))
        } else if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else {
            LlmResponse::text("?")
        }
    });
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let alice = host.create_agent(AgentSpec::new("Alice", "")).await.unwrap();
    let bob = host.create_agent(AgentSpec::new("Bob", "")).await.unwrap();
    // Alice's second step sends to Bob.
    mock.push_all(vec![
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"pinging Bob"})),
        LlmResponse::tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": bob.id.as_str(), "message": "please check the build"})),
        LlmResponse::text("sent"),
    ]);
    host.send_user_message(alice.id.as_str(), "tell Bob to check the build", vec![]).await.unwrap();
    let events =
        collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { agent_id, .. } if agent_id == &bob.id), Duration::from_secs(3)).await;
    assert!(events.iter().any(|e| matches!(e, HostEvent::A2ASent { to, .. } if to == bob.id.as_str())));
    let bob_turn = events
        .iter()
        .find_map(|e| match e {
            HostEvent::TurnEnded { agent_id, result, .. } if agent_id == &bob.id => Some(result.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(bob_turn.sent_message_count, 0, "silence allowed on agent wakes");
    assert!(
        events.iter().any(
            |e| matches!(e, HostEvent::RunStarted { agent_id, lane: Lane::Agent, source: RunSource::Agent, .. } if agent_id == &bob.id)
        )
    );
    // Bob's prompt carried the wake cue; Alice's transcript has the outbound entry.
    let bob_request = mock
        .requests()
        .into_iter()
        .find(|r| {
            r.messages.iter().any(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("[agent]") && text.contains("Alice")))
        })
        .expect("bob was woken with the [agent] cue");
    assert!(bob_request.system.contains("Your agent name is \"Bob\""));
    let alice_transcript = host.transcript(alice.id.as_str(), 50).unwrap();
    assert!(alice_transcript.iter().any(|e| matches!(e, TranscriptEntry::Message { to_agent: Some(t), .. } if t.name == "Bob")));
    let bob_transcript = host.transcript(bob.id.as_str(), 50).unwrap();
    assert!(bob_transcript.iter().any(|e| matches!(e, TranscriptEntry::Message { from_agent: Some(f), .. } if f.name == "Alice")));
    host.shutdown().await;
}

#[tokio::test]
async fn a2a_validation_messages() {
    let mock = MockLlm::new();
    let (host, _dir) = open_host(mock.clone()).await;
    let alice = host.create_agent(AgentSpec::new("Alice", "")).await.unwrap();
    let a = alice.id.clone();
    mock.push_all(vec![
        LlmResponse::tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": a.as_str(), "message": "hi me"}))
            .and_tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": "nope", "message": "hi"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ok"})),
        LlmResponse::text("done"),
    ]);
    host.send_user_message(a.as_str(), "test validation", vec![]).await.unwrap();
    let last = mock.requests().last().unwrap().clone();
    let LlmMessage::ToolResults(results) = last.messages.last().unwrap() else { panic!() };
    assert!(results[0].content.contains("can't message yourself"));
    assert!(results[1].content.contains("No agent found with id nope"));
    host.shutdown().await;
}

#[tokio::test]
async fn priority_message_interrupts_agent_lane_but_user_message_is_never_preempted() {
    fn last_user(req: &LlmRequest) -> String {
        req.messages
            .last()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default()
    }
    let mut mock = MockLlm::new().with_responder(|req| {
        let last = last_user(req);
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if last.contains("[routine]") {
            LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 3"}))
        } else if let Some((_, id)) = last.split_once("stop bob id=") {
            let id = id.split_whitespace().next().unwrap_or_default();
            LlmResponse::tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": id, "message": "STOP", "priority": true}))
                .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"sent"}))
        } else {
            LlmResponse::text("quiet")
        }
    });
    mock.delay = Some(Duration::from_millis(200));
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let alice = host.create_agent(AgentSpec::new("Alice", "")).await.unwrap();
    let bob = host.create_agent(AgentSpec::new("Bob", "")).await.unwrap();

    // Bob works on the automation lane (blocked in a slow Shell); Alice sends a priority message.
    host.enqueue(bob.id.as_str(), gns_runtime::RunJob::hidden("[routine] slow check", RunOptions::automation())).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.send_user_message(alice.id.as_str(), format!("stop bob id={}", bob.id), vec![]).await.unwrap();
    let events = collect_until(
        &mut rx,
        |e| matches!(e, HostEvent::TurnEnded { agent_id, result, .. } if agent_id == &bob.id && !result.aborted),
        Duration::from_secs(6),
    )
    .await;
    assert!(events.iter().any(|e| matches!(e, HostEvent::Interrupted { agent_id, reason, .. } if agent_id == &bob.id && reason.contains("priority agent message"))), "automation turn was interrupted: {events:?}");
    assert!(events.iter().any(|e| matches!(e, HostEvent::TurnEnded { agent_id, result, .. } if agent_id == &bob.id && result.aborted)));
    assert!(
        events.iter().any(|e| matches!(e, HostEvent::RunStarted { agent_id, lane: Lane::Agent, .. } if agent_id == &bob.id)),
        "priority wake ran"
    );

    // Now Bob is in a user turn; a priority message must NOT interrupt it.
    let bob_id = bob.id.clone();
    let host2 = host.clone();
    let user_turn = tokio::spawn(async move { host2.send_user_message(bob_id.as_str(), "slow user question", vec![]).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.send_user_message(alice.id.as_str(), format!("stop bob id={}", bob.id), vec![]).await.unwrap();
    let result = user_turn.await.unwrap();
    assert!(!result.aborted, "user lane is never preempted");
    host.shutdown().await;
}

#[tokio::test]
async fn group_round_only_mentioned_member_speaks_and_pass_is_dropped() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = req
            .messages
            .last()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if req.system.contains("You are Bob, one participant") && last.contains("@Bob explain") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"Bob here: hello.py prints hi"}))
        } else if req.system.contains("one participant in a group chat") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"(pass)"}))
        } else {
            LlmResponse::text("idle")
        }
    });
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let alice = host.create_agent(AgentSpec::new("Alice", "planner")).await.unwrap();
    let bob = host.create_agent(AgentSpec::new("Bob", "coder")).await.unwrap();
    let group = host
        .create_group(GroupSpec {
            name: "Review".into(),
            description: "code review room".into(),
            members: vec![alice.id.clone(), bob.id.clone()],
        })
        .await
        .unwrap();
    assert_eq!(group.members.len(), 2);
    host.post_to_group_as_user(group.id.as_str(), "@Bob explain hello.py").await.unwrap();
    let events =
        collect_until(&mut rx, |e| matches!(e, HostEvent::GroupPosted { speaker, .. } if speaker == "Bob"), Duration::from_secs(3)).await;
    let bob_posts = events.iter().filter(|e| matches!(e, HostEvent::GroupPosted { speaker, .. } if speaker == "Bob")).count();
    assert_eq!(bob_posts, 1);
    // Only Bob was asked to speak: no room turn for Alice.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let alice_room_turns = mock.requests().iter().filter(|r| r.system.contains("You are Alice, one participant")).count();
    assert_eq!(alice_room_turns, 0);
    let history = host.group_history(group.id.as_str()).unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert!(!history.iter().any(|m| m.content.contains("(pass)")));

    // Unaddressed post: everyone is asked; Alice passes (dropped), Bob speaks; round ends when nobody adds more.
    host.post_to_group_as_user(group.id.as_str(), "anything else?").await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let alice_room_turns = mock.requests().iter().filter(|r| r.system.contains("You are Alice, one participant")).count();
    assert!(alice_room_turns >= 1);
    let history = host.group_history(group.id.as_str()).unwrap();
    assert!(history.iter().all(|m| !m.content.contains("(pass)")));
    // Other members' posts live only in the room; the member's own room
    // turns are persisted with the group tag.
    let bob_transcript = host.transcript(bob.id.as_str(), 50).unwrap();
    assert!(!bob_transcript.iter().any(|e| matches!(e, TranscriptEntry::Message { group_id: Some(_), run_id: None, .. })));
    assert!(bob_transcript.iter().any(|e| matches!(e, TranscriptEntry::Message { group_id: Some(_), run_id: Some(_), hidden: true, content, .. } if content.starts_with("[Group chat: \"Review\""))));
    assert!(bob_transcript.iter().any(|e| matches!(e, TranscriptEntry::SendMessage { group_id: Some(_), message, .. } if message.content.as_deref() == Some("Bob here: hello.py prints hi"))));
    host.shutdown().await;
}

#[tokio::test]
async fn group_turns_enter_member_history_once() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = req
            .messages
            .last()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if req.system.contains("one participant in a group chat") && last.contains("No new messages") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"(pass)"}))
        } else if req.system.contains("one participant in a group chat") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"Bob in the room"}))
        } else if last.contains("what did you say") {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"I said it in the room"}))
        } else {
            LlmResponse::text("idle")
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    config.user_name = Some("Shihua".into());
    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    let mut rx = host.subscribe();
    let bob = host.create_agent(AgentSpec::new("Bob", "coder")).await.unwrap();
    let group =
        host.create_group(GroupSpec { name: "Review".into(), description: String::new(), members: vec![bob.id.clone()] }).await.unwrap();
    host.post_to_group_as_user(group.id.as_str(), "@Bob please summarize hello.py").await.unwrap();
    collect_until(&mut rx, |e| matches!(e, HostEvent::GroupPosted { speaker, .. } if speaker == "Bob"), Duration::from_secs(3)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The local user renders as `User:` in the room (no display name).
    let history = host.group_history(group.id.as_str()).unwrap();
    assert!(matches!(&history[0].speaker, gns_core::groups::Speaker::User { name: None }), "{history:?}");
    let room_prompt = mock.requests().into_iter().find(|r| r.system.contains("one participant in a group chat")).unwrap();
    assert!(matches!(&room_prompt.messages[0], LlmMessage::User { text, .. } if text.contains("User: @Bob please summarize hello.py")));
    // The room prompt is layered on top of the regular sections.
    assert!(room_prompt.system.contains("Agent profile:"), "{}", room_prompt.system);

    // Bob's next 1:1 turn sees exactly one copy of the room exchange, tagged.
    host.send_user_message(bob.id.as_str(), "what did you say in the review room?", vec![]).await.unwrap();
    let dm = mock.requests().into_iter().rev().find(|r| !r.system.contains("one participant")).unwrap();
    let tagged = dm
        .messages
        .iter()
        .filter(
            |m| matches!(m, LlmMessage::User { text, .. } if text.contains("[Group chat: \"Review\"") && text.contains("please summarize")),
        )
        .count();
    assert_eq!(tagged, 1, "the room turn that carried the question appears once: {:?}", dm.messages);
    let mentions =
        dm.messages.iter().filter(|m| matches!(m, LlmMessage::User { text, .. } if text.contains("please summarize hello.py"))).count();
    assert_eq!(mentions, 1, "mirrored room post is not duplicated into context");
    assert!(dm.messages.iter().any(|m| matches!(m, LlmMessage::Assistant { tool_calls, .. } if tool_calls.iter().any(|c| c.name == SEND_MESSAGE_TOOL_NAME && c.arguments["content"] == "Bob in the room"))));
    host.shutdown().await;
}

#[tokio::test]
async fn memory_write_shows_up_and_the_block_freezes_per_compaction_epoch() {
    let mock = MockLlm::new().with_responder(|req| {
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ok"}))
        }
    });
    let (host, dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("Mem", "")).await.unwrap();
    let before = host.system_prompt(agent.id.as_str()).unwrap();
    let again = host.system_prompt(agent.id.as_str()).unwrap();
    assert_eq!(before, again, "snapshot reused while nothing changed");
    mock.push_all(vec![
        LlmResponse::tool_call(
            UPDATE_STATE_TOOL_NAME,
            json!({"target":"memory","action":"write","fact":"The user prefers pnpm over npm","tier":"profile"}),
        )
        .and_tool_call(UPDATE_STATE_TOOL_NAME, json!({"target":"memory","action":"write","fact":"Likes concise replies","scope":"user"}))
        .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"remembered"})),
        LlmResponse::text("done"),
    ]);
    host.send_user_message(agent.id.as_str(), "remember that I prefer pnpm", vec![]).await.unwrap();
    let after = host.system_prompt(agent.id.as_str()).unwrap();
    assert!(after.contains("The user prefers pnpm over npm"));
    assert!(after.contains("[via Mem] Likes concise replies"), "{after}");
    assert!(dir.path().join("agents").join(agent.id.as_str()).join("memory").join("profile.md").exists());
    assert!(dir.path().join("user-memory").join("agents").join(agent.id.as_str()).join("log").exists());
    let recall = host.memory_recall(agent.id.as_str()).unwrap();
    assert_eq!(recall.agent.profile.len(), 1);
    assert_eq!(recall.user_shards.len(), 1);

    // Forget removes it.
    mock.push_all(vec![
        LlmResponse::tool_call(
            UPDATE_STATE_TOOL_NAME,
            json!({"target":"memory","action":"forget","fact":"The user prefers pnpm over npm"}),
        )
        .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"forgot"})),
        LlmResponse::text("done"),
    ]);
    host.send_user_message(agent.id.as_str(), "forget the pnpm thing", vec![]).await.unwrap();
    let recall = host.memory_recall(agent.id.as_str()).unwrap();
    assert!(recall.agent.profile.is_empty());
    // The rendered block stays frozen until the compaction epoch advances.
    assert!(host.system_prompt(agent.id.as_str()).unwrap().contains("prefers pnpm"));
    host.agent_handle(agent.id.as_str()).unwrap().db.set_json(kv_keys::COMPACTION_EPOCH, &1u64).unwrap();
    assert!(!host.system_prompt(agent.id.as_str()).unwrap().contains("prefers pnpm"));
    host.shutdown().await;
}

#[tokio::test]
async fn profile_update_is_announced_and_folded_after_compaction() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(UPDATE_STATE_TOOL_NAME, json!({"target":"profile","action":"set","name":"Nova"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"renamed"})),
        LlmResponse::text("done"),
        send_message("still here"),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("Old", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "rename yourself to Nova", vec![]).await.unwrap();
    assert_eq!(host.find_agent(agent.id.as_str()).unwrap().name, "Nova");
    assert!(host.find_agent("Nova").is_some());
    // The profile section is frozen for the compaction epoch; the change is
    // announced in the next turn's prompt instead.
    assert!(host.system_prompt(agent.id.as_str()).unwrap().contains("Your agent name is \"Old\""));
    host.send_user_message(agent.id.as_str(), "who are you now?", vec![]).await.unwrap();
    let last = mock.requests().into_iter().rev().find(|r| !matches!(r.messages.last(), Some(LlmMessage::ToolResults(_)))).unwrap();
    assert!(
        matches!(last.messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("<agent_profile_update>") && text.contains("Current name: Nova")),
        "{:?}",
        last.messages.last()
    );
    host.agent_handle(agent.id.as_str()).unwrap().db.set_json(kv_keys::COMPACTION_EPOCH, &1u64).unwrap();
    assert!(host.system_prompt(agent.id.as_str()).unwrap().contains("Your agent name is \"Nova\""));
    host.shutdown().await;
}

#[tokio::test]
async fn routine_fires_on_automation_lane_and_records_runs() {
    let mock = MockLlm::new().with_responder(|req| {
        let last = req
            .messages
            .last()
            .map(|m| match m {
                LlmMessage::User { text, .. } => text.clone(),
                _ => String::new(),
            })
            .unwrap_or_default();
        if last.starts_with("[routine]") {
            LlmResponse::text("nothing changed, staying quiet")
        } else if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else {
            LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ok"}))
        }
    });
    let (host, dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Watcher", "")).await.unwrap();
    mock.push_all(vec![
        LlmResponse::tool_call(UPDATE_STATE_TOOL_NAME, json!({"target":"routine","action":"create","name":"Workspace check","prompt":"Count files in the workspace; stay quiet if unchanged.","schedule":"@every 1s"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"set up"})),
        LlmResponse::text("done"),
    ]);
    host.send_user_message(agent.id.as_str(), "check my workspace every second", vec![]).await.unwrap();
    let routines = host.routines(agent.id.as_str()).unwrap();
    assert_eq!(routines.len(), 1);
    assert_eq!(routines[0].id, "workspace-check");
    assert!(dir.path().join("agents").join(agent.id.as_str()).join("automations").join("workspace-check").join("automation.json").exists());

    let events = collect_until(
        &mut rx,
        |e| matches!(e, HostEvent::TurnEnded { result, .. } if result.steps > 0 && result.sent_message_count == 0),
        Duration::from_secs(5),
    )
    .await;
    assert!(events.iter().any(|e| matches!(e, HostEvent::RoutineFired { automation_id, .. } if automation_id == "workspace-check")));
    assert!(events.iter().any(|e| matches!(e, HostEvent::RunStarted { lane: Lane::Automation, source: RunSource::Automation, .. })));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let record = &host.routines(agent.id.as_str()).unwrap()[0];
    assert!(!record.runs.is_empty());
    assert!(record.runs.iter().any(|r| r.status == gns_core::routine::AutomationRunStatus::Ok));

    // Pause stops firing.
    mock.push_all(vec![
        LlmResponse::tool_call(UPDATE_STATE_TOOL_NAME, json!({"target":"routine","action":"pause","id":"workspace-check"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"paused"})),
        LlmResponse::text("done"),
    ]);
    host.send_user_message(agent.id.as_str(), "pause that routine", vec![]).await.unwrap();
    assert!(!host.routines(agent.id.as_str()).unwrap()[0].is_enabled);
    host.shutdown().await;
}

#[tokio::test]
async fn widget_ends_turn_and_answer_resumes() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(
            SEND_MESSAGE_TOOL_NAME,
            json!({"type":"widget","widget":{"prompt":"Which one?","options":[{"label":"A"},{"label":"B"}]}}),
        ),
        LlmResponse::text("should not be reached in the same turn"),
        send_message("Great, going with A"),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("Q", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "pick something for me", vec![]).await.unwrap();
    assert!(result.awaiting_user_selection);
    assert_eq!(result.steps, 1);
    assert!(host.is_awaiting_user(agent.id.as_str()));
    let result = host.answer_widget(agent.id.as_str(), "A").await.unwrap();
    assert!(!host.is_awaiting_user(agent.id.as_str()));
    assert_eq!(result.sent_message_count, 1);
    host.shutdown().await;
}

#[tokio::test]
async fn reopening_host_recovers_agents_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockLlm::scripted(vec![send_message("hi"), LlmResponse::text("done")]);
    let mut config = AgentHostConfig::new(dir.path());
    config.kickstart_new_agents = false;
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config.clone(), Arc::new(mock.clone())).await.unwrap();
    let agent = host.create_agent(AgentSpec::new("Persist", "keeps state")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "hello persistence", vec![]).await.unwrap();
    host.shutdown().await;

    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    let found = host.find_agent("Persist").unwrap();
    assert_eq!(found.id, agent.id);
    let transcript = host.transcript(agent.id.as_str(), 50).unwrap();
    assert!(transcript.iter().any(|e| matches!(e, TranscriptEntry::Message { content, .. } if content == "hello persistence")));
    mock.push_all(vec![send_message("again"), LlmResponse::text("done")]);
    host.send_user_message(agent.id.as_str(), "second", vec![]).await.unwrap();
    let last = mock.requests().last().unwrap().clone();
    let users = last.messages.iter().filter(|m| matches!(m, LlmMessage::User { .. })).count();
    assert!(users >= 2, "history was rebuilt from the database");
    host.shutdown().await;
}

#[tokio::test]
async fn custom_tool_and_prompt_section_are_pluggable() {
    #[derive(Debug)]
    struct Echo;
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct EchoArgs {
        text: String,
    }
    #[async_trait]
    impl TypedTool for Echo {
        type Args = EchoArgs;
        fn name(&self) -> &str {
            "Echo"
        }
        fn description(&self) -> &str {
            "echo"
        }
        async fn run(&self, _ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(format!("echo: {}", args.text)))
        }
    }
    #[derive(Debug)]
    struct Motto;
    impl gns_core::prompt::PromptSection for Motto {
        fn id(&self) -> &str {
            "motto"
        }
        fn render(&self, _ctx: &gns_core::prompt::PromptContext) -> Option<String> {
            Some("Motto: ship it.".to_owned())
        }
    }
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call("Echo", json!({"text": "ping"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ok"})),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    host.register_tool(Typed::arc(Echo));
    host.add_prompt_section(Arc::new(Motto), gns_core::prompt::SectionPosition::After(gns_core::prompt::section_ids::PROFILE.into()));
    let agent = host.create_agent(AgentSpec::new("X", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "use echo", vec![]).await.unwrap();
    let first = mock.requests()[0].clone();
    assert!(first.tools.iter().any(|t| t.name == "Echo" && t.parameters["properties"]["text"].is_object()));
    let profile_pos = first.system.find("Agent profile:").unwrap();
    let motto_pos = first.system.find("Motto: ship it.").unwrap();
    assert!(motto_pos > profile_pos);
    let last = mock.requests().last().unwrap().clone();
    assert!(matches!(last.messages.last().unwrap(), LlmMessage::ToolResults(r) if r[0].content == "echo: ping"));
    host.shutdown().await;
}

#[tokio::test]
async fn repeated_send_message_is_delivered_each_time() {
    // The original host has no duplicate guard: every SendMessage call is delivered.
    let greeting = "你好！我是 Coder。有什么我可以帮你的吗？";
    let mock = MockLlm::scripted(vec![send_message(greeting), send_message(greeting), LlmResponse::text("done")]);
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("Coder", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "你好", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    assert_eq!(result.steps, 3);
    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    let sent = events.iter().filter(|e| matches!(e, HostEvent::SendMessage { .. })).count();
    assert_eq!(sent, 2);
    // The tool result carries the message address.
    let requests = mock.requests();
    assert!(
        matches!(requests[1].messages.last().unwrap(), LlmMessage::ToolResults(t) if t[0].content.starts_with("Message sent to user. (id: t0s0)"))
    );
    host.shutdown().await;
}

#[tokio::test]
async fn distinct_follow_up_messages_still_go_through() {
    let mock = MockLlm::scripted(vec![send_message("On it."), send_message("Here is the result."), LlmResponse::text("done")]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "hi", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    assert_eq!(result.steps, 3);
    host.shutdown().await;
}

#[tokio::test]
async fn preempted_inbound_wake_redelivers_without_losing_queued_messages() {
    fn last_user(req: &LlmRequest) -> String {
        match req.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.clone(),
            _ => String::new(),
        }
    }
    let mock = MockLlm::new().with_responder(|req| {
        let last = last_user(req);
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if last.contains("[agent]") && last.contains("second") {
            LlmResponse::text("quiet")
        } else if last.contains("[agent]") {
            LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 2"}))
        } else if let Some((_, id)) = last.split_once("send two id=") {
            let id = id.split_whitespace().next().unwrap_or_default();
            LlmResponse::tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": id, "message": "first task"}))
                .and_tool_call(SEND_TO_AGENT_TOOL_NAME, json!({"target_id": id, "message": "second task"}))
                .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"sent both"}))
        } else {
            send_message("hi")
        }
    });
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let alice = host.create_agent(AgentSpec::new("Alice", "")).await.unwrap();
    let bob = host.create_agent(AgentSpec::new("Bob", "")).await.unwrap();
    host.send_user_message(alice.id.as_str(), format!("send two id={}", bob.id), vec![]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await; // Bob is inside the slow Shell for "first task"
    host.send_user_message(bob.id.as_str(), "hello bob", vec![]).await.unwrap();

    // Wait until Bob has finished the redriven "first task" wake, which is
    // the last one in the queue after "second task".
    let finished = std::sync::atomic::AtomicUsize::new(0);
    let events = collect_until(
        &mut rx,
        |e| {
            if matches!(e, HostEvent::TurnEnded { agent_id, result, .. } if agent_id == &bob.id && !result.aborted && result.sent_message_count == 0) {
                return finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 >= 2;
            }
            false
        },
        Duration::from_secs(8),
    )
    .await;
    let bob_agent_runs =
        events.iter().filter(|e| matches!(e, HostEvent::RunStarted { agent_id, lane: Lane::Agent, .. } if agent_id == &bob.id)).count();
    let lasts: Vec<String> = mock.requests().iter().map(last_user).collect();
    assert!(bob_agent_runs >= 3, "aborted + redriven + second, got {bob_agent_runs}; last user messages: {lasts:#?}");
    let quiet_wakes = events.iter().filter(|e| matches!(e, HostEvent::TurnEnded { agent_id, result, .. } if agent_id == &bob.id && !result.aborted && result.sent_message_count == 0)).count();
    assert_eq!(quiet_wakes, 2, "redriven + second both ended quietly; last user messages: {lasts:#?}");
    let transcript = host.transcript(bob.id.as_str(), 100).unwrap();
    let inbound: Vec<&str> = transcript
        .iter()
        .filter_map(|e| match e {
            TranscriptEntry::Message { from_agent: Some(f), content, .. } if f.id == alice.id => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(inbound, vec!["first task", "second task"], "both messages persisted exactly once");
    host.shutdown().await;
}

#[tokio::test]
async fn reply_nudge_runs_after_a_silent_user_turn_and_stops_at_the_cap() {
    // Model ends without a reply, then answers the first post-turn nudge.
    let mock = MockLlm::scripted(vec![
        LlmResponse::text("thinking"),
        send_message("here is the answer"), // hidden REPLY_NUDGE turn
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "what is 2+2?", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    let nudges = mock
        .requests()
        .iter()
        .filter(|r| matches!(r.messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("left the user without the result")))
        .count();
    assert_eq!(nudges, 1, "one nudge was enough");

    // A model that never replies is nudged MAX_REPLY_NUDGES times, then the turn ends.
    let mock2 = MockLlm::new().with_responder(|_| LlmResponse::text("never"));
    let (host2, _dir2) = open_host(mock2.clone()).await;
    let mut rx2 = host2.subscribe();
    let agent2 = host2.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let result2 = host2.send_user_message(agent2.id.as_str(), "hello?", vec![]).await.unwrap();
    assert_eq!(result2.sent_message_count, 0);
    assert!(result2.error.as_deref().is_some_and(|e| e.contains("without delivering a reply through SendMessage")));
    let nudges2 = mock2
        .requests()
        .iter()
        .filter(|r| matches!(r.messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("left the user without the result")))
        .count();
    assert_eq!(nudges2, MAX_REPLY_NUDGES);
    let events = collect_until(&mut rx2, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    assert!(events.iter().any(|e| matches!(e, HostEvent::Error { message, .. } if Some(message) == result2.error.as_ref())));
    assert!(!events.iter().any(|e| matches!(e, HostEvent::SendMessage { .. })), "private text must never be auto-delivered");
    host.shutdown().await;
    host2.shutdown().await;
}

#[tokio::test]
async fn closing_send_nudge_after_ack_then_silent_tools() {
    let mock = MockLlm::scripted(vec![
        send_message("On it."),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf x > a.txt"})),
        LlmResponse::text(r#"{"type":"text","content":"Wrote a.txt"}"#), // private text is still not delivery
        send_message("Wrote a.txt"),                                     // CLOSING_SEND_NUDGE turn
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "write a.txt", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 2);
    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(2)).await;
    let sent: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            HostEvent::SendMessage { message, .. } => message.content.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(sent, vec!["On it.", "Wrote a.txt"]);
    assert_eq!(mock.requests()[3].options.required_tool.as_deref(), Some(SEND_MESSAGE_TOOL_NAME));
    assert_eq!(mock.requests()[4].options.required_tool, None);
    assert!(mock.requests().iter().any(|r| matches!(r.messages.last(), Some(LlmMessage::User { text, .. }) if text.contains("acknowledged the user and then ran tool calls"))));
    host.shutdown().await;
}

#[tokio::test]
async fn broadcast_runs_on_background_lane_and_does_not_preempt() {
    let mut mock = MockLlm::new().with_responder(|req| {
        let last = match req.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.clone(),
            _ => String::new(),
        };
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if last.starts_with("[routine]") {
            LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 1"}))
        } else if last.starts_with("[broadcast]") {
            send_message("got the broadcast")
        } else {
            send_message("hi")
        }
    });
    mock.delay = Some(Duration::from_millis(50));
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    host.enqueue(a.id.as_str(), gns_runtime::RunJob::hidden("[routine] slow", RunOptions::automation())).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(host.broadcast_to_agents(None, "everyone: status?").unwrap(), (1, 1));
    let events = collect_until(
        &mut rx,
        |e| matches!(e, HostEvent::SendMessage { message, .. } if message.content.as_deref() == Some("got the broadcast")),
        Duration::from_secs(5),
    )
    .await;
    assert!(!events.iter().any(|e| matches!(e, HostEvent::Interrupted { .. })), "broadcast must not preempt: {events:?}");
    assert!(events.iter().any(|e| matches!(e, HostEvent::RunStarted { source: RunSource::Broadcast, lane: Lane::Automation, .. })));
    host.shutdown().await;
}

#[tokio::test]
async fn kickstart_clears_introduction_pending_only_after_greeting() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::text("(silent first attempt)"),
        send_message("Hello! I'm new."), // hidden-turn nudge
        LlmResponse::text("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let mut config = AgentHostConfig::new(dir.path());
    config.memory_extraction = false;
    config.dream_after_idle = None;
    let host = AgentHost::open(config, Arc::new(mock.clone())).await.unwrap();
    let mut rx = host.subscribe();
    let mut spec = AgentSpec::new("K", "");
    spec.kickstart = true;
    let agent = host.create_agent(spec).await.unwrap();
    let events = collect_until(&mut rx, |e| matches!(e, HostEvent::TurnEnded { .. }), Duration::from_secs(3)).await;
    assert!(
        events.iter().any(|e| matches!(e, HostEvent::SendMessage { message, .. } if message.content.as_deref() == Some("Hello! I'm new.")))
    );
    let pending: bool = host.agent_handle(agent.id.as_str()).unwrap().db.get_json(kv_keys::INTRODUCTION_PENDING).unwrap().unwrap();
    assert!(!pending);
    assert_eq!(mock.requests()[0].options.required_tool.as_deref(), Some(SEND_MESSAGE_TOOL_NAME));
    assert_eq!(mock.requests()[1].options.required_tool.as_deref(), Some(SEND_MESSAGE_TOOL_NAME));
    assert_eq!(mock.requests()[2].options.required_tool, None);
    host.shutdown().await;
}

#[tokio::test]
async fn work_out_loud_reminders_fire_after_silent_tool_streaks() {
    // Ack, then 7 silent Shell steps: the early-result reminder fires once at
    // the first silent step, the "several tool calls" reminder from the 7th on.
    let mut script = vec![send_message("On it.")];
    for i in 0..8 {
        script.push(LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": format!("echo {i}")})));
    }
    script.push(send_message("all done"));
    script.push(LlmResponse::text("done"));
    let mock = MockLlm::scripted(script);
    let (host, _dir) = open_host(mock.clone()).await;
    let agent = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "do the thing", vec![]).await.unwrap();
    let last_texts: Vec<String> = mock
        .requests()
        .iter()
        .map(|r| match r.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.clone(),
            _ => String::new(),
        })
        .collect();
    let early: Vec<usize> = last_texts.iter().enumerate().filter(|(_, t)| t.contains("cannot see tool output")).map(|(i, _)| i).collect();
    let several: Vec<usize> =
        last_texts.iter().enumerate().filter(|(_, t)| t.contains("several tool calls without a SendMessage")).map(|(i, _)| i).collect();
    assert_eq!(early, vec![2], "once per silent streak, right after the first silent step: {last_texts:?}");
    assert_eq!(several, vec![8, 9], "after more than 6 silent calls, every step: {last_texts:?}");
    host.shutdown().await;
}

#[tokio::test]
async fn newer_user_message_supersedes_running_user_turn() {
    let mut mock = MockLlm::new().with_responder(|req| {
        let last = match req.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.clone(),
            _ => String::new(),
        };
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if last.contains("slow task") {
            send_message("starting").and_tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 3"}))
        } else {
            send_message("stopped, what next?")
        }
    });
    mock.delay = Some(Duration::from_millis(30));
    let (host, _dir) = open_host(mock.clone()).await;
    let mut rx = host.subscribe();
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let host2 = host.clone();
    let id = a.id.clone();
    let first = tokio::spawn(async move { host2.send_user_message(id.as_str(), "slow task please", vec![]).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(400)).await;
    let started = std::time::Instant::now();
    let second = host.send_user_message(a.id.as_str(), "stop that", vec![]).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(2), "the new message did not wait for the 3s shell");
    assert_eq!(second.sent_message_count, 1);
    let first = first.await.unwrap();
    assert!(first.aborted);
    let events = collect_until(&mut rx, |_| false, Duration::from_millis(200)).await;
    assert!(events.iter().any(|e| matches!(e, HostEvent::Interrupted { reason, .. } if reason == "superseded by a new user message")));
    host.shutdown().await;
}

#[tokio::test]
async fn deleting_a_busy_agent_waits_for_its_turn_and_cleans_up() {
    let mock = MockLlm::new().with_responder(|req| {
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else {
            send_message("working").and_tool_call(SHELL_TOOL_NAME, json!({"command": "sleep 5"}))
        }
    });
    let (host, dir) = open_host(mock.clone()).await;
    let a = host.create_agent(AgentSpec::new("Doomed", "")).await.unwrap();
    let host2 = host.clone();
    let id = a.id.clone();
    let turn = tokio::spawn(async move { host2.send_user_message(id.as_str(), "start something slow", vec![]).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    host.delete_agent(a.id.as_str()).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(3), "the running shell was cancelled, not waited out");
    assert!(!dir.path().join("agents").join(a.id.as_str()).exists());
    assert!(host.list_agents().iter().all(|x| x.id != a.id));
    let result = turn.await.unwrap();
    assert!(matches!(&result, Ok(r) if r.aborted) || result.is_err(), "{result:?}");
    host.shutdown().await;
}

#[tokio::test]
async fn room_rounds_are_serialized_and_new_messages_count_from_the_last_reply() {
    let mut mock = MockLlm::new().with_responder(|req| {
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            return LlmResponse::text("done");
        }
        let last = match req.messages.last() {
            Some(LlmMessage::User { text, .. }) => text.clone(),
            _ => String::new(),
        };
        if last.contains("New messages in the room") {
            let n = last.matches("post ").count();
            return LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content": format!("reply to {n} new post(s)")}));
        }
        if last.contains("No new messages in the room") {
            return LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content": "(pass)"}));
        }
        send_message("hi")
    });
    mock.delay = Some(Duration::from_millis(150));
    let (host, _dir) = open_host(mock.clone()).await;
    let a = host.create_agent(AgentSpec::new("Ann", "")).await.unwrap();
    let group =
        host.create_group(GroupSpec { name: "room".into(), description: String::new(), members: vec![a.id.clone()] }).await.unwrap();
    host.post_to_group_as_user(group.id.as_str(), "post one").await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    host.post_to_group_as_user(group.id.as_str(), "post two").await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let history = host.group_history(group.id.as_str()).unwrap();
    let replies: Vec<&str> =
        history.iter().filter(|m| matches!(m.speaker, gns_core::groups::Speaker::Member { .. })).map(|m| m.content.as_str()).collect();
    let answered: usize =
        replies.iter().map(|r| r.trim_start_matches("reply to ").split(' ').next().unwrap().parse::<usize>().unwrap_or(99)).sum();
    // The reply to post one lands after post two, so post two is not "new
    // since the member last spoke": the second round passes, as in the original.
    assert_eq!(answered, 1, "{replies:?}");
    assert_eq!(replies.len(), 1, "{replies:?}");
    host.shutdown().await;
}
