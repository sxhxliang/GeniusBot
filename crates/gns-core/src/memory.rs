//! Three-tier memory model (agent / user / project) with the prompt renderers,
//! extraction/episode prompts and their parsers. Pure functions only; file IO
//! lives in `gns-store`. Every model-facing string is a verbatim port of
//! `source/host/runner/sand-memory.ts`.

use crate::consts::*;
use crate::prompt::DEFAULT_PRODUCT_NAME;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Where a fact is filed on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryKind {
    /// `profile.md`: foundational facts kept in mind every turn.
    Profile,
    /// `log/YYYY-MM.md`: dated history.
    Log,
}

/// Tier requested by the model when writing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryTier {
    Profile,
    Log,
    /// Stored in the log with a `[note] ` prefix so it fades fast.
    Note,
}

impl MemoryTier {
    /// Which file a tier lands in.
    pub fn kind(self) -> MemoryKind {
        match self {
            MemoryTier::Profile => MemoryKind::Profile,
            MemoryTier::Log | MemoryTier::Note => MemoryKind::Log,
        }
    }
}

/// Whose memory a fact belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScope {
    Agent,
    User,
    Project,
}

impl MemoryScope {
    /// Lowercase label for prompts and events.
    pub fn label(self) -> &'static str {
        match self {
            MemoryScope::Agent => "agent",
            MemoryScope::User => "user",
            MemoryScope::Project => "project",
        }
    }
}

/// One remembered fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub content: String,
    pub created_at: i64,
    pub kind: MemoryKind,
}

/// A fact tagged with the assistant that recorded it (shared user memory).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenancedMemory {
    pub record: MemoryRecord,
    pub via: String,
}

/// Facts recalled for a prompt: profile facts plus recent log facts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecall {
    pub profile: Vec<MemoryRecord>,
    pub recent: Vec<MemoryRecord>,
}

/// Collapse whitespace and clamp content length.
pub fn normalize_memory_content(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(MEMORY_MAX_CONTENT_LENGTH).collect()
}

/// Key used to dedupe facts.
pub fn memory_dedupe_key(content: &str) -> String {
    normalize_memory_content(content).to_lowercase()
}

/// Importance weight from the content prefix.
pub fn memory_importance(content: &str) -> f64 {
    if content.starts_with(MEMORY_EPISODE_PREFIX) {
        1.5
    } else if content.starts_with(MEMORY_NOTE_PREFIX) {
        0.5
    } else {
        1.0
    }
}

/// `rank = log2(importance) + created_at / half_life`; higher is fresher/more important.
pub fn memory_recall_rank(record: &MemoryRecord) -> f64 {
    memory_importance(&record.content).log2() + record.created_at as f64 / (MEMORY_DECAY_HALF_LIFE_DAYS * DAY_MS)
}

/// `YYYY-MM-DD` (UTC), or `unknown date` for a missing timestamp.
pub fn format_memory_date(created_at_ms: i64) -> String {
    if created_at_ms <= 0 {
        return "unknown date".to_owned();
    }
    chrono::DateTime::from_timestamp_millis(created_at_ms)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown date".to_owned())
}

/// `- (learned 2026-08-15) fact`
pub fn fact_line(record: &MemoryRecord) -> String {
    format!("- (learned {}) {}", format_memory_date(record.created_at), record.content)
}

/// Newest first, the order the original's agent-tier recall uses for both
/// profile and recent facts (no importance ranking).
pub fn sort_newest_first(records: &[MemoryRecord]) -> Vec<MemoryRecord> {
    let mut sorted = records.to_vec();
    sorted.sort_by_key(|r| std::cmp::Reverse(r.created_at));
    sorted
}

/// Pick the newest recent facts within a count limit and a character budget
/// (the first line is always shown). Returns the selection (newest first)
/// and how many were omitted.
pub fn select_recent_for_prompt(records: &[MemoryRecord], limit: usize, char_budget: usize) -> (Vec<MemoryRecord>, usize) {
    let mut budget = char_budget;
    let mut chosen: Vec<MemoryRecord> = Vec::new();
    for record in sort_newest_first(records) {
        if chosen.len() >= limit {
            break;
        }
        let cost = fact_line(&record).chars().count();
        if !chosen.is_empty() && cost > budget {
            break;
        }
        budget = budget.saturating_sub(cost);
        chosen.push(record);
    }
    let omitted = records.len().saturating_sub(chosen.len());
    (chosen, omitted)
}

/// Agent-scope memory section (`renderMemorySystemPrompt`). Empty when there
/// is nothing to show and no location.
pub fn render_memory_system_prompt(recall: &MemoryRecall, location: Option<&str>) -> String {
    let profile = sort_newest_first(&recall.profile);
    let recent = &recall.recent;
    if profile.is_empty() && recent.is_empty() && location.is_none() {
        return String::new();
    }
    let mut lines = vec![
        "Memory: durable facts you have learned about the user and their world.".to_owned(),
        "These persist across every conversation with this agent, even after the chat is cleared. Rely on them so you stay consistent and avoid re-asking what you already know.".to_owned(),
    ];
    if let Some(location) = location {
        lines.push(format!(
            "Your memory lives in a folder at {location}: profile.md holds who the user is (kept in mind every turn) and log/ holds dated history."
        ));
        lines.push("Read or grep those files with Read and Shell on your own computer when you need older facts that are not listed here. To CHANGE memory, prefer the update_state tool (target \"memory\"): action \"write\" with a fact and a tier (profile | log | note), or action \"forget\" with the exact text of a recorded fact.".to_owned());
    }
    if !profile.is_empty() {
        lines.push("About the user:".to_owned());
        lines.extend(profile.iter().map(fact_line));
    }
    if !recent.is_empty() {
        lines.push("Recently:".to_owned());
        let (chosen, omitted) = select_recent_for_prompt(recent, MEMORY_RECENT_PROMPT_LIMIT, MEMORY_RECENT_PROMPT_CHAR_BUDGET);
        lines.extend(chosen.iter().map(fact_line));
        if omitted > 0 {
            lines.push(match location {
                Some(_) => format!("({omitted} more log facts on disk — grep the log/ folder for them.)"),
                None => format!("({omitted} more log facts not shown.)"),
            });
        }
    }
    if profile.is_empty() && recent.is_empty() {
        lines.push("No facts recorded yet.".to_owned());
    }
    lines.join("\n")
}

