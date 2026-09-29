//! Second-batch behaviour: policies and approvals, shell isolation,
//! background compaction, deletion, kickstart ordering, routine misfires.

use gns_core::*;
use gns_llm::MockLlm;
use gns_runtime::{AgentHost, AgentHostConfig};
use gns_tools::{ApproveToolsPolicy, ShellGuardPolicy};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn send_message(text: &str) -> LlmResponse {
    LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type": "text", "content": text}))
}

fn last_tool_results(mock: &MockLlm) -> Vec<LlmToolResult> {
    let last = mock.requests().into_iter().rev().find(|r| !r.system.starts_with("Summarize this conversation"));
    match last.and_then(|r| r.messages.last().cloned()) {
        Some(LlmMessage::ToolResults(results)) => results,
        other => panic!("expected tool results, got {other:?}"),
    }
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

#[tokio::test]
async fn shell_child_does_not_see_host_secrets() {
    // HOME always exists in the host process; a Shell configured without it in
    // the allowlist must not leak it, while GNS_AGENT_ID and PATH still arrive.
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "echo \"[$HOME]\"; echo \"[$GNS_AGENT_ID]\"; echo \"[$PATH]\""}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ran"})),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let mut shell = gns_tools::ShellConfig::default();
    shell.env_allowlist.retain(|k| k != "HOME");
    host.register_tool(gns_tools::ShellTool::arc(shell));
    let agent = host.create_agent(AgentSpec::new("Sh", "")).await.unwrap();
    host.send_user_message(agent.id.as_str(), "run the env check", vec![]).await.unwrap();
    let out = &last_tool_results(&mock)[0].content;
    assert!(out.contains("[]\n"), "HOME must be withheld: {out}");
    assert!(out.contains(&format!("[{}]", agent.id)), "GNS_AGENT_ID injected: {out}");
    assert!(out.contains(&format!("[{}]", std::env::var("PATH").unwrap())), "PATH must be passed through: {out}");
    host.shutdown().await;
}

#[tokio::test]
async fn shell_survives_large_stderr_and_backgrounds_on_timeout() {
    // 200KB on stderr while stdout stays open used to deadlock the reader.
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "head -c 200000 /dev/zero | tr '\\0' x 1>&2; echo out-ok"}))
            .and_tool_call(SHELL_TOOL_NAME, json!({"command": "echo partial; sleep 5; echo never", "block_until_ms": 500}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ran"})),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let agent = host.create_agent(AgentSpec::new("Sh", "")).await.unwrap();
    let started = std::time::Instant::now();
    host.send_user_message(agent.id.as_str(), "stress the shell", vec![]).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10), "no deadlock");
    let results = last_tool_results(&mock);
    assert!(results[0].content.starts_with("Exit code: 0"), "{}", results[0].content);
    assert!(results[0].content.contains("out-ok"), "{}", results[0].content);
    // A command that outlives block_until_ms is moved to the background, not killed.
    assert!(results[1].content.contains("was sent to the background"), "{}", results[1].content);
    assert!(results[1].content.contains("partial"), "output before backgrounding is kept: {}", results[1].content);
    host.shutdown().await;
}

#[tokio::test]
async fn shell_guard_denies_and_reports() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "rm -rf / --no-preserve-root"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"tried"})),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    host.add_policy(Arc::new(ShellGuardPolicy::default()));
    let agent = host.create_agent(AgentSpec::new("Sh", "")).await.unwrap();
    let mut rx = host.subscribe();
    host.send_user_message(agent.id.as_str(), "wipe the disk", vec![]).await.unwrap();
    let results = last_tool_results(&mock);
    assert!(results[0].content.contains("Policy blocked this action"), "{}", results[0].content);
    assert!(results[0].content.contains("Do not retry"));
    let mut blocked_event = false;
    while let Ok(event) = rx.try_recv() {
        if let HostEvent::ToolCall { status: ToolCallStatus::Failed, summary: Some(s), .. } = event
            && s == "blocked by policy"
        {
            blocked_event = true;
        }
    }
    assert!(blocked_event);
    host.shutdown().await;
}

