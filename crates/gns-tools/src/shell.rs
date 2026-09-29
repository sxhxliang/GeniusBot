//! `Shell` and `AwaitShell` (`create-shell-tool.ts`, `prompts/dsv3.ts`,
//! `formatters.ts`, `await.ts`).
//!
//! A command runs under `sh` in the agent's current directory (persisted per
//! agent across calls). Its interleaved stdout/stderr streams to a terminal
//! file `<workspace>/.gns/terminals/<shellId>.txt` with a `pid` /
//! `running_for_ms` header refreshed every 5 s and an `exit_code` /
//! `elapsed_ms` footer once it exits. If it finishes within `block_until_ms`
//! the result is returned inline; otherwise it keeps running in the
//! background and the agent is woken with `[A background command just
//! completed]` when it exits. `AwaitShell` polls the terminal file.

use crate::paths::sandbox_path;
use async_trait::async_trait;
use gns_core::prompt::{ShellCompletion, build_shell_revival_prompt};
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shell limits and environment policy.
#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// `block_until_ms` when the model omits it.
    pub default_block_until_ms: u64,
    /// Upper bound for `block_until_ms`.
    pub max_block_until_ms: u64,
    pub shell: String,
    /// Environment variables copied from the host process (everything else,
    /// including API keys, is withheld from the child).
    pub env_allowlist: Vec<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            default_block_until_ms: SHELL_DEFAULT_BLOCK_UNTIL_MS,
            max_block_until_ms: 600_000,
            shell: "sh".to_owned(),
            env_allowlist: [
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
                "TERM",
                "TMPDIR",
                "TZ",
                "XDG_RUNTIME_DIR",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        }
    }
}

/// Arguments of `Shell`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The command to execute
    pub command: String,
    /// The absolute path to the working directory to execute the command in (defaults to current directory)
    #[serde(default)]
    pub working_directory: Option<String>,
    /// Clear, concise description of what this command does in 5-10 words. Examples:
    /// Input: ls
    /// Output: Lists files in current directory
    ///
    /// Input: git status
    /// Output: Shows working tree status
    ///
    /// Input: npm install
    /// Output: Installs package dependencies
    ///
    /// Input: mkdir foo
    /// Output: Creates directory 'foo'
    #[serde(default)]
    pub description: Option<String>,
    /// How long to block and wait for the command to complete before moving it to background (in milliseconds). Defaults to 30000ms (30 seconds). Set to 0 to immediately run the command in the background. The timer includes the shell startup time.
    #[serde(default)]
    pub block_until_ms: Option<u64>,
}

const SHELL_DESCRIPTION: &str = "Executes a given command in a shell session with optional timeout.\nBefore executing the command, please follow these steps:\n1. Check for Running Processes:\n   - Before starting dev servers or long-running processes that should not be duplicated, search the terminals folder to check if they are already running in existing terminals.\n   - You can use this information to determine which terminal, if any, matches the command you want to run, contains the output from the command you want to inspect, or has changed since you last read them.\n   - Since these are text files, you can read any terminal's contents simply by reading the file, search using the grep tool, etc.\n2. Command Execution:\n   - Always quote file paths that contain spaces with double quotes (e.g., cd \"path with spaces/file.txt\")\n   - Examples of proper quoting:\n     - cd \"/Users/name/My Documents\" (correct)\n     - cd /Users/name/My Documents (incorrect - will fail)\n     - python \"/path/with spaces/script.py\" (correct)\n     - python /path/with spaces/script.py (incorrect - will fail)\n   - After ensuring proper quoting, execute the command.\n   - Capture the output of the command.\nUsage notes:\n- The command argument is required.\n- You can specify an optional timeout in milliseconds (up to 600000ms / 10 minutes). If not specified, commands will timeout after 30000ms (30 seconds).\n- It is very helpful if you write a clear, concise description of what this command does in 5-10 words.\n- VERY IMPORTANT: You MUST avoid using search commands like `find` and `grep`. Instead use Grep, Glob to search. You MUST avoid read tools like `cat`, `head`, and `tail`, and use Read to read files.\n- If you _still_ need to use `grep`, STOP. ALWAYS USE ripgrep at `rg` first, which all users have pre-installed.\n- When issuing multiple commands, use the ';' or '&&' operator to separate them. DO NOT use newlines (newlines are ok in quoted strings).\n- Try to maintain your current working directory throughout the session by using absolute paths and avoiding usage of `cd`. You may use `cd` if the User explicitly requests it.<good-example>pytest /foo/bar/tests</good-example><bad-example>cd /foo/bar && pytest tests</bad-example>\n\nManaging long-running commands:\n- Commands that don't complete within `block_until_ms` (default 30s) are moved to background. The command keeps running and output streams to a terminal file. Set `block_until_ms: 0` to immediately background (use for dev servers, watchers, or any long-running process).\n- You do not need to use '&' at the end of commands.\n- Make sure to set `block_until_ms` to higher than the command's expected runtime. Add some buffer since block_until_ms includes shell startup time; increase buffer next time based on `elapsed_ms` if you chose too low. E.g. if you sleep for 40s, recommended `block_until_ms` is 45s.\n\n- You'll be notified when the backgrounded command completes. Only poll with `AwaitShell` when the command requires close monitoring — long-running jobs that can silently hang or degrade before completing (training runs, evals, deployments, long builds, datagen pipelines, DB migrations, large data transfers). For fire-and-forget commands (tests, installs, dev servers/watchers, short scripts), start them and keep working — you can always poll with `AwaitShell` later if you end up blocked on the result.";