/// Merge per-assistant shards of shared user memory (`mergeUserMemoryShards`):
/// newest wins on duplicate keys; profile facts newest first (content as the
/// tie-break), recent facts by importance/decay rank then newest first.
pub fn merge_user_memory_shards(
    shards: &[(String, MemoryRecall)],
    profile_limit: usize,
    recent_limit: usize,
) -> (Vec<ProvenancedMemory>, Vec<ProvenancedMemory>) {
    fn merge(items: Vec<ProvenancedMemory>, limit: usize, rank: bool) -> Vec<ProvenancedMemory> {
        let mut by_key: HashMap<String, ProvenancedMemory> = HashMap::new();
        for item in items {
            let key = memory_dedupe_key(&item.record.content);
            match by_key.get(&key) {
                Some(current) if item.record.created_at <= current.record.created_at => {}
                _ => {
                    by_key.insert(key, item);
                }
            }
        }
        let mut out: Vec<ProvenancedMemory> = by_key.into_values().collect();
        out.sort_by(|a, b| {
            if rank {
                memory_recall_rank(&b.record)
                    .partial_cmp(&memory_recall_rank(&a.record))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.record.created_at.cmp(&a.record.created_at))
            } else {
                b.record.created_at.cmp(&a.record.created_at).then_with(|| a.record.content.cmp(&b.record.content))
            }
        });
        out.truncate(limit);
        out
    }
    let mut profile = Vec::new();
    let mut recent = Vec::new();
    for (via, recall) in shards {
        profile.extend(recall.profile.iter().cloned().map(|record| ProvenancedMemory { record, via: via.clone() }));
        recent.extend(recall.recent.iter().cloned().map(|record| ProvenancedMemory { record, via: via.clone() }));
    }
    (merge(profile, profile_limit, false), merge(recent, recent_limit, true))
}

/// `- (learned d) [via X] fact`
pub fn provenanced_line(record: &ProvenancedMemory) -> String {
    let via = record.via.trim();
    let via = if via.is_empty() { String::new() } else { format!(" [via {via}]") };
    format!("- (learned {}){via} {}", format_memory_date(record.record.created_at), record.record.content)
}

fn append_budgeted_provenanced_facts(
    lines: &mut Vec<String>,
    records: &[ProvenancedMemory],
    char_budget: usize,
    more_label: &str,
    grep_hint: &str,
) {
    let mut budget = char_budget;
    let mut shown = 0usize;
    for record in records {
        let line = provenanced_line(record);
        let cost = line.chars().count();
        if shown > 0 && cost > budget {
            break;
        }
        lines.push(line);
        budget = budget.saturating_sub(cost);
        shown += 1;
    }
    let omitted = records.len().saturating_sub(shown);
    if omitted > 0 {
        lines.push(format!("({omitted} more shared {more_label} on disk — grep {grep_hint} for them.)"));
    }
}

/// Shared user-scope memory section (`renderUserMemorySystemPrompt`). Empty
/// without a user memory directory.
pub fn render_user_memory_system_prompt(
    profile: &[ProvenancedMemory],
    recent: &[ProvenancedMemory],
    user_memory_dir: Option<&str>,
    own_shard_dir: Option<&str>,
) -> String {
    let Some(user_memory_dir) = user_memory_dir else { return String::new() };
    let has_facts = !profile.is_empty() || !recent.is_empty();
    let mut lines = vec![
        "User memory: durable facts shared across every assistant this user runs — their name, timezone, lasting preferences, and anything all of the user's assistants should know. This is separate from your own memory (shown below) and is visible to all of them.".to_owned(),
        "Precedence: when a shared user fact conflicts with your OWN memory, prefer your own — it is curated for your role and may deliberately override a shared default.".to_owned(),
    ];
    if let Some(own_shard_dir) = own_shard_dir {
        lines.push(format!("User memory lives under {user_memory_dir}, split into one shard folder per assistant so every file has a single writer. Your own shard is at {own_shard_dir} (a profile.md and log/YYYY-MM.md you can read and grep with Read and Shell on your own computer). To CHANGE shared user memory, prefer the update_state tool (target \"memory\", scope \"user\", action \"write\" or \"forget\"). Never edit another assistant's shard."));
        lines.push("To fix or replace a shared fact another assistant recorded, write the corrected fact into YOUR shard via update_state — the newest wins on conflict. Record a fact here only when it is clearly about the user and useful to every assistant; keep role-specific facts in your own memory (scope \"agent\").".to_owned());
    }
    if has_facts {
        lines.push("Shared facts are tagged [via <assistant>] so you can tell which assistant learned each one.".to_owned());
    }
    if !profile.is_empty() {
        lines.push("About the user (shared):".to_owned());
        append_budgeted_provenanced_facts(&mut lines, profile, MEMORY_USER_PROFILE_CHAR_BUDGET, "profile facts", "the user-memory/ folder");
    }
    if !recent.is_empty() {
        lines.push("Recently (shared):".to_owned());
        append_budgeted_provenanced_facts(&mut lines, recent, MEMORY_USER_RECENT_CHAR_BUDGET, "log facts", "the user-memory/ folder");
    }
    if !has_facts {
        lines.push("No shared facts recorded yet.".to_owned());
    }
    lines.join("\n")
}