#[tokio::test]
async fn approval_flow_approve_then_deny() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf A > a.txt"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"first"})),
        LlmResponse::text("done"),
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf B > b.txt"}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"second"})),
        LlmResponse::text("done"),
    ]);
    let (host, dir) = open_host(mock.clone(), |_| {}).await;
    host.add_policy(Arc::new(ApproveToolsPolicy { tools: vec![SHELL_TOOL_NAME.to_owned()] }));
    let agent = host.create_agent(AgentSpec::new("W", "")).await.unwrap();
    let mut rx = host.subscribe();

    // Turn 1: approve.
    let host2 = host.clone();
    let id = agent.id.clone();
    let turn = tokio::spawn(async move { host2.send_user_message(id.as_str(), "write a.txt", vec![]).await.unwrap() });
    let request = loop {
        match rx.recv().await.unwrap() {
            HostEvent::ApprovalRequested { request } => break request,
            _ => continue,
        }
    };
    assert_eq!(request.tool, SHELL_TOOL_NAME);
    assert_eq!(request.agent_id, agent.id);
    assert_eq!(host.pending_approvals().len(), 1);
    assert!(host.approve(&request.id, true, "ok"));
    turn.await.unwrap();
    assert!(dir.path().join("agents").join(agent.id.as_str()).join("workspace").join("a.txt").exists());
    assert!(host.pending_approvals().is_empty());
    assert!(!host.approve(&request.id, true, "again"), "resolved ids are gone");

    // Turn 2: deny.
    let host2 = host.clone();
    let id = agent.id.clone();
    let turn = tokio::spawn(async move { host2.send_user_message(id.as_str(), "write b.txt", vec![]).await.unwrap() });
    let request = loop {
        match rx.recv().await.unwrap() {
            HostEvent::ApprovalRequested { request } => break request,
            _ => continue,
        }
    };
    assert!(host.approve(&request.id, false, "not now"));
    turn.await.unwrap();
    assert!(!dir.path().join("agents").join(agent.id.as_str()).join("workspace").join("b.txt").exists());
    let results = last_tool_results(&mock);
    assert!(results[0].content.contains("The user declined this action (not now)"), "{}", results[0].content);
    host.shutdown().await;
}

#[tokio::test]
async fn approval_is_cancelled_with_the_turn() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SHELL_TOOL_NAME, json!({"command": "printf A > a.txt"})),
        LlmResponse::text("quiet"),
        send_message("hello"),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    host.add_policy(Arc::new(ApproveToolsPolicy { tools: vec![SHELL_TOOL_NAME.to_owned()] }));
    let agent = host.create_agent(AgentSpec::new("W", "")).await.unwrap();
    // A hidden automation-lane turn asks for approval, then a user message preempts it.
    host.enqueue(agent.id.as_str(), gns_runtime::RunJob::hidden("[routine] write it", RunOptions::automation())).unwrap();
    let mut rx = host.subscribe();
    loop {
        if let HostEvent::ApprovalRequested { .. } = rx.recv().await.unwrap() {
            break;
        }
    }
    let result = host.send_user_message(agent.id.as_str(), "hi there friend", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(host.pending_approvals().is_empty(), "cancelled turn withdrew its request");
    host.shutdown().await;
}

#[tokio::test]
async fn file_images_outside_directories_are_rejected() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"look","images":[{"url":"file:///etc/hosts"}]}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"ok"})),
        LlmResponse::text("done"),
    ]);
    let (host, _dir) = open_host(mock.clone(), |_| {}).await;
    let agent = host.create_agent(AgentSpec::new("I", "")).await.unwrap();
    let result = host.send_user_message(agent.id.as_str(), "show me the hosts file", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1, "only the clean message was delivered");
    let results = last_tool_results(&mock);
    assert!(results[0].content.contains("outside your directories"));
    host.shutdown().await;
}

