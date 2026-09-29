//! `automations/<id>/automation.json` + `automations/<id>/runs.json` store,
//! byte-compatible with the original host's `automation-store.ts`:
//!
//! * `automation.json` = `{name, prompt, schedule?, trigger?, enabled, createdAt, lastRunAt, raisedNotices?}`
//!   (`schedule` for cron-only routines, `trigger` for everything else);
//! * `runs.json` = newest-first array of `{id, trigger, startedAt, finishedAt, status, detail?, event?, coalescedRunIds?}`, at most 20.
//!
//! The previous Rust layout (`isEnabled`, embedded `runs`) is still read; every
//! write uses the original format.

use crate::files::write_text_atomic;
use gns_core::routine::{
    AutomationConfig, AutomationRecord, AutomationRun, AutomationRunStatus, AutomationRunTrigger, Trigger, clamp_automation_name,
    is_routine_notice_id, is_safe_folder_id, normalize_automation_prompt, normalize_schedule, parse_stored_trigger,
    slugify_automation_name,
};
use gns_core::text::now_ms;
use gns_core::{
    AUTOMATION_MAX_PER_AGENT, AUTOMATION_MAX_RUN_DETAIL_LENGTH, AUTOMATION_MAX_RUN_HISTORY, HostError, MAX_EVENTS_IN_AUTOMATION_WAKE,
};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const AUTOMATIONS_DIRNAME: &str = "automations";
pub const CONFIG_FILENAME: &str = "automation.json";
pub const RUNS_FILENAME: &str = "runs.json";

/// `parseStoredConfigTrigger`: a `trigger` object wins, else a legacy `schedule` string.
pub fn parse_stored_config_trigger(parsed: &serde_json::Map<String, Value>) -> Option<Trigger> {
    if let Some(trigger) = parsed.get("trigger").filter(|t| !t.is_null())
        && let Some(trigger) = parse_stored_trigger(trigger)
    {
        return Some(trigger);
    }
    let schedule = parsed.get("schedule").and_then(Value::as_str).map(normalize_schedule).unwrap_or_default();
    if schedule.is_empty() { None } else { Some(Trigger::cron(schedule)) }
}

/// `parseRaisedNotices`: known notice ids only, deduplicated.
pub fn parse_raised_notices(value: Option<&Value>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for entry in value.and_then(Value::as_array).into_iter().flatten() {
        let Some(id) = entry.as_str().map(str::trim) else { continue };
        if !id.is_empty() && !ids.iter().any(|x| x == id) && is_routine_notice_id(id) {
            ids.push(id.to_owned());
        }
    }
    ids
}

fn finite_i64(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_f64()?;
    if number.is_finite() { Some(number as i64) } else { None }
}

/// `parseStoredConfig`: `None` for anything unusable. `enabled !== false`;
/// `createdAt = min(authored, fallback)`. The legacy Rust `isEnabled` key is
/// honoured when `enabled` is absent.
pub fn parse_stored_config(raw: &str, fallback_created_at: i64) -> Option<AutomationConfig> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let parsed = parsed.as_object()?;
    let name = parsed.get("name").and_then(Value::as_str).map(clamp_automation_name).unwrap_or_default();
    let prompt = parsed.get("prompt").and_then(Value::as_str).map(normalize_automation_prompt).unwrap_or_default();
    let trigger = parse_stored_config_trigger(parsed);
    let (Some(trigger), false, false) = (trigger, name.is_empty(), prompt.is_empty()) else { return None };
    let authored = finite_i64(parsed.get("createdAt")).unwrap_or(fallback_created_at);
    let enabled = match parsed.get("enabled") {
        Some(Value::Bool(false)) => false,
        Some(_) => true,
        None => parsed.get("isEnabled") != Some(&Value::Bool(false)),
    };
    Some(AutomationConfig {
        name,
        prompt,
        trigger,
        is_enabled: enabled,
        created_at: authored.min(fallback_created_at),
        last_run_at: finite_i64(parsed.get("lastRunAt")),
        raised_notices: parse_raised_notices(parsed.get("raisedNotices")),
    })
}

/// Runs embedded in a legacy (pre-`runs.json`) `automation.json`.
fn parse_legacy_embedded_runs(raw: &str) -> Vec<AutomationRun> {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parsed.get("runs").and_then(Value::as_array).map(|runs| runs.iter().filter_map(parse_stored_run).collect()).unwrap_or_default()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredConfig<'a> {
    name: &'a str,
    prompt: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trigger: Option<&'a Trigger>,
    enabled: bool,
    created_at: i64,
    last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    raised_notices: Option<&'a [String]>,
}