/// One project's shared memory as seen by this agent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectMemoryBlock {
    pub slug: String,
    pub name: String,
    /// This agent's own shard folder inside the project, when it has joined.
    pub own_shard_dir: Option<String>,
    pub profile: Vec<ProvenancedMemory>,
    pub recent: Vec<ProvenancedMemory>,
}

impl ProjectMemoryBlock {
    /// Whether the block has any fact.
    pub fn has_facts(&self) -> bool {
        !self.profile.is_empty() || !self.recent.is_empty()
    }
    fn newest(&self) -> i64 {
        self.profile.iter().chain(self.recent.iter()).map(|r| r.record.created_at).max().unwrap_or(0).max(0)
    }
}

/// Which projects are injected in full and which are only named.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectMemoryRecall {
    pub injected: Vec<ProjectMemoryBlock>,
    /// `(slug, name)` of the projects beyond the cap.
    pub also_member_of: Vec<(String, String)>,
}

impl ProjectMemoryRecall {
    /// Whether any injected block has facts.
    pub fn has_facts(&self) -> bool {
        self.injected.iter().any(ProjectMemoryBlock::has_facts)
    }
}

/// Order projects (facts first, then newest fact, then slug) and split them
/// at `injected_cap` (`selectProjectMemoryBlocks`).
pub fn select_project_memory_blocks(blocks: &[ProjectMemoryBlock], injected_cap: usize) -> ProjectMemoryRecall {
    let mut ordered = blocks.to_vec();
    ordered.sort_by(|a, b| b.has_facts().cmp(&a.has_facts()).then(b.newest().cmp(&a.newest())).then_with(|| a.slug.cmp(&b.slug)));
    let also_member_of = ordered.iter().skip(injected_cap).map(|b| (b.slug.clone(), b.name.clone())).collect();
    ordered.truncate(injected_cap);
    ProjectMemoryRecall { injected: ordered, also_member_of }
}

/// Project-scope memory section (`renderProjectMemorySystemPrompt`). Empty
/// without a projects root directory.
pub fn render_project_memory_blocks(recall: &ProjectMemoryRecall, projects_root_dir: Option<&str>) -> String {
    let Some(root) = projects_root_dir else { return String::new() };
    let mut lines = vec![
        "Project memory: durable facts shared by every assistant that has joined a project — the project's decisions, conventions, and state. Projects are optional and opt-in; joining one lets its memory into your prompt below.".to_owned(),
        "Precedence across memory tiers: on conflict prefer your OWN memory first, then project memory, then user memory — the most specific wins.".to_owned(),
        format!("Projects live under {root}: each is a folder <slug>/ holding a project.md (frontmatter name/description) and memory/by-agent/<assistantId>/ shards (one per contributing assistant, a standard profile.md + log/). Read and grep those folders with Read and Shell on your own computer; prefer the update_state tool for every CHANGE:"),
        "  - Define a project: update_state target \"project\", action \"create\", project=<slug>, name=... (optional description). If the slug already exists this is create-is-join.".to_owned(),
        "  - Join or leave: update_state target \"project\", action \"join\" or \"leave\", project=<slug>. Only projects you have joined load below; to see who else is a member, grep the assistants' projects.json files.".to_owned(),
        "  - Write project facts with update_state target \"memory\", scope \"project\", project=<slug>, action \"write\" or \"forget\" (never another assistant's shard); newest wins on conflict. Record a fact here only when it is about the project and useful to every member.".to_owned(),
    ];
    for block in &recall.injected {
        let tail = match &block.own_shard_dir {
            None => ":".to_owned(),
            Some(dir) => format!(" — your shard: {dir}:"),
        };
        lines.push(format!("Project \"{}\" ({}){tail}", block.name, block.slug));
        let grep_hint = "this project's memory/ folder";
        if !block.profile.is_empty() {
            lines.push("About this project (shared):".to_owned());
            append_budgeted_provenanced_facts(&mut lines, &block.profile, MEMORY_PROJECT_PROFILE_CHAR_BUDGET, "profile facts", grep_hint);
        }
        if !block.recent.is_empty() {
            lines.push("Recently (shared):".to_owned());
            append_budgeted_provenanced_facts(&mut lines, &block.recent, MEMORY_PROJECT_RECENT_CHAR_BUDGET, "log facts", grep_hint);
        }
        if !block.has_facts() {
            lines.push("No shared facts recorded yet for this project.".to_owned());
        }
    }
    if !recall.also_member_of.is_empty() {
        let names = recall.also_member_of.iter().map(|(slug, name)| format!("{name} ({slug})")).collect::<Vec<_>>().join(", ");
        lines.push(format!("Also a member of: {names} — grep those project folders for their memory."));
    }
    lines.join("\n")
}