/// Per-agent shell session state shared by every call.
#[derive(Debug, Default)]
struct ShellState {
    /// Current directory per agent (persists across calls).
    cwd: HashMap<AgentId, PathBuf>,
}

/// Run a command in the agent's shell session.
#[derive(Debug)]
pub struct ShellTool {
    config: ShellConfig,
    state: Arc<Mutex<ShellState>>,
}

impl ShellTool {
    pub fn arc(config: ShellConfig) -> Arc<dyn Tool> {
        Typed::arc(Self { config, state: Arc::default() })
    }
}

/// `<workspace>/.gns/terminals`
pub fn terminals_dir(workspace: &Path) -> PathBuf {
    workspace.join(TERMINALS_DIRNAME)
}

/// Next free numeric shell id in the terminals folder.
fn allocate_shell_id(terminals: &Path) -> u64 {
    let max = std::fs::read_dir(terminals)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u64>().ok()))
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    max + 1
}

/// Fixed-width header so it can be rewritten in place.
fn terminal_header(pid: u32, running_for_ms: u64) -> String {
    format!("---\npid: {pid:<10}\nrunning_for_ms: {running_for_ms:<12}\n---\n")
}

fn terminal_footer(exit_code: i64, elapsed_ms: u64) -> String {
    format!("\n---\nexit_code: {exit_code}\nelapsed_ms: {elapsed_ms}\n---\n")
}

fn rewrite_header(path: &Path, pid: u32, running_for_ms: u64) {
    if let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(path)
        && file.seek(SeekFrom::Start(0)).is_ok()
    {
        let _ = file.write_all(terminal_header(pid, running_for_ms).as_bytes());
    }
}

/// Parsed terminal file (`parseFooter` / `parseRunningForMs` / `bodyWithoutMetadata`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalSnapshot {
    pub body: String,
    pub running_for_ms: Option<u64>,
    pub is_complete: bool,
    pub exit_code: Option<i64>,
    pub elapsed_ms: Option<u64>,
}

/// Split a terminal file into header fields, body and footer fields.
pub fn parse_terminal_file(content: &str) -> TerminalSnapshot {
    let mut snapshot = TerminalSnapshot::default();
    let mut body = content;
    if let Some(rest) = body.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---\n")
    {
        let header = &rest[..end];
        snapshot.running_for_ms = header.lines().find_map(|l| l.strip_prefix("running_for_ms:")).and_then(|v| v.trim().parse().ok());
        body = &rest[end + 5..];
    }
    let trimmed_end = body.trim_end_matches(['\n', ' ', '\t', '\r']);
    if trimmed_end.ends_with("\n---")
        && let Some(open) = trimmed_end[..trimmed_end.len() - 4].rfind("\n---\n")
    {
        let footer = &trimmed_end[open + 5..trimmed_end.len() - 4];
        snapshot.is_complete = true;
        snapshot.exit_code = footer.lines().find_map(|l| l.strip_prefix("exit_code:")).and_then(|v| v.trim().parse().ok());
        snapshot.elapsed_ms = footer.lines().find_map(|l| l.strip_prefix("elapsed_ms:")).and_then(|v| v.trim().parse().ok());
        body = &body[..open];
    }
    snapshot.body = body.to_owned();
    snapshot
}