#[tokio::test]
async fn compaction_inserts_divider_without_duplicating_entries() {
    let mock = MockLlm::new().with_responder(|req| {
        if let Some(LlmMessage::ToolResults(_)) = req.messages.last() {
            LlmResponse::text("done")
        } else if req.system.contains("tasked with summarizing") {
            LlmResponse::text("SUMMARY: earlier chatter about files")
        } else {
            send_message("reply ".repeat(20).trim())
        }
    });
    // The mock reports no usage, so the window is estimated at 4 chars per token.
    let (host, _dir) = open_host(mock.clone(), |c| c.context_window_tokens = 10_100).await;
    let agent = host.create_agent(AgentSpec::new("C", "")).await.unwrap();
    for i in 0..8 {
        host.send_user_message(agent.id.as_str(), format!("message number {i} with some padding text to fill the window"), vec![])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await; // let background compaction settle
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let transcript = host.transcript(agent.id.as_str(), 500).unwrap();
    let dividers: Vec<_> = transcript.iter().filter(|e| matches!(e, TranscriptEntry::Divider { .. })).collect();
    assert!(!dividers.is_empty(), "a divider was written");
    // No entry appears twice.
    let mut ids: Vec<String> = transcript.iter().map(|e| e.id().to_string()).collect();
    let before = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), before, "no duplicated entries");
    let user_messages = transcript.iter().filter(|e| matches!(e, TranscriptEntry::Message { role: Role::User, .. })).count();
    assert_eq!(user_messages, 8, "history is preserved, not deleted");
    assert!(matches!(dividers.last().unwrap(), TranscriptEntry::Divider { summarized_through: Some(_), .. }));

    // The next prompt starts with the summary and is smaller than the full history.
    host.send_user_message(agent.id.as_str(), "one more message please", vec![]).await.unwrap();
    let last = mock.requests().into_iter().rev().find(|r| !r.system.contains("tasked with summarizing")).unwrap();
    assert!(
        matches!(&last.messages[0], LlmMessage::User { text, .. } if text.starts_with("[Previous conversation summary]: ") && text.contains("SUMMARY:"))
    );
    assert!(last.messages.len() < 8 * 3);
    let epoch: u64 = host.agent_handle(agent.id.as_str()).unwrap().db.get_json(kv_keys::COMPACTION_EPOCH).unwrap().unwrap();
    assert!(epoch >= 1);
    host.shutdown().await;
}

#[tokio::test]
async fn delete_agent_removes_files_group_membership_and_routing() {
    let mock = MockLlm::scripted(vec![send_message("hi"), LlmResponse::text("done")]);
    let (host, dir) = open_host(mock.clone(), |_| {}).await;
    let a = host.create_agent(AgentSpec::new("A", "")).await.unwrap();
    let b = host.create_agent(AgentSpec::new("B", "")).await.unwrap();
    let group = host
        .create_group(GroupSpec { name: "G".into(), description: String::new(), members: vec![a.id.clone(), b.id.clone()] })
        .await
        .unwrap();
    host.send_user_message(a.id.as_str(), "hello", vec![]).await.unwrap();
    let mut rx = host.subscribe();
    host.delete_agent(a.id.as_str()).await.unwrap();
    assert!(!dir.path().join("agents").join(a.id.as_str()).exists());
    assert!(host.find_agent(a.id.as_str()).is_none());
    assert_eq!(host.find_group(group.id.as_str()).unwrap().members.len(), 1);
    assert!(matches!(host.send_user_message(a.id.as_str(), "x", vec![]).await, Err(HostError::AgentNotFound(_))));
    let mut deleted = false;
    while let Ok(e) = rx.try_recv() {
        if matches!(e, HostEvent::AgentDeleted { ref agent_id } if agent_id == &a.id) {
            deleted = true;
        }
    }
    assert!(deleted);
    host.delete_group(group.id.as_str()).unwrap();
    assert!(host.find_group(group.id.as_str()).is_none());
    assert!(!dir.path().join("agents").join(group.id.as_str()).exists());
    host.shutdown().await;
}