/// Legacy adapter over [`render_project_memory_blocks`] for callers that only
/// know `(slug, profile, recent)` triples. The original renders nothing
/// without a projects root directory, so this always returns `None`; wire
/// the runtime to [`select_project_memory_blocks`] + [`render_project_memory_blocks`].
pub fn render_project_memory_system_prompt(projects: &[(String, Vec<ProvenancedMemory>, Vec<ProvenancedMemory>)]) -> Option<String> {
    let blocks: Vec<ProjectMemoryBlock> = projects
        .iter()
        .map(|(slug, profile, recent)| ProjectMemoryBlock {
            slug: slug.clone(),
            name: slug.clone(),
            own_shard_dir: None,
            profile: profile.clone(),
            recent: recent.clone(),
        })
        .collect();
    let rendered = render_project_memory_blocks(&select_project_memory_blocks(&blocks, MEMORY_PROJECT_INJECTED_CAP), None);
    if rendered.is_empty() { None } else { Some(rendered) }
}

/// Result of a memory extraction or dream pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryExtraction {
    /// New facts to record, each with its tier (`[note] ` facts already carry their prefix).
    pub additions: Vec<(String, MemoryTier)>,
    /// Existing facts to drop (exact text).
    pub removals: Vec<String>,
}

impl MemoryExtraction {
    /// Nothing to apply.
    pub fn is_empty(&self) -> bool {
        self.additions.is_empty() && self.removals.is_empty()
    }
}

/// System prompt for per-turn fact extraction (`buildExtractionSystemPrompt`).
pub fn build_extraction_system_prompt() -> String {
    [
        MEMORY_EXTRACTION_PROMPT_MARKER.to_owned(),
        "You maintain the long-term memory of a personal assistant. Read the latest exchange and decide what — if anything — is worth remembering for future, unrelated conversations.".to_owned(),
        String::new(),
        "Tag each fact you keep with a category:".to_owned(),
        "- \"profile\": enduring facts about who the user is and how to work with them — their name and how to address them, role, location, languages, lasting preferences and constraints, and important people or relationships. These are remembered indefinitely.".to_owned(),
        "- \"log\": substantive history worth keeping — ongoing projects and tasks, decisions, commitments, and time-bound details.".to_owned(),
        "- \"note\": minor, low-stakes details that might help someday but are not worth keeping in mind every turn (small one-off preferences, incidental context). Notes fade from the always-visible list fastest but stay on disk.".to_owned(),
        String::new(),
        "Do NOT record one-off request mechanics, what the assistant did this turn, general knowledge, or anything already present in the existing memory list.".to_owned(),
        String::new(),
        "If the new exchange updates or contradicts a fact in the existing memory list (e.g. the user moved, changed jobs, or renamed something), drop anything clearly superseded: output a line \"remove: <the exact existing fact text>\" and then add the corrected fact. Only remove facts that appear verbatim in the existing list — never invent removals.".to_owned(),
        String::new(),
        "Write each fact as a self-contained statement, one per line: \"profile: <fact>\", \"log: <fact>\", or \"note: <fact>\" to add (e.g. \"profile: The user's name is Ian\", \"log: Planning a trip to Tokyo in October 2025\"), or \"remove: <existing fact>\" to drop a superseded one.".to_owned(),
        format!("Output exactly {MEMORY_EXTRACTION_NONE_SENTINEL} (and nothing else) when there is nothing to add or remove."),
    ]
    .join("\n")
}

/// User prompt for per-turn fact extraction (`buildExtractionUserPrompt`).
pub fn build_extraction_user_prompt(user_message: &str, agent_message: &str, existing: &[String]) -> String {
    let existing_block =
        if existing.is_empty() { "(empty)".to_owned() } else { existing.iter().map(|m| format!("- {m}")).collect::<Vec<_>>().join("\n") };
    let user = user_message.trim();
    let agent = agent_message.trim();
    [
        "Existing memory:".to_owned(),
        existing_block,
        String::new(),
        "Latest exchange:".to_owned(),
        format!("User: {}", if user.is_empty() { "(no message)" } else { user }),
        format!("Assistant: {}", if agent.is_empty() { "(no message)" } else { agent }),
    ]
    .join("\n")
}

static BULLET_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*(?:[-*•]|\d+[.)])\s+").expect("valid regex"));
static CATEGORY_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(profile|log|note|remove)\s*:\s*(.+)$").expect("valid regex"));

/// Parse extraction output (`parseExtractedMemories`): bullets are stripped,
/// `profile:`/`log:`/`note:`/`remove:` tags are honoured, untagged lines
/// become log additions, notes get the `[note] ` prefix, `NONE` lines are
/// dropped, and additions duplicating `existing` or each other are skipped.
pub fn parse_memory_extraction(raw: &str, existing: &[String]) -> MemoryExtraction {
    let mut extraction = MemoryExtraction::default();
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(MEMORY_EXTRACTION_NONE_SENTINEL) {
        return extraction;
    }
    let mut seen: HashSet<String> = existing.iter().map(|f| memory_dedupe_key(f)).collect();
    for raw_line in trimmed.split('\n') {
        let stripped = BULLET_PREFIX.replace(raw_line, "");
        let captures = CATEGORY_LINE.captures(&stripped);
        let tag = captures.as_ref().and_then(|c| c.get(1)).map(|m| m.as_str().to_lowercase());
        let bare = normalize_memory_content(captures.as_ref().and_then(|c| c.get(2)).map(|m| m.as_str()).unwrap_or(&stripped));
        if bare.is_empty() || bare.eq_ignore_ascii_case(MEMORY_EXTRACTION_NONE_SENTINEL) {
            continue;
        }
        let (content, tier) = match tag.as_deref() {
            Some("remove") => {
                extraction.removals.push(bare);
                continue;
            }
            Some("profile") => (bare, MemoryTier::Profile),
            Some("note") => (normalize_memory_content(&format!("{MEMORY_NOTE_PREFIX}{bare}")), MemoryTier::Note),
            _ => (bare, MemoryTier::Log),
        };
        let key = memory_dedupe_key(&content);
        if !seen.insert(key) {
            continue;
        }
        extraction.additions.push((content, tier));
    }
    extraction
}