/// `truncateOutput(.., frontAndBack = true)`.
pub fn truncate_front_and_back(output: &str, max_chars: usize) -> (String, bool) {
    let total = output.chars().count();
    if total <= max_chars {
        return (output.to_owned(), false);
    }
    let half = max_chars / 2;
    let head: String = output.chars().take(half).collect();
    let tail: String = output.chars().skip(total - half).collect();
    (format!("{head}\n\n... (output truncated) ...\n\n{tail}"), true)
}

/// `formatShellResult` for an inline completion.
pub fn format_shell_result(output: &str, exit_code: i64, elapsed_ms: u64, cwd: &Path) -> String {
    let (shown, truncated) = truncate_front_and_back(output, SHELL_CHAR_HARD_LIMIT);
    format!(
        "Exit code: {exit_code}\n\nCommand output{}:\n\n```\n{shown}\n```\n\nCommand completed in {elapsed_ms} ms.\n\nShell state (cwd, env vars) persists for subsequent calls. Current directory: {}",
        if truncated { format!(" (truncated to {SHELL_CHAR_HARD_LIMIT} characters)") } else { String::new() },
        cwd.display()
    )
}

/// `formatShellPartialOutputSection`.
fn partial_output_section(partial: &str, heading: &str, truncated_suffix: &str, empty: &str) -> String {
    if partial.is_empty() {
        return empty.to_owned();
    }
    let (shown, truncated) = truncate_front_and_back(partial, SHELL_CHAR_HARD_LIMIT);
    format!("{heading}{}:\n\n```\n{shown}\n```", if truncated { truncated_suffix } else { "" })
}

/// `formatBackgroundedResult` (the `msToWait` branch).
pub fn format_backgrounded_result(partial: &str, shell_id: u64, output_path: &Path, pid: u32, ms_to_wait: u64) -> String {
    let mut out = format!("The command did not complete in {ms_to_wait}ms and was sent to the background.\nShell ID: {shell_id}\n");
    if pid != 0 {
        out.push_str(&format!("PID: {pid}\n"));
    }
    out.push_str(&format!("The output is being written to {}. Don't mention Shell ID to the user.\n\n", output_path.display()));
    out + &partial_output_section(
        partial,
        "Output collected before backgrounding",
        " (truncated)",
        "No output was collected before backgrounding.",
    )
}

/// The `isBackground` success result (`block_until_ms: 0`).
pub fn format_background_started(shell_id: u64, output_path: &Path, pid: u32, command: &str) -> String {
    let mut out = format!("Background command started successfully.\nShell ID: {shell_id}\n");
    if pid != 0 {
        out.push_str(&format!("PID: {pid}\n"));
    }
    out + &format!("Command: {command}\nOutput will be written to {}. Don't mention Shell ID to the user.", output_path.display())
}