/// `serializeConfig`: the exact `automation.json` text.
pub fn serialize_config(config: &AutomationConfig) -> String {
    let stored = StoredConfig {
        name: &config.name,
        prompt: &config.prompt,
        schedule: config.trigger.schedule(),
        trigger: if config.trigger.is_cron() { None } else { Some(&config.trigger) },
        enabled: config.is_enabled,
        created_at: config.created_at,
        last_run_at: config.last_run_at,
        raised_notices: if config.raised_notices.is_empty() { None } else { Some(&config.raised_notices) },
    };
    let mut text = serde_json::to_string_pretty(&stored).unwrap_or_default();
    text.push('\n');
    text
}

/// `clampRunDetail`: trimmed, at most 300 characters, `None` when empty.
pub fn clamp_run_detail(detail: Option<&str>) -> Option<String> {
    let value = detail?.trim();
    if value.is_empty() { None } else { Some(value.chars().take(AUTOMATION_MAX_RUN_DETAIL_LENGTH).collect()) }
}

/// `clampCoalescedRunIds`: non-empty strings, at most 25, `None` when empty.
pub fn clamp_coalesced_run_ids(ids: Option<&[String]>) -> Option<Vec<String>> {
    let ids: Vec<String> = ids?.iter().filter(|id| !id.is_empty()).take(MAX_EVENTS_IN_AUTOMATION_WAKE).cloned().collect();
    if ids.is_empty() { None } else { Some(ids) }
}

/// `parseStoredRun`: lenient parse of one `runs.json` entry.
pub fn parse_stored_run(entry: &Value) -> Option<AutomationRun> {
    let entry = entry.as_object()?;
    let id = entry.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let started_at = finite_i64(entry.get("startedAt"))?;
    let status = match entry.get("status").and_then(Value::as_str) {
        Some("error") => AutomationRunStatus::Error,
        Some("running") => AutomationRunStatus::Running,
        _ => AutomationRunStatus::Ok,
    };
    let trigger = AutomationRunTrigger::parse(entry.get("trigger").and_then(Value::as_str).unwrap_or(""));
    let coalesced: Option<Vec<String>> =
        entry.get("coalescedRunIds").and_then(Value::as_array).map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_owned).collect());
    Some(AutomationRun {
        id: id.to_owned(),
        trigger,
        started_at,
        finished_at: finite_i64(entry.get("finishedAt")),
        status,
        detail: clamp_run_detail(entry.get("detail").and_then(Value::as_str)),
        event: clamp_run_detail(entry.get("event").and_then(Value::as_str)),
        coalesced_run_ids: clamp_coalesced_run_ids(coalesced.as_deref()),
    })
}

/// Routine files for one agent.
#[derive(Clone, Debug)]
pub struct AutomationStore {
    dir: PathBuf,
}

