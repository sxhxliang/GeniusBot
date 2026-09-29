//! Durable file references and explicitly delegated work. File bytes never travel through an LLM.

use crate::{AgentId, EntryId, GroupId, HostError, OutboundMessage, RunId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: String,
    pub creator: AgentId,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "lowercase")]
pub enum ConversationRef {
    Agent(AgentId),
    Group(GroupId),
}

impl ConversationRef {
    pub fn grant_key(&self) -> String {
        match self {
            Self::Agent(id) => format!("agent:{id}"),
            Self::Group(id) => format!("group:{id}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Queued,
    Running,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DelegateTaskRequest {
    pub target_id: String,
    pub task: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
    #[serde(default)]
    pub require_files: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct CompleteTaskRequest {
    /// Only completed or failed is accepted. Completed means submitted, not independently reviewed.
    pub status: TaskStatus,
    pub summary: String,
    #[serde(default)]
    pub verification: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub artifact_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: String,
    pub requester: AgentId,
    pub executor: AgentId,
    pub origin: ConversationRef,
    pub origin_message: Option<EntryId>,
    pub instruction: String,
    pub require_files: bool,
    pub inputs: Vec<ArtifactRef>,
    pub artifacts: Vec<ArtifactRef>,
    pub status: TaskStatus,
    pub summary: String,
    pub verification: String,
    pub revision: u64,
    pub attempt: u64,
    /// Only an omitted submission may be continued by its already-running shell job.
    #[serde(default)]
    pub background_resume_allowed: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Original submission makes a retried completion independent of changes to its source files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<CompleteTaskRequest>,
}

impl TaskRecord {
    pub fn result_message(&self) -> OutboundMessage {
        let mut m = OutboundMessage::text(self.summary.clone());
        m.artifacts = self.artifacts.clone();
        m.task = Some(Box::new(self.clone()));
        m
    }
    pub fn check_executor(&self, agent: &AgentId) -> Result<(), HostError> {
        if &self.executor != agent {
            return Err(HostError::invalid("only the assigned executor may update this task"));
        }
        Ok(())
    }
}

/// Stable durable notification. Projection into a transcript and model wake are separately tracked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskDelivery {
    pub id: String,
    pub task: TaskRecord,
    pub target: ConversationRef,
    pub wake_agent: Option<AgentId>,
    pub projected: bool,
    pub wake_started: bool,
    #[serde(default)]
    pub wake_attempts: u32,
    #[serde(default)]
    pub retry_after: i64,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TaskRunContext {
    pub task_id: String,
    pub attempt: u64,
    pub run_id: RunId,
}

pub fn artifact_prompt(artifacts: &[ArtifactRef]) -> String {
    if artifacts.is_empty() {
        return String::new();
    }
    let mut s =
        "\nAttached files (use FetchArtifact with the artifact_id to obtain a local path; bytes are not in this message):".to_owned();
    for a in artifacts {
        s.push_str(&format!("\n- {}: artifact_id={}, {} bytes, SHA-256 {}", a.name, a.id, a.size, a.sha256));
    }
    s
}