#[async_trait]
impl TypedTool for ShellTool {
    type Args = ShellArgs;
    fn name(&self) -> &str {
        SHELL_TOOL_NAME
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Local
    }
    fn description(&self) -> &str {
        SHELL_DESCRIPTION
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let command = args.command.trim().to_owned();
        if command.is_empty() {
            return Err(ToolError::input("command is required"));
        }
        let roots = [ctx.workspace_dir.as_path(), ctx.data_dir.as_path()];
        let cwd = match args.working_directory.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(dir) => sandbox_path(dir, &ctx.workspace_dir, &roots)?,
            None => self
                .state
                .lock()
                .ok()
                .and_then(|s| s.cwd.get(&ctx.agent_id).cloned())
                .filter(|p| p.is_dir())
                .unwrap_or_else(|| ctx.workspace_dir.clone()),
        };
        std::fs::create_dir_all(&cwd).map_err(|e| ToolError::failed(format!("cannot create cwd: {e}")))?;
        let block_until_ms = args.block_until_ms.unwrap_or(self.config.default_block_until_ms).min(self.config.max_block_until_ms);
        let terminals = terminals_dir(&ctx.workspace_dir);
        std::fs::create_dir_all(&terminals).map_err(|e| ToolError::failed(e.to_string()))?;
        let (shell_id, output_path, cwd_file) = {
            let _guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let id = allocate_shell_id(&terminals);
            (id, terminals.join(format!("{id}.txt")), terminals.join(format!("{id}.cwd")))
        };
        let mut file = std::fs::File::create(&output_path).map_err(|e| ToolError::failed(e.to_string()))?;
        file.write_all(terminal_header(0, 0).as_bytes()).map_err(|e| ToolError::failed(e.to_string()))?;
        let err_file = file.try_clone().map_err(|e| ToolError::failed(e.to_string()))?;
        let script = format!(
            "cd \"$GNS_SHELL_CWD\" || exit 1\n{command}\n__gns_rc=$?\nprintf '%s' \"$PWD\" > \"$GNS_SHELL_CWD_FILE\"\nexit $__gns_rc\n"
        );
        let started = Instant::now();
        let spawned = tokio::process::Command::new(&self.config.shell)
            .arg("-c")
            .arg(&script)
            .current_dir(&cwd)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| self.config.env_allowlist.iter().any(|a| a == k)))
            .env("GNS_AGENT_ID", ctx.agent_id.as_str())
            .env("GNS_WORKSPACE", &ctx.workspace_dir)
            .env("GNS_SHELL_ID", shell_id.to_string())
            .env("GNS_SHELL_CWD", &cwd)
            .env("GNS_SHELL_CWD_FILE", &cwd_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(err_file))
            .kill_on_drop(false)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                let _ = std::fs::remove_file(&output_path);
                return Ok(ToolOutput::text(format!("Error: Command failed to spawn: {e}\n\nCommand: {command}")));
            }
        };
        let pid = child.id().unwrap_or(0);
        rewrite_header(&output_path, pid, 0);

        // The monitor owns the child: it refreshes the header, appends the
        // footer, and wakes the agent when the command was backgrounded.
        let backgrounded = Arc::new(std::sync::atomic::AtomicBool::new(block_until_ms == 0));
        let kill = CancellationToken::new();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<i64>();
        {
            let backgrounded = backgrounded.clone();
            let kill = kill.clone();
            let services = ctx.services.clone();
            let agent_id = ctx.agent_id.clone();
            let origin_task = services.background_task_context(&ctx.run_id);
            let output_path = output_path.clone();
            let title =
                args.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned).unwrap_or_else(|| command.clone());
            let job_id = shell_id.to_string();
            tokio::spawn(async move {
                let status = loop {
                    tokio::select! {
                        s = child.wait() => break s.ok(),
                        _ = kill.cancelled() => { let _ = child.kill().await; break child.wait().await.ok(); }
                        _ = tokio::time::sleep(Duration::from_millis(SHELL_HEADER_REFRESH_MS)) => {
                            rewrite_header(&output_path, pid, started.elapsed().as_millis() as u64);
                        }
                    }
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let exit_code = status.and_then(|s| s.code()).map(i64::from).unwrap_or(-1);
                rewrite_header(&output_path, pid, elapsed_ms);
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&output_path) {
                    let _ = f.write_all(terminal_footer(exit_code, elapsed_ms).as_bytes());
                }
                let _ = done_tx.send(exit_code);
                if backgrounded.load(std::sync::atomic::Ordering::SeqCst) && !kill.is_cancelled() {
                    services.emit(HostEvent::BackgroundJobFinished { agent_id: agent_id.clone(), job_id, exit_code: exit_code as i32 });
                    let prompt = build_shell_revival_prompt(&[ShellCompletion {
                        title,
                        status: if exit_code == 0 { "success".to_owned() } else { "failed".to_owned() },
                        detail: Some(format!("Exit code: {exit_code} after {elapsed_ms} ms.")),
                        output_path: Some(output_path.display().to_string()),
                        quiet_origin: None,
                    }]);
                    let _ = services.wake_agent_for_task(&agent_id, origin_task.as_ref(), prompt);
                }
            });
        }

        let summary = gns_core::text::clamp_line(&command, 80);
        if block_until_ms == 0 {
            ctx.services.emit(HostEvent::BackgroundJobStarted {
                agent_id: ctx.agent_id.clone(),
                job_id: shell_id.to_string(),
                command: command.clone(),
            });
            return Ok(ToolOutput::text(format_background_started(shell_id, &output_path, pid, &command))
                .with_summary(format!("background: {summary}")));
        }
        let outcome = tokio::select! {
            _ = ctx.cancel.cancelled() => { kill.cancel(); return Err(ToolError::Cancelled); }
            r = done_rx => r.ok(),
            _ = tokio::time::sleep(Duration::from_millis(block_until_ms)) => None,
        };
        match outcome {
            Some(exit_code) => {
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let snapshot = parse_terminal_file(&std::fs::read_to_string(&output_path).unwrap_or_default());
                let new_cwd = std::fs::read_to_string(&cwd_file).ok().map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or(cwd);
                let _ = std::fs::remove_file(&cwd_file);
                if let Ok(mut s) = self.state.lock() {
                    s.cwd.insert(ctx.agent_id.clone(), new_cwd.clone());
                }
                Ok(ToolOutput::text(format_shell_result(&snapshot.body, exit_code, elapsed_ms, &new_cwd)).with_summary(summary))
            }
            None => {
                backgrounded.store(true, std::sync::atomic::Ordering::SeqCst);
                ctx.services.emit(HostEvent::BackgroundJobStarted {
                    agent_id: ctx.agent_id.clone(),
                    job_id: shell_id.to_string(),
                    command: command.clone(),
                });
                let snapshot = parse_terminal_file(&std::fs::read_to_string(&output_path).unwrap_or_default());
                Ok(ToolOutput::text(format_backgrounded_result(&snapshot.body, shell_id, &output_path, pid, block_until_ms))
                    .with_summary(format!("background: {summary}")))
            }
        }
    }
}