impl AutomationStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn config_path(&self, id: &str) -> PathBuf {
        self.dir.join(id).join(CONFIG_FILENAME)
    }
    pub fn runs_path(&self, id: &str) -> PathBuf {
        self.dir.join(id).join(RUNS_FILENAME)
    }

    /// Subfolder names that are safe ids, sorted.
    pub fn list_ids(&self) -> Result<Vec<String>, HostError> {
        let rd = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ids: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|id| is_safe_folder_id(id))
            .collect();
        ids.sort();
        Ok(ids)
    }

    fn fallback_created_at(path: &Path) -> i64 {
        let Ok(meta) = std::fs::metadata(path) else { return now_ms() };
        let stamp = meta.created().ok().or_else(|| meta.modified().ok());
        stamp
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .filter(|ms| *ms > 0)
            .unwrap_or_else(now_ms)
    }

    /// Raw file text, `None` when missing.
    fn read_raw(path: &Path) -> Result<Option<String>, HostError> {
        match std::fs::read_to_string(path) {
            Ok(raw) => Ok(Some(raw)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The parsed definition: `Ok(None)` when the folder has no config,
    /// `Err` when the file exists but is unusable.
    pub fn read_config(&self, id: &str) -> Result<Option<AutomationConfig>, HostError> {
        let path = self.config_path(id);
        let Some(raw) = Self::read_raw(&path)? else { return Ok(None) };
        parse_stored_config(&raw, Self::fallback_created_at(&path))
            .map(Some)
            .ok_or_else(|| HostError::invalid(format!("{} is not a usable automation.json", path.display())))
    }

    /// `writeConfig`. Runs embedded in a legacy `automation.json` are moved
    /// to `runs.json` first so the rewrite does not lose them.
    pub fn write_config(&self, id: &str, config: &AutomationConfig) -> Result<(), HostError> {
        if !self.runs_path(id).exists()
            && let Some(raw) = Self::read_raw(&self.config_path(id))?
        {
            let legacy = parse_legacy_embedded_runs(&raw);
            if !legacy.is_empty() {
                self.write_runs(id, &legacy)?;
            }
        }
        write_text_atomic(&self.config_path(id), &serialize_config(config))
    }

    /// `readRuns`: newest first, at most 20; falls back to runs embedded in a
    /// legacy `automation.json` when `runs.json` does not exist.
    pub fn read_runs(&self, id: &str) -> Vec<AutomationRun> {
        let mut runs: Vec<AutomationRun> = match Self::read_raw(&self.runs_path(id)) {
            Ok(Some(raw)) => serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|v| v.as_array().map(|entries| entries.iter().filter_map(parse_stored_run).collect()))
                .unwrap_or_default(),
            _ => Self::read_raw(&self.config_path(id)).ok().flatten().map(|raw| parse_legacy_embedded_runs(&raw)).unwrap_or_default(),
        };
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
        runs.truncate(AUTOMATION_MAX_RUN_HISTORY);
        runs
    }

    /// `writeRuns`.
    pub fn write_runs(&self, id: &str, runs: &[AutomationRun]) -> Result<(), HostError> {
        let mut text = serde_json::to_string_pretty(runs)?;
        text.push('\n');
        write_text_atomic(&self.runs_path(id), &text)
    }

    /// `toRecord` without a next-run time (the scheduler derives it).
    pub fn to_record(&self, id: &str, config: AutomationConfig) -> AutomationRecord {
        let runs = self.read_runs(id);
        AutomationRecord::from_config(id, config, None, runs, self.config_path(id).to_string_lossy().into_owned())
    }

    /// `listDefinitions`: every usable routine, oldest first. A corrupt
    /// config hides only its own routine.
    pub fn list(&self) -> Result<Vec<AutomationRecord>, HostError> {
        let mut out = Vec::new();
        for id in self.list_ids()? {
            match self.read_config(&id) {
                Ok(Some(config)) => out.push(self.to_record(&id, config)),
                Ok(None) => {}
                Err(e) => tracing::warn!(path = %self.config_path(&id).display(), error = %e, "skipping unreadable automation.json"),
            }
        }
        out.sort_by_key(|r| r.created_at);
        Ok(out)
    }

    /// One routine by folder id.
    pub fn get(&self, id: &str) -> Result<Option<AutomationRecord>, HostError> {
        if !is_safe_folder_id(id) {
            return Ok(None);
        }
        Ok(self.read_config(id)?.map(|config| self.to_record(id, config)))
    }

    /// `count`: usable definitions.
    pub fn count(&self) -> Result<usize, HostError> {
        Ok(self.list_ids()?.iter().filter(|id| matches!(self.read_config(id), Ok(Some(_)))).count())
    }

    /// `uniqueId`: the slug, then `-2`…`-999`, then `-<now>`.
    pub fn unique_id(&self, name: &str) -> Result<String, HostError> {
        let now = now_ms();
        let base = slugify_automation_name(name, now);
        let existing = self.list_ids()?;
        if !existing.contains(&base) {
            return Ok(base);
        }
        for suffix in 2..1_000 {
            let candidate = format!("{base}-{suffix}");
            if !existing.contains(&candidate) {
                return Ok(candidate);
            }
        }
        Ok(format!("{base}-{now}"))
    }

    /// Backwards-compatible alias of [`AutomationStore::unique_id`].
    pub fn allocate_id(&self, name: &str) -> Result<String, HostError> {
        self.unique_id(name)
    }

    /// `upsert` (create): enforces the per-agent cap and allocates the folder.
    /// The caller normalises the spec (name/prompt/trigger) first.
    pub fn create(&self, config: AutomationConfig) -> Result<AutomationRecord, HostError> {
        if self.count()? >= AUTOMATION_MAX_PER_AGENT {
            return Err(HostError::invalid(format!("this agent already has {AUTOMATION_MAX_PER_AGENT} routines; delete one first")));
        }
        let id = self.unique_id(&config.name)?;
        self.write_config(&id, &config)?;
        Ok(self.to_record(&id, config))
    }

    /// Overwrite a routine's definition.
    pub fn save(&self, id: &str, config: &AutomationConfig) -> Result<(), HostError> {
        if !is_safe_folder_id(id) {
            return Err(HostError::invalid(format!("invalid routine id {id:?}")));
        }
        self.write_config(id, config)
    }

    /// `setEnabled`: flips only the flag.
    pub fn set_enabled(&self, id: &str, is_enabled: bool) -> Result<Option<AutomationRecord>, HostError> {
        let Some(mut config) = self.get_config_safe(id)? else { return Ok(None) };
        if config.is_enabled != is_enabled {
            config.is_enabled = is_enabled;
            self.write_config(id, &config)?;
        }
        Ok(Some(self.to_record(id, config)))
    }

    /// `markNoticeRaised`.
    pub fn mark_notice_raised(&self, id: &str, notice: &str) -> Result<Option<AutomationRecord>, HostError> {
        if !is_routine_notice_id(notice) {
            return Ok(None);
        }
        let Some(mut config) = self.get_config_safe(id)? else { return Ok(None) };
        if !config.raised_notices.iter().any(|n| n == notice) {
            config.raised_notices.push(notice.to_owned());
            self.write_config(id, &config)?;
        }
        Ok(Some(self.to_record(id, config)))
    }

    /// `recordRun`: move `lastRunAt` to `at` (done before the run starts).
    pub fn record_run(&self, id: &str, at: i64) -> Result<Option<AutomationRecord>, HostError> {
        let Some(mut config) = self.get_config_safe(id)? else { return Ok(None) };
        config.last_run_at = Some(at);
        self.write_config(id, &config)?;
        Ok(Some(self.to_record(id, config)))
    }

    /// `beginRun`: insert a `running` entry at the head of `runs.json`. An
    /// existing run with the same id is returned untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_run(
        &self,
        id: &str,
        trigger: AutomationRunTrigger,
        at: i64,
        event: Option<&str>,
        run_id: Option<String>,
        coalesced_run_ids: Option<&[String]>,
    ) -> Result<Option<AutomationRun>, HostError> {
        if self.get_config_safe(id)?.is_none() {
            return Ok(None);
        }
        let run_id = run_id.unwrap_or_else(|| gns_core::RunId::new().0);
        let mut runs = self.read_runs(id);
        if let Some(existing) = runs.iter().find(|r| r.id == run_id) {
            return Ok(Some(existing.clone()));
        }
        let run = AutomationRun {
            id: run_id,
            trigger,
            started_at: at,
            finished_at: None,
            status: AutomationRunStatus::Running,
            detail: None,
            event: clamp_run_detail(event),
            coalesced_run_ids: clamp_coalesced_run_ids(coalesced_run_ids),
        };
        runs.insert(0, run.clone());
        runs.truncate(AUTOMATION_MAX_RUN_HISTORY);
        self.write_runs(id, &runs)?;
        Ok(Some(run))
    }

    /// `finishRun`: set the outcome of a run (unknown run ids are ignored).
    pub fn finish_run(
        &self,
        id: &str,
        run_id: &str,
        status: AutomationRunStatus,
        at: i64,
        detail: Option<&str>,
    ) -> Result<Option<AutomationRecord>, HostError> {
        let Some(config) = self.get_config_safe(id)? else { return Ok(None) };
        let mut runs = self.read_runs(id);
        if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
            run.finished_at = Some(at);
            run.status = status;
            if let Some(clamped) = clamp_run_detail(detail) {
                run.detail = Some(clamped);
            }
            self.write_runs(id, &runs)?;
        }
        Ok(Some(self.to_record(id, config)))
    }

    /// `remove`: delete the folder.
    pub fn delete(&self, id: &str) -> Result<bool, HostError> {
        if !is_safe_folder_id(id) {
            return Ok(false);
        }
        let dir = self.dir.join(id);
        if !dir.is_dir() {
            return Ok(false);
        }
        std::fs::remove_dir_all(dir)?;
        Ok(true)
    }

    fn get_config_safe(&self, id: &str) -> Result<Option<AutomationConfig>, HostError> {
        if !is_safe_folder_id(id) {
            return Ok(None);
        }
        self.read_config(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(name: &str) -> AutomationConfig {
        AutomationConfig {
            name: name.into(),
            prompt: "check".into(),
            trigger: Trigger::cron("0 9 * * 1-5"),
            is_enabled: true,
            created_at: 1,
            last_run_at: None,
            raised_notices: vec![],
        }
    }

    #[test]
    fn create_list_delete_with_unique_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutomationStore::new(dir.path().join("automations"));
        let a = store.create(cfg("Daily Digest")).unwrap();
        let b = store.create(cfg("Daily Digest")).unwrap();
        assert_eq!(a.id, "daily-digest");
        assert_eq!(b.id, "daily-digest-2");
        assert_eq!(a.schedule.as_deref(), Some("0 9 * * 1-5"));
        assert_eq!(a.trigger_description, "Weekdays at 9:00 AM");
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(store.count().unwrap(), 2);
        assert!(store.delete("daily-digest").unwrap());
        assert!(!store.delete("daily-digest").unwrap());
        assert!(!store.delete("../etc").unwrap());
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn writes_the_original_file_format() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutomationStore::new(dir.path().join("automations"));
        let mut config = cfg("Digest");
        config.created_at = 1_700_000_000_000;
        store.create(config).unwrap();
        let text = std::fs::read_to_string(store.config_path("digest")).unwrap();
        assert_eq!(
            text,
            "{\n  \"name\": \"Digest\",\n  \"prompt\": \"check\",\n  \"schedule\": \"0 9 * * 1-5\",\n  \"enabled\": true,\n  \"createdAt\": 1700000000000,\n  \"lastRunAt\": null\n}\n"
        );
        let listener = parse_stored_trigger(&json!({"type":"slack","channel":"#eng","match":{"kind":"mention"}})).unwrap();
        let mut config = cfg("Pings");
        config.trigger = listener;
        config.is_enabled = false;
        config.raised_notices = vec!["github-listener-scope".into()];
        store.create(config).unwrap();
        let text = std::fs::read_to_string(store.config_path("pings")).unwrap();
        assert_eq!(
            text,
            "{\n  \"name\": \"Pings\",\n  \"prompt\": \"check\",\n  \"trigger\": {\n    \"type\": \"slack\",\n    \"channel\": \"#eng\",\n    \"match\": {\n      \"kind\": \"mention\"\n    }\n  },\n  \"enabled\": false,\n  \"createdAt\": 1,\n  \"lastRunAt\": null,\n  \"raisedNotices\": [\n    \"github-listener-scope\"\n  ]\n}\n"
        );
        // Runs live in runs.json, newest first, capped at 20.
        for i in 0..25 {
            store.begin_run("digest", AutomationRunTrigger::Schedule, 1_000 + i, None, None, None).unwrap().unwrap();
        }
        let runs = store.read_runs("digest");
        assert_eq!(runs.len(), AUTOMATION_MAX_RUN_HISTORY);
        assert_eq!(runs[0].started_at, 1_024);
        let run_id = runs[0].id.clone();
        store.finish_run("digest", &run_id, AutomationRunStatus::Error, 2_000, Some(&"x".repeat(400))).unwrap();
        let back: Value = serde_json::from_str(&std::fs::read_to_string(store.runs_path("digest")).unwrap()).unwrap();
        let first = &back.as_array().unwrap()[0];
        assert_eq!(first["status"], "error");
        assert_eq!(first["finishedAt"], 2_000);
        assert_eq!(first["detail"].as_str().unwrap().len(), AUTOMATION_MAX_RUN_DETAIL_LENGTH);
        assert!(first.get("event").is_none() && first.get("coalescedRunIds").is_none());
        // Config writes never touch runs.json content.
        store.set_enabled("digest", false).unwrap();
        assert!(!std::fs::read_to_string(store.config_path("digest")).unwrap().contains("runs"));
        assert_eq!(store.read_runs("digest").len(), AUTOMATION_MAX_RUN_HISTORY);
    }

    #[test]
    fn reads_original_fixture_and_legacy_rust_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("automations");
        let store = AutomationStore::new(&root);
        std::fs::create_dir_all(root.join("standup")).unwrap();
        std::fs::write(
            root.join("standup").join(CONFIG_FILENAME),
            r#"{
  "name": "Standup",
  "prompt": "Post the standup.",
  "schedule": "CRON_TZ=America/New_York 0 9 * * 1-5",
  "enabled": true,
  "createdAt": 1700000000000,
  "lastRunAt": 1700100000000
}
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("standup").join(RUNS_FILENAME),
            r#"[
  { "id": "b", "trigger": "event", "startedAt": 20, "finishedAt": 21, "status": "ok", "event": "ann in #eng: \"hi\"", "coalescedRunIds": ["c", "d"] },
  { "id": "a", "trigger": "bogus", "startedAt": 10, "finishedAt": null, "status": "running" },
  { "startedAt": 5 }
]
"#,
        )
        .unwrap();
        let record = store.get("standup").unwrap().unwrap();
        assert_eq!(record.name, "Standup");
        assert_eq!(record.trigger, Trigger::cron("CRON_TZ=America/New_York 0 9 * * 1-5"));
        assert_eq!(record.trigger_description, "Weekdays at 9:00 AM (America/New_York)");
        assert_eq!((record.created_at, record.last_run_at), (1_700_000_000_000, Some(1_700_100_000_000)));
        assert_eq!(record.runs.len(), 2);
        assert_eq!(record.runs[0].coalesced_run_ids.as_deref(), Some(&["c".to_owned(), "d".to_owned()][..]));
        assert_eq!(record.runs[1].trigger, AutomationRunTrigger::Schedule);
        assert_eq!(record.runs[1].status, AutomationRunStatus::Running);

        // Legacy Rust layout: isEnabled, tagged trigger, embedded runs.
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::write(
            root.join("old").join(CONFIG_FILENAME),
            r#"{"name":"Old","prompt":"p","trigger":{"type":"cron","schedule":"@hourly"},"isEnabled":false,"createdAt":5,"lastRunAt":null,"runs":[{"id":"r","trigger":"schedule","startedAt":6,"finishedAt":7,"status":"ok"}]}"#,
        )
        .unwrap();
        let old = store.get("old").unwrap().unwrap();
        assert!(!old.is_enabled);
        assert_eq!(old.schedule.as_deref(), Some("@hourly"));
        assert_eq!(old.runs.len(), 1);
        // Enabling rewrites the file in the original format; the runs move to runs.json on the next run.
        store.set_enabled("old", true).unwrap();
        let text = std::fs::read_to_string(store.config_path("old")).unwrap();
        assert!(text.contains("\"enabled\": true") && text.contains("\"schedule\": \"@hourly\"") && !text.contains("isEnabled"));
        assert_eq!(store.read_runs("old").len(), 1, "legacy runs migrated to runs.json");
        assert!(store.runs_path("old").exists());
        store.begin_run("old", AutomationRunTrigger::Manual, 9, None, None, None).unwrap();
        assert_eq!(store.read_runs("old").len(), 2);
        // Notices, run stamps and flags.
        assert!(store.mark_notice_raised("old", "nope").unwrap().is_none());
        let marked = store.mark_notice_raised("old", "github-listener-scope").unwrap().unwrap();
        assert_eq!(marked.raised_notices, vec!["github-listener-scope".to_owned()]);
        assert_eq!(store.record_run("old", 42).unwrap().unwrap().last_run_at, Some(42));
        // createdAt never exceeds the file's own birth time.
        std::fs::create_dir_all(root.join("future")).unwrap();
        std::fs::write(
            root.join("future").join(CONFIG_FILENAME),
            r#"{"name":"F","prompt":"p","schedule":"@daily","createdAt":9999999999999}"#,
        )
        .unwrap();
        assert!(store.get("future").unwrap().unwrap().created_at < 9_999_999_999_999);
        assert_eq!(store.list().unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["old", "standup", "future"]);
    }

    #[test]
    fn corrupt_routine_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = AutomationStore::new(dir.path().join("automations"));
        store.create(cfg("Good")).unwrap();
        std::fs::create_dir_all(dir.path().join("automations").join("broken")).unwrap();
        std::fs::write(dir.path().join("automations").join("broken").join(CONFIG_FILENAME), "{ half").unwrap();
        std::fs::create_dir_all(dir.path().join("automations").join("webhook")).unwrap();
        std::fs::write(
            dir.path().join("automations").join("webhook").join(CONFIG_FILENAME),
            r#"{"name":"W","prompt":"p","trigger":{"type":"webhook","name":"x"},"isEnabled":true,"createdAt":1,"lastRunAt":null}"#,
        )
        .unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["good"]);
        assert!(store.get("broken").is_err(), "direct access still reports the corruption");
        assert!(store.get("webhook").is_err(), "the retired webhook trigger is unusable");
        assert_eq!(store.count().unwrap(), 1);
    }
}