static RELEVANCE_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}]{4,}").expect("valid regex"));
const RELEVANCE_STOPWORDS: &[&str] = &[
    "that", "this", "with", "from", "they", "them", "then", "than", "what", "when", "where", "which", "will", "would", "could", "should",
    "have", "been", "being", "about", "just", "like", "your", "does", "were", "also", "into", "over", "only", "some", "more", "most",
    "very", "much", "here", "there", "their", "these", "those", "because", "while", "after", "before", "user",
];

fn relevance_tokens(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    RELEVANCE_TOKEN.find_iter(&lower).map(|m| m.as_str().to_owned()).filter(|t| !RELEVANCE_STOPWORDS.contains(&t.as_str())).collect()
}

/// Archive facts sharing the most tokens with `query`, most overlap then
/// newest first, at most `max` (`selectRelevantMemories`).
pub fn select_relevant_memory_records(query: &str, memories: &[MemoryRecord], max: usize) -> Vec<MemoryRecord> {
    if max == 0 || memories.is_empty() {
        return Vec::new();
    }
    let query_tokens = relevance_tokens(query);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &MemoryRecord)> = memories
        .iter()
        .map(|memory| (relevance_tokens(&memory.content).iter().filter(|t| query_tokens.contains(*t)).count(), memory))
        .filter(|(overlap, _)| *overlap > 0)
        .collect();
    scored.sort_by(|(lo, l), (ro, r)| ro.cmp(lo).then(r.created_at.cmp(&l.created_at)));
    scored.into_iter().take(max).map(|(_, m)| m.clone()).collect()
}

/// [`select_relevant_memory_records`] over bare fact texts: most overlap
/// first, ties in input order (there is no date to break them with).
pub fn select_relevant_memories(archive: &[String], text: &str, limit: usize) -> Vec<String> {
    if limit == 0 || archive.is_empty() {
        return Vec::new();
    }
    let query_tokens = relevance_tokens(text);
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, &String)> = archive
        .iter()
        .map(|fact| (relevance_tokens(fact).iter().filter(|t| query_tokens.contains(*t)).count(), fact))
        .filter(|(overlap, _)| *overlap > 0)
        .collect();
    scored.sort_by(|(lo, _), (ro, _)| ro.cmp(lo));
    scored.into_iter().take(limit).map(|(_, f)| f.clone()).collect()
}

/// The "existing memory" list handed to the extractor: everything in the
/// prompt plus up to [`MEMORY_EXTRACTION_RELEVANT_ARCHIVE_LIMIT`] archive facts
/// relevant to the exchange (`gatherExtractionMemories`).
pub fn gather_extraction_memories(recall: &MemoryRecall, archive: &[MemoryRecord], exchange_text: &str) -> Vec<String> {
    let in_prompt: Vec<&MemoryRecord> = recall.profile.iter().chain(recall.recent.iter()).collect();
    let seen: HashSet<String> = in_prompt.iter().map(|m| memory_dedupe_key(&m.content)).collect();
    let candidates: Vec<MemoryRecord> = archive.iter().filter(|m| !seen.contains(&memory_dedupe_key(&m.content))).cloned().collect();
    in_prompt
        .into_iter()
        .map(|m| m.content.clone())
        .chain(
            select_relevant_memory_records(exchange_text, &candidates, MEMORY_EXTRACTION_RELEVANT_ARCHIVE_LIMIT)
                .into_iter()
                .map(|m| m.content),
        )
        .collect()
}

/// System prompt for the idle-time consolidation ("dreaming") pass. The
/// original gates dreaming off by default; this text is local.
pub fn build_dream_system_prompt() -> String {
    [
        "You consolidate an assistant's memory while it is idle. You get its profile facts (kept in mind every turn) and its recent dated log.",
        "Promote to the profile only what has proven durable: recurring preferences, stable facts about the user, standing arrangements. Write each as \"profile: <fact>\".",
        "Drop facts that are stale, superseded, contradicted by newer ones, or duplicates, using \"remove: <existing fact>\" with the exact recorded text (from either list).",
        "Do not restate facts already in the profile, do not invent anything, keep the profile small. If nothing should change, answer exactly NONE.",
    ]
    .join("\n")
}

/// User prompt for the consolidation pass.
pub fn build_dream_user_prompt(profile: &[MemoryRecord], log: &[MemoryRecord]) -> String {
    let mut out = String::from("Profile facts:\n");
    if profile.is_empty() {
        out.push_str("(none)\n");
    }
    for record in profile {
        out.push_str(&fact_line(record));
        out.push('\n');
    }
    out.push_str("\nRecent log:\n");
    if log.is_empty() {
        out.push_str("(none)\n");
    }
    for record in log {
        out.push_str(&fact_line(record));
        out.push('\n');
    }
    out
}