/// Arguments of `AwaitShell`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AwaitShellArgs {
    /// Optional shell id to poll. If omitted, this tool sleeps for the full block_until_ms duration and then returns. Required when block_until_ms is 0.
    #[serde(default)]
    pub shell_id: Option<String>,
    /// Max sleep time to block before returning (in milliseconds). Defaults to 30000ms. Set to 0 for non-blocking status check.
    #[serde(default)]
    pub block_until_ms: Option<u64>,
    /// Block until the regex matches stdout/stderr stream (or task completes). Matches anywhere in the shell output, not just new output. Will not match terminal file headers or footers, e.g. exit_code. Accepts JavaScript regex patterns (compiled with the multiline `m` flag).
    #[serde(default)]
    pub pattern: Option<String>,
}

const AWAIT_SHELL_DESCRIPTION: &str = "Check or poll a backgrounded shell job. For work that does not have a shell id, you can omit the shell_id arg to sleep for the full `block_until_ms` duration (prefer this over sleeping in the shell, because it renders nicely to the user). At the end of your turn, you will be notified about any unawaited jobs that completed. If you think a job completed (e.g. because you killed it), observe it with AwaitShell to skip the notification, because stale notifications can confuse the user.\n\nPrefer NOT to poll reflexively with AwaitShell. Multitask on independent work while backgrounded jobs run, or finish your turn and rely on the end-of-turn completion notification. Poll with AwaitShell only when one of the following is true:\n- Your very next step is blocked on this specific job's result and you have no other productive work to do, OR\n- The task requires close monitoring (see shell guidance below).\n- Never poll a task whose tool result says it was \"manually backgrounded by the user\".\n- Shell: only poll with AwaitShell when the command requires close monitoring. Close monitoring means a long-running job that can silently hang, degrade, or need a course correction before it completes — e.g. training runs, eval runs, deployments, long builds, datagen pipelines, DB migrations, large data transfers. For fire-and-forget commands (tests, installs, dev servers/watchers, short scripts, etc.) the completion notification is enough — start them, keep working, and only poll with AwaitShell later if you end up blocked on the result.\n- Shell sanity check (regardless of close monitoring): when you spawn a command directly into the background (`block_until_ms: 0`), do a single status check by reading the output file to confirm the command didn't fail to start. This is a one-shot smoke check, not a polling loop.\n- Shell close-monitoring guidance:\n  - HARD STOPPING CONSTRAINT: once you've decided to actively poll, don't stop until (a) the job terminates, (b) the command reaches a healthy steady state (only for non-terminating commands, e.g. dev server/watcher), or (c) the command is hung — follow the hang guidance below.\n  - Waiting until a regex matches the output can be useful for e.g. known startup/status/error logs.\n  - Size `block_until_ms` to the command's expected runtime. When waiting further, avoid round 5-minute waits: prefer slices of 60–270s (keeps prompt cache warm) or 1200s+ (one cache miss buys a long wait).\n  - Output file header has `pid` and `running_for_ms` (updated every 5000ms).\n  - When finished, footer with `exit_code` and `elapsed_ms` appears (regex only matches the body, not header/footer).\n  - If the command is taking longer than expected and appears hung, kill it if safe using the pid in the header. If possible, fix the hang and proceed.";

