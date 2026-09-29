//! Newtype identifiers. All ids are UUID v4 strings so they stay readable in
//! prompts and on disk (`<root>/agents/<id>/`).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Generate a fresh random id.
            pub fn new() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }
            /// Borrow the id as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

string_id!(
    /// Identifier of a single agent (also its directory name under `agents/`).
    AgentId
);
string_id!(
    /// Identifier of a group chat (also its directory name under `agents/`).
    GroupId
);
string_id!(
    /// Identifier of one runner turn (a "request id" in the original design).
    RunId
);
string_id!(
    /// Identifier of a transcript entry.
    EntryId
);

/// Any addressable target of `SendToAgent`: an agent or a group.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TargetId {
    /// A single agent.
    Agent(AgentId),
    /// A group chat.
    Group(GroupId),
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetId::Agent(id) => f.write_str(id.as_str()),
            TargetId::Group(id) => f.write_str(id.as_str()),
        }
    }
}