/// Short exchanges that carry nothing worth remembering.
const TRIVIAL_EXCHANGES: &[&str] = &[
    "hi",
    "hey",
    "hello",
    "yo",
    "sup",
    "thanks",
    "thank you",
    "ty",
    "thx",
    "ok",
    "okay",
    "k",
    "kk",
    "cool",
    "nice",
    "great",
    "awesome",
    "perfect",
    "yes",
    "yep",
    "yeah",
    "no",
    "nope",
    "sure",
    "got it",
    "gotcha",
    "lol",
    "haha",
    "np",
    "done",
    "good",
    "bye",
];

/// Heuristic (`isMemorableExchange`): long messages and questions always
/// count; short ones count unless they are a known pleasantry.
pub fn is_memorable_exchange(user_message: &str) -> bool {
    let user = user_message.trim();
    if user.is_empty() {
        return false;
    }
    if user.chars().count() > 40 || user.contains('?') {
        return true;
    }
    let normalized = user
        .to_lowercase()
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '!' | '.' | '…' | ',' | '~' | ')' | ']'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    !TRIVIAL_EXCHANGES.contains(&normalized.as_str())
}

/// One turn buffered toward an episode summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeTurn {
    pub ts: i64,
    pub user: String,
    pub agent: String,
}

/// System prompt for the episode summariser (`buildEpisodeSystemPrompt`).
pub fn build_episode_system_prompt_for(product_name: &str) -> String {
    [
        MEMORY_EPISODE_PROMPT_MARKER.to_owned(),
        format!("You maintain the long-term memory of a personal desktop assistant named {product_name}."),
        format!("You are given the most recent turns of a conversation between the user and {product_name}, in order, each tagged with its date."),
        format!("Write ONE short journal-style sentence (two at most) capturing what the user and {product_name} were actually working on across these turns — the throughline, key decisions, and outcomes — so it stays useful months from now."),
        "Anchor any time references with the absolute dates shown, never relative words like \"yesterday\". Drop greetings, acknowledgements, and anything ephemeral. Never invent details.".to_owned(),
        format!("Output just the sentence(s), no preamble or bullets. Output exactly {MEMORY_EXTRACTION_NONE_SENTINEL} if nothing in this stretch is worth remembering."),
    ]
    .join("\n")
}

/// [`build_episode_system_prompt_for`] with the default product name.
pub fn build_episode_system_prompt() -> String {
    build_episode_system_prompt_for(DEFAULT_PRODUCT_NAME)
}

/// User prompt for the episode summariser (`buildEpisodeUserPrompt`).
pub fn build_episode_user_prompt_for(turns: &[EpisodeTurn], product_name: &str) -> String {
    let blocks: Vec<String> = turns
        .iter()
        .map(|turn| {
            let mut lines = vec![format!("({})", format_memory_date(turn.ts))];
            if !turn.user.trim().is_empty() {
                lines.push(format!("User: {}", turn.user.trim()));
            }
            if !turn.agent.trim().is_empty() {
                lines.push(format!("{product_name}: {}", turn.agent.trim()));
            }
            lines.join("\n")
        })
        .collect();
    format!("Recent turns, oldest first:\n\n{}", blocks.join("\n\n"))
}