/// Poll a backgrounded shell job.
#[derive(Debug, Default)]
pub struct AwaitShellTool;

impl AwaitShellTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

async fn sleep_or_cancel(ctx: &ToolContext, duration: Duration) -> Result<(), ToolError> {
    tokio::select! {
        _ = ctx.cancel.cancelled() => Err(ToolError::Cancelled),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

#[async_trait]
impl TypedTool for AwaitShellTool {
    type Args = AwaitShellArgs;
    fn name(&self) -> &str {
        AWAIT_SHELL_TOOL_NAME
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Local
    }
    fn description(&self) -> &str {
        AWAIT_SHELL_DESCRIPTION
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let block_until_ms = args.block_until_ms.unwrap_or(SHELL_DEFAULT_BLOCK_UNTIL_MS);
        let task_id = args.shell_id.as_deref().map(str::trim).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("none")).unwrap_or("");
        let started = Instant::now();
        if task_id.is_empty() {
            if block_until_ms == 0 {
                return Err(ToolError::input("Must pass a task id or wait for a nonzero duration."));
            }
            sleep_or_cancel(ctx, Duration::from_millis(block_until_ms)).await?;
            return Ok(ToolOutput::text("Slept briefly."));
        }
        if !task_id.chars().all(|c| c.is_ascii_digit()) {
            return Err(ToolError::input(
                "You should NOT wait for subagents to complete. End your turn instead; completions are queued, or do parallel work.",
            ));
        }
        let pattern = args.pattern.as_deref().map(str::trim).filter(|p| !p.is_empty());
        let matcher = match pattern {
            Some(p) => {
                Some(regex::RegexBuilder::new(p).multi_line(true).build().map_err(|e| ToolError::input(format!("invalid pattern: {e}")))?)
            }
            None => None,
        };
        let output_path = terminals_dir(&ctx.workspace_dir).join(format!("{task_id}.txt"));
        let deadline = started + Duration::from_millis(block_until_ms);
        loop {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let raw = match std::fs::read(&output_path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(ToolOutput::text(format!("Error awaiting task: No shell found for id {task_id}")));
                }
                Err(e) => return Err(ToolError::failed(format!("cannot read {}: {e}", output_path.display()))),
            };
            let output_length = raw.len();
            let snapshot = parse_terminal_file(&String::from_utf8_lossy(&raw));
            let runtime_ms = snapshot.elapsed_ms.or(snapshot.running_for_ms).unwrap_or(started.elapsed().as_millis() as u64);
            let matched = matcher.as_ref().is_some_and(|m| m.is_match(&snapshot.body));
            if snapshot.is_complete {
                let exit = snapshot.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_owned());
                return Ok(ToolOutput::text(format!(
                    "Task completed in {runtime_ms}ms with exit code: {exit}.\noutput_file_path: {}\noutput_length: {output_length}",
                    output_path.display()
                ))
                .with_summary(format!("shell {task_id}")));
            }
            if block_until_ms == 0 || Instant::now() >= deadline || matched {
                return Ok(ToolOutput::text(format!(
                    "Task still running after {runtime_ms}ms...\noutput_file_path: {}\noutput_length: {output_length}",
                    output_path.display()
                ))
                .with_summary(format!("shell {task_id}")));
            }
            sleep_or_cancel(ctx, Duration::from_millis(SHELL_CHECK_SLICE_MS)).await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_file_round_trips() {
        let mut text = terminal_header(42, 5000);
        text.push_str("hello\nworld\n");
        let running = parse_terminal_file(&text);
        assert_eq!(running.body, "hello\nworld\n");
        assert_eq!(running.running_for_ms, Some(5000));
        assert!(!running.is_complete);
        text.push_str(&terminal_footer(3, 6100));
        let done = parse_terminal_file(&text);
        assert_eq!(done.body, "hello\nworld\n");
        assert!(done.is_complete);
        assert_eq!(done.exit_code, Some(3));
        assert_eq!(done.elapsed_ms, Some(6100));
        // Rewriting the header keeps its byte length.
        assert_eq!(terminal_header(0, 0).len(), terminal_header(99_999, 123_456_789).len());
    }

    #[test]
    fn result_formats_match_the_original() {
        let cwd = Path::new("/tmp/ws");
        assert_eq!(
            format_shell_result("ok\n", 0, 12, cwd),
            "Exit code: 0\n\nCommand output:\n\n```\nok\n\n```\n\nCommand completed in 12 ms.\n\nShell state (cwd, env vars) persists for subsequent calls. Current directory: /tmp/ws"
        );
        let long = "x".repeat(SHELL_CHAR_HARD_LIMIT + 10);
        let text = format_shell_result(&long, 1, 5, cwd);
        assert!(text.starts_with("Exit code: 1\n\nCommand output (truncated to 20000 characters):\n\n```\n"));
        assert!(text.contains("\n\n... (output truncated) ...\n\n"));
        let backgrounded = format_backgrounded_result("", 7, Path::new("/t/7.txt"), 99, 30000);
        assert_eq!(
            backgrounded,
            "The command did not complete in 30000ms and was sent to the background.\nShell ID: 7\nPID: 99\nThe output is being written to /t/7.txt. Don't mention Shell ID to the user.\n\nNo output was collected before backgrounding."
        );
        assert!(
            format_backgrounded_result("partial", 7, Path::new("/t/7.txt"), 0, 1)
                .ends_with("Output collected before backgrounding:\n\n```\npartial\n```")
        );
        assert_eq!(
            format_background_started(3, Path::new("/t/3.txt"), 5, "sleep 9"),
            "Background command started successfully.\nShell ID: 3\nPID: 5\nCommand: sleep 9\nOutput will be written to /t/3.txt. Don't mention Shell ID to the user."
        );
    }

    /// A `HostServices` stub recording wakes and events.
    #[derive(Default)]
    struct StubServices {
        wakes: Mutex<Vec<String>>,
        events: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl HostServices for StubServices {
        fn roster(&self) -> Vec<AgentAddress> {
            Vec::new()
        }
        fn groups(&self) -> Vec<GroupAddress> {
            Vec::new()
        }
        async fn deliver_message(&self, _: &AgentId, _: &RunId, _: OutboundMessage) -> Result<EntryId, HostError> {
            Ok(EntryId::new())
        }
        async fn send_to_agent(&self, _: &AgentId, _: &str, _: String, _: Vec<ImageRef>, _: bool) -> Result<String, HostError> {
            Ok(String::new())
        }
        async fn create_agent(&self, _: AgentSpec) -> Result<AgentAddress, HostError> {
            Err(HostError::Other("stub".into()))
        }
        async fn update_agent(&self, _: &str, _: ProfilePatch) -> Result<Option<AgentAddress>, HostError> {
            Ok(None)
        }
        fn update_settings(&self, _: &AgentId, _: SettingsPatch) -> Result<(), HostError> {
            Ok(())
        }
        fn memory(&self) -> Arc<dyn MemoryService> {
            unimplemented!()
        }
        fn routines(&self) -> Arc<dyn RoutineService> {
            unimplemented!()
        }
        fn emit(&self, event: HostEvent) {
            self.events.lock().unwrap().push(format!("{event:?}"));
        }
        async fn run_subagent(&self, _: &AgentId, _: &RunId, _: SubagentSpec) -> Result<SubagentResult, HostError> {
            Err(HostError::Other("stub".into()))
        }
        fn wake_agent(&self, _: &AgentId, prompt: String) -> Result<(), HostError> {
            self.wakes.lock().unwrap().push(prompt);
            Ok(())
        }
    }

    fn context(workspace: &Path, services: Arc<StubServices>) -> ToolContext {
        ToolContext {
            agent_id: AgentId::from("a1"),
            agent_name: "A".into(),
            run_id: RunId::new(),
            lane: Lane::User,
            source: RunSource::User,
            workspace_dir: workspace.to_path_buf(),
            data_dir: workspace.join(".data"),
            agents_root: workspace.to_path_buf(),
            cancel: CancellationToken::new(),
            services,
            group_id: None,
        }
    }

    async fn call(tool: &Arc<dyn Tool>, ctx: &ToolContext, args: serde_json::Value) -> String {
        tool.call(ctx, args).await.unwrap().content
    }

    #[tokio::test]
    async fn shell_runs_inline_persists_cwd_and_backgrounds_slow_commands() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        let services = Arc::new(StubServices::default());
        let ctx = context(&ws, services.clone());
        let shell = ShellTool::arc(ShellConfig::default());
        let out = call(&shell, &ctx, serde_json::json!({"command": "echo hi; cd sub"})).await;
        assert!(out.starts_with("Exit code: 0\n\nCommand output:\n\n```\nhi\n\n```\n\nCommand completed in "), "{out}");
        assert!(out.ends_with(&format!("Current directory: {}", ws.join("sub").display())), "{out}");
        let out = call(&shell, &ctx, serde_json::json!({"command": "pwd; exit 3"})).await;
        assert!(out.starts_with(&format!("Exit code: 3\n\nCommand output:\n\n```\n{}\n", ws.join("sub").display())), "{out}");
        let out = call(&shell, &ctx, serde_json::json!({"command": "definitely-not-a-command-xyz"})).await;
        assert!(out.starts_with("Exit code: 127"), "{out}");

        let out = call(
            &shell,
            &ctx,
            serde_json::json!({"command": "echo start; sleep 1; echo done", "block_until_ms": 100, "description": "Sleeps briefly"}),
        )
        .await;
        assert!(out.starts_with("The command did not complete in 100ms and was sent to the background.\nShell ID: 4\nPID: "), "{out}");
        let terminal = terminals_dir(&ws).join("4.txt");
        assert!(out.contains(&format!("The output is being written to {}. Don't mention Shell ID to the user.", terminal.display())));
        assert!(out.ends_with("Output collected before backgrounding:\n\n```\nstart\n\n```"), "{out}");

        let await_tool = AwaitShellTool::arc();
        let out = call(&await_tool, &ctx, serde_json::json!({"shell_id": "4", "block_until_ms": 0})).await;
        assert!(out.starts_with("Task still running after "), "{out}");
        let out = call(&await_tool, &ctx, serde_json::json!({"shell_id": "4", "block_until_ms": 5000, "pattern": "^done$"})).await;
        assert!(out.starts_with("Task completed in ") && out.contains("ms with exit code: 0.\noutput_file_path: "), "{out}");
        assert!(out.contains("\noutput_length: "));
        let file = std::fs::read_to_string(&terminal).unwrap();
        let snapshot = parse_terminal_file(&file);
        assert_eq!(snapshot.body, "start\ndone\n");
        assert_eq!(snapshot.exit_code, Some(0));
        let wakes = services.wakes.lock().unwrap().clone();
        assert_eq!(wakes.len(), 1);
        assert!(wakes[0].starts_with("[A background command just completed] A command you started in the background has finished.\n\nBackground command \"Sleeps briefly\" finished.\nExit code: 0 after "), "{}", wakes[0]);
        assert!(wakes[0].contains(&format!("Full output: {}", terminal.display())));

        let out = call(&await_tool, &ctx, serde_json::json!({"shell_id": "99", "block_until_ms": 0})).await;
        assert_eq!(out, "Error awaiting task: No shell found for id 99");
        let out = call(&await_tool, &ctx, serde_json::json!({"block_until_ms": 10})).await;
        assert_eq!(out, "Slept briefly.");
        let out = call(&shell, &ctx, serde_json::json!({"command": "sleep 0.2", "block_until_ms": 0})).await;
        assert!(out.starts_with("Background command started successfully.\nShell ID: 5\n"), "{out}");
    }
}