#[tokio::test]
async fn kickstart_is_superseded_by_the_first_user_message() {
    let mut mock = MockLlm::new().with_responder(|req| {
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
        } else if last.contains("[first run]") {
            send_message("Hello, I'm new here!")
        } else {
            send_message("answering you")
        }
    });
    mock.delay = Some(Duration::from_millis(150));
    let (host, _dir) = open_host(mock.clone(), |c| c.kickstart_new_agents = true).await;
    let mut rx = host.subscribe();
    let mut spec = AgentSpec::new("K", "");
    spec.kickstart = true;
    let agent = host.create_agent(spec).await.unwrap();
    // Talk to the agent once the kickstart has dispatched its model call.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let result = host.send_user_message(agent.id.as_str(), "are you there?", vec![]).await.unwrap();
    assert_eq!(result.sent_message_count, 1);
    let mut interrupted = false;
    while let Ok(e) = rx.try_recv() {
        if matches!(e, HostEvent::Interrupted { reason, .. } if reason == "superseded by a new user message") {
            interrupted = true;
        }
    }
    assert!(interrupted, "a dispatched kickstart is superseded by the user's first message");
    let pending: bool = host.agent_handle(agent.id.as_str()).unwrap().db.get_json(kv_keys::INTRODUCTION_PENDING).unwrap().unwrap();
    assert!(!pending, "the first user send clears the introduction flag");
    host.shutdown().await;
}

#[tokio::test]
async fn read_is_capped_and_widget_answer_is_raw() {
    let mock = MockLlm::scripted(vec![
        LlmResponse::tool_call(READ_TOOL_NAME, json!({"path": "big.txt", "limit": 5}))
            .and_tool_call(READ_TOOL_NAME, json!({"path": "big.txt", "offset": 99}))
            .and_tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"text","content":"read"})),
        LlmResponse::text("done"),
        LlmResponse::tool_call(SEND_MESSAGE_TOOL_NAME, json!({"type":"widget","widget":{"prompt":"Pick","options":[{"label":"Blue"}]}})),
        send_message("Blue it is"),
        LlmResponse::text("done"),
    ]);
    let (host, dir) = open_host(mock.clone(), |_| {}).await;
    let agent = host.create_agent(AgentSpec::new("R", "")).await.unwrap();
    let big = dir.path().join("agents").join(agent.id.as_str()).join("workspace").join("big.txt");
    std::fs::create_dir_all(big.parent().unwrap()).unwrap();
    std::fs::write(&big, "x".repeat(100 * 1024)).unwrap();
    host.send_user_message(agent.id.as_str(), "read the big file", vec![]).await.unwrap();
    let results = last_tool_results(&mock);
    let out = &results[0].content;
    assert!(out.contains("exceeds maximum allowed characters (100000 characters)"), "{}", &out[out.len().saturating_sub(200)..]);
    assert!(out.len() < 100 * 1024);
    let past = &results[1].content;
    assert!(past.contains("Offset 99 is beyond file length (1 lines)"), "{past}");

    host.send_user_message(agent.id.as_str(), "pick a colour for me", vec![]).await.unwrap();
    host.answer_widget(agent.id.as_str(), "Blue").await.unwrap();
    let answer_request = mock.requests().into_iter().rev().nth(1).unwrap();
    assert!(
        matches!(answer_request.messages.iter().rev().find(|m| matches!(m, LlmMessage::User { .. })), Some(LlmMessage::User { text, .. }) if text.contains("\nBlue\n")),
        "{:?}",
        answer_request.messages
    );
    host.shutdown().await;
}