/// [`build_episode_user_prompt_for`] with the default product name.
pub fn build_episode_user_prompt(turns: &[EpisodeTurn]) -> String {
    build_episode_user_prompt_for(turns, DEFAULT_PRODUCT_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(content: &str, day: i64) -> MemoryRecord {
        MemoryRecord { content: content.into(), created_at: day * DAY_MS as i64, kind: MemoryKind::Log }
    }

    fn via(content: &str, day: i64, via: &str) -> ProvenancedMemory {
        ProvenancedMemory { record: rec(content, day), via: via.into() }
    }

    #[test]
    fn importance_prefixes() {
        assert_eq!(memory_importance("[episode] x"), 1.5);
        assert_eq!(memory_importance("[note] x"), 0.5);
        assert_eq!(memory_importance("x"), 1.0);
    }

    #[test]
    fn rank_prefers_fresh_and_important() {
        let old_episode = rec("[episode] big", 0);
        let new_note = rec("[note] small", 10);
        let new_plain = rec("plain", 10);
        assert!(memory_recall_rank(&new_plain) > memory_recall_rank(&new_note));
        assert!(memory_recall_rank(&new_note) < memory_recall_rank(&old_episode) + 1.0);
    }

    #[test]
    fn agent_selection_is_newest_first_without_importance() {
        let mut records: Vec<_> = (1..=10).map(|i| rec(&format!("fact number {i}"), i)).collect();
        records.push(rec("[episode] old but important", 0));
        let (chosen, omitted) = select_recent_for_prompt(&records, 3, 10_000);
        assert_eq!(chosen.len(), 3);
        assert_eq!(omitted, 8);
        assert_eq!(chosen[0].content, "fact number 10");
        assert!(chosen.windows(2).all(|w| w[0].created_at >= w[1].created_at));
        let (tight, _) = select_recent_for_prompt(&records, 10, 40);
        assert_eq!(tight.len(), 1, "the first line is always shown");
        assert!(is_memorable_exchange("use pnpm") && !is_memorable_exchange("thanks!") && !is_memorable_exchange("Got it."));
        assert_eq!(format_memory_date(0), "unknown date");
    }

    #[test]
    fn agent_block_matches_original() {
        assert_eq!(render_memory_system_prompt(&MemoryRecall::default(), None), "");
        let empty = render_memory_system_prompt(&MemoryRecall::default(), Some("/m"));
        assert!(empty.starts_with(
            "Memory: durable facts you have learned about the user and their world.\nThese persist across every conversation"
        ));
        assert!(empty.contains("Your memory lives in a folder at /m: profile.md holds who the user is"));
        assert!(empty.contains("Read or grep those files with Read and Shell on your own computer when you need older facts"));
        assert!(empty.ends_with("No facts recorded yet."));
        let recall = MemoryRecall {
            profile: vec![
                MemoryRecord { content: "older".into(), created_at: 1, kind: MemoryKind::Profile },
                MemoryRecord { content: "newer".into(), created_at: 2, kind: MemoryKind::Profile },
            ],
            recent: vec![rec("a", 1), rec("b", 2)],
        };
        let block = render_memory_system_prompt(&recall, None);
        assert!(block.contains("About the user:\n- (learned 1970-01-01) newer\n- (learned 1970-01-01) older\nRecently:\n- (learned 1970-01-03) b\n- (learned 1970-01-02) a"));
        assert!(!block.contains("No facts recorded yet."));
    }

    #[test]
    fn user_block_matches_original() {
        assert_eq!(render_user_memory_system_prompt(&[], &[], None, None), "");
        let empty = render_user_memory_system_prompt(&[], &[], Some("/u"), Some("/u/a1"));
        assert!(empty.starts_with("User memory: durable facts shared across every assistant this user runs — their name, timezone"));
        assert!(empty.contains("\nPrecedence: when a shared user fact conflicts with your OWN memory, prefer your own"));
        assert!(empty.contains("User memory lives under /u, split into one shard folder per assistant so every file has a single writer. Your own shard is at /u/a1 (a profile.md and log/YYYY-MM.md you can read and grep with Read and Shell on your own computer)."));
        assert!(empty.contains("write the corrected fact into YOUR shard via update_state — the newest wins on conflict."));
        assert!(!empty.contains("Shared facts are tagged"));
        assert!(empty.ends_with("No shared facts recorded yet."));
        let profile = vec![via("Name is Ian", 3, "Ada")];
        let recent = vec![via("Moved to Berlin", 4, " ")];
        let full = render_user_memory_system_prompt(&profile, &recent, Some("/u"), None);
        assert!(full.contains("Shared facts are tagged [via <assistant>] so you can tell which assistant learned each one.\nAbout the user (shared):\n- (learned 1970-01-04) [via Ada] Name is Ian\nRecently (shared):\n- (learned 1970-01-05) Moved to Berlin"));
        assert!(!full.contains("User memory lives under"));
    }

    #[test]
    fn user_block_budget_reports_omitted() {
        let long: Vec<ProvenancedMemory> = (0..30).map(|i| via(&"x".repeat(200), i, "A")).collect();
        let rendered = render_user_memory_system_prompt(&[], &long, Some("/u"), None);
        let shown = rendered.lines().filter(|l| l.starts_with("- (learned")).count();
        assert!(shown < 30 && shown > 0);
        assert!(rendered.contains(&format!("({} more shared log facts on disk — grep the user-memory/ folder for them.)", 30 - shown)));
    }

    #[test]
    fn merge_orders_like_original() {
        let shards = vec![
            (
                "A".to_owned(),
                MemoryRecall {
                    profile: vec![rec("zeta", 5), rec("alpha", 5), rec("dup", 1)],
                    recent: vec![rec("[note] n", 9), rec("plain", 8)],
                },
            ),
            ("B".to_owned(), MemoryRecall { profile: vec![rec("DUP", 2)], recent: vec![rec("[episode] e", 7)] }),
        ];
        let (profile, recent) = merge_user_memory_shards(&shards, 10, 10);
        let p: Vec<_> = profile.iter().map(|m| (m.record.content.as_str(), m.via.as_str())).collect();
        assert_eq!(p, vec![("alpha", "A"), ("zeta", "A"), ("DUP", "B")]);
        let r: Vec<_> = recent.iter().map(|m| m.record.content.as_str()).collect();
        assert_eq!(r, vec!["[episode] e", "plain", "[note] n"], "importance/decay rank, not plain recency");
        let (capped, _) = merge_user_memory_shards(&shards, 1, 10);
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn project_block_matches_original() {
        let blocks = vec![
            ProjectMemoryBlock { slug: "empty".into(), name: "Empty".into(), own_shard_dir: None, profile: vec![], recent: vec![] },
            ProjectMemoryBlock {
                slug: "web".into(),
                name: "Web App".into(),
                own_shard_dir: Some("/p/web/memory/by-agent/a1".into()),
                profile: vec![via("Uses Vite", 3, "Ada")],
                recent: vec![],
            },
            ProjectMemoryBlock {
                slug: "api".into(),
                name: "API".into(),
                own_shard_dir: None,
                profile: vec![],
                recent: vec![via("Cut v2", 9, "Bob")],
            },
            ProjectMemoryBlock { slug: "zzz".into(), name: "Last".into(), own_shard_dir: None, profile: vec![], recent: vec![] },
        ];
        let recall = select_project_memory_blocks(&blocks, 3);
        assert_eq!(recall.injected.iter().map(|b| b.slug.as_str()).collect::<Vec<_>>(), vec!["api", "web", "empty"]);
        assert_eq!(recall.also_member_of, vec![("zzz".to_owned(), "Last".to_owned())]);
        assert_eq!(render_project_memory_blocks(&recall, None), "");
        let rendered = render_project_memory_blocks(&recall, Some("/p"));
        assert!(rendered.starts_with("Project memory: durable facts shared by every assistant that has joined a project"));
        assert!(rendered.contains("Projects live under /p: each is a folder <slug>/ holding a project.md"));
        assert!(rendered.contains("  - Define a project: update_state target \"project\", action \"create\", project=<slug>, name=... (optional description). If the slug already exists this is create-is-join."));
        assert!(rendered.contains("  - Join or leave: update_state target \"project\", action \"join\" or \"leave\", project=<slug>."));
        assert!(rendered.contains("Project \"API\" (api):\nRecently (shared):\n- (learned 1970-01-10) [via Bob] Cut v2\nProject \"Web App\" (web) — your shard: /p/web/memory/by-agent/a1:\nAbout this project (shared):\n- (learned 1970-01-04) [via Ada] Uses Vite\nProject \"Empty\" (empty):\nNo shared facts recorded yet for this project."));
        assert!(rendered.ends_with("Also a member of: Last (zzz) — grep those project folders for their memory."));
        assert!(render_project_memory_system_prompt(&[("x".into(), vec![], vec![])]).is_none());
    }

    #[test]
    fn extraction_prompts_match_original() {
        let system = build_extraction_system_prompt();
        assert!(system.starts_with(
            "<<SAND_MEMORY_EXTRACTION>>\nYou maintain the long-term memory of a personal assistant. Read the latest exchange"
        ));
        assert!(system.contains("\n\nTag each fact you keep with a category:\n- \"profile\": enduring facts"));
        assert!(system.ends_with("Output exactly NONE (and nothing else) when there is nothing to add or remove."));
        assert_eq!(
            build_extraction_user_prompt(" hi ", "", &[]),
            "Existing memory:\n(empty)\n\nLatest exchange:\nUser: hi\nAssistant: (no message)"
        );
        assert_eq!(
            build_extraction_user_prompt("", "yo", &["a".into(), "b".into()]),
            "Existing memory:\n- a\n- b\n\nLatest exchange:\nUser: (no message)\nAssistant: yo"
        );
    }

    #[test]
    fn extraction_parsing_matches_original() {
        let existing = vec!["The user's name is Ian".to_owned()];
        let parsed = parse_memory_extraction(
            "profile: The user's name is Ian\n- log: Planning a trip to Tokyo in October 2025\n2) NOTE:   likes   short replies\nremove: Uses npm\nchatter without prefix\n* NONE\nprofile: Prefers pnpm\nprofile: prefers PNPM",
            &existing,
        );
        assert_eq!(
            parsed.additions,
            vec![
                ("Planning a trip to Tokyo in October 2025".to_owned(), MemoryTier::Log),
                ("[note] likes short replies".to_owned(), MemoryTier::Note),
                ("chatter without prefix".to_owned(), MemoryTier::Log),
                ("Prefers pnpm".to_owned(), MemoryTier::Profile),
            ]
        );
        assert_eq!(parsed.removals, vec!["Uses npm".to_owned()]);
        assert!(parse_memory_extraction("NONE", &[]).is_empty());
        assert!(parse_memory_extraction("  none \n", &[]).is_empty());
    }

    #[test]
    fn relevance_selection_matches_original() {
        let archive = vec![
            rec("Uses Vite with React on the web project", 1),
            rec("Prefers tabs", 2),
            rec("React project deploys to Vercel", 3),
            rec("The user likes tea", 4),
        ];
        let picked = select_relevant_memory_records("How is the react project deployed? this that with", &archive, 10);
        assert_eq!(
            picked.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
            vec!["React project deploys to Vercel", "Uses Vite with React on the web project"]
        );
        assert!(select_relevant_memory_records("the and", &archive, 10).is_empty());
        assert!(select_relevant_memory_records("react", &archive, 0).is_empty());
        let texts: Vec<String> = archive.iter().map(|m| m.content.clone()).collect();
        assert_eq!(select_relevant_memories(&texts, "react project deploys", 1), vec!["React project deploys to Vercel".to_owned()]);
        assert!(select_relevant_memories(&texts, "zzzz", 5).is_empty());
        let recall = MemoryRecall { profile: vec![rec("Prefers tabs", 2)], recent: vec![] };
        let gathered = gather_extraction_memories(&recall, &archive, "react project");
        assert_eq!(gathered, vec!["Prefers tabs", "React project deploys to Vercel", "Uses Vite with React on the web project"]);
    }

    #[test]
    fn episode_prompts_match_original() {
        let system = build_episode_system_prompt();
        assert!(system.starts_with("<<SAND_MEMORY_EPISODE>>\nYou maintain the long-term memory of a personal desktop assistant named Genius Bot.\nYou are given the most recent turns"));
        assert!(system.ends_with("Output exactly NONE if nothing in this stretch is worth remembering."));
        let user = build_episode_user_prompt(&[
            EpisodeTurn { ts: DAY_MS as i64, user: "hi".into(), agent: "hello".into() },
            EpisodeTurn { ts: 2 * DAY_MS as i64, user: String::new(), agent: "done".into() },
        ]);
        assert_eq!(user, "Recent turns, oldest first:\n\n(1970-01-02)\nUser: hi\nGenius Bot: hello\n\n(1970-01-03)\nGenius Bot: done");
    }

    #[test]
    fn dedupe_is_case_and_space_insensitive() {
        assert_eq!(memory_dedupe_key("Likes   PNPM"), memory_dedupe_key("likes pnpm"));
    }
}
