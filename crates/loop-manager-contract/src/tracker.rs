//! The tracker interface (docs/design/contracts.md 5): the operations the core calls on the
//! tracker of a project. The daemon is the only claimer (invariant 3); the state of the orchestrator is the truth of
//! ownership and the assignee in the tracker mirrors it (docs/design/contracts.md 5).

use crate::BoxFuture;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A bead (contracts 5.2); `epic` is computed by the adapter, the rig of a bead is the prefix of its id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bead {
    pub id: String,
    pub title: String,
    pub description: String,
    /// The tracker's status as text: `open`, `in_progress`, `blocked`, `deferred`, `closed`, or one the project's
    /// tracker policy adds (such as `rework`); the core decides nothing by it.
    pub status: String,
    pub priority: i64,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub epic: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}

/// A comment on a bead (contracts 5.2).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comment {
    pub author: String,
    pub created_at: String,
    pub text: String,
}

/// A bead with its comments, oldest first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Commented {
    #[serde(flatten)]
    pub bead: Bead,
    pub comments: Vec<Comment>,
}

/// What a prompt shows of a bead beyond its fields (contracts 5.2): its comments, and its parents with theirs, from
/// the root down to its own parent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lineage {
    #[serde(flatten)]
    pub bead: Commented,
    pub parents: Vec<Commented>,
}

/// One write to the tracker (contracts 5.2): `seq` orders the writes and is the cursor of `changes`; `actor` is who
/// made it (`loop-manager:<run id>[:<attempt n>]` for loop-manager and its sessions); `changed` names the fields of an `updated`.
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub seq: i64,
    /// `created`, `updated` or `closed`.
    pub kind: String,
    pub actor: String,
    pub changed: Vec<String>,
    /// The bead as it is when the change is read.
    pub bead: Bead,
}

/// An open bead under a root and what holds it (contracts 5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    /// The waiting bead: under the root at any depth (not the root), with any status but `closed` and `tombstone`.
    pub bead: String,
    /// What holds it: an open blocker (any status but `closed` and `tombstone`) of a blocking dependency of the bead or
    /// of one of its parents at any height, the root and the beads above it included; or the bead itself or one of
    /// its parents when its status is `blocked`.
    pub blocker: String,
    /// The type of that dependency, `blocks`, `conditional-blocks` or `waits-for`; `status` when a `blocked` status
    /// holds it.
    pub kind: String,
    /// The status of the blocker.
    pub status: String,
    /// Whether the blocker is the root or under it.
    pub under: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrackerError {
    NotFound,
    /// Someone else holds the bead: told by the state of the bead after the call.
    Claimed {
        holder: String,
    },
    /// A conditional release or close found another owner; nothing changed.
    OwnerMismatch {
        holder: Option<String>,
    },
    /// Transient: the effect queue retries it with backoff.
    Unavailable {
        text: String,
    },
    /// Permanent.
    Invalid {
        text: String,
    },
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such bead"),
            Self::Claimed { holder } => write!(f, "claimed by `{holder}`"),
            Self::OwnerMismatch { holder } => write!(f, "held by `{}`", holder.as_deref().unwrap_or("nobody")),
            Self::Unavailable { text } => write!(f, "tracker unavailable: {text}"),
            Self::Invalid { text } => write!(f, "invalid: {text}"),
        }
    }
}

/// The operations of contracts 5.1, those the core calls; `owner` is `loop-manager:<run id>`.
pub trait Tracker: Send + Sync {
    fn claim<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<Bead, TrackerError>>;
    /// Conditional on `assignee == owner`; clears the assignee and sets `open`, but a closed bead stays closed.
    fn release<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Conditional on `assignee == owner`.
    fn close<'a>(&'a self, owner: &'a str, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Every ready bead, in the tracker's order (the core picks within an epic itself).
    fn ready(&self) -> BoxFuture<'_, Result<Vec<Bead>, TrackerError>>;
    /// The ids of the ready beads under `root`, at any depth, in the tracker's order; `NotFound` with no `root`.
    fn under<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<String>, TrackerError>>;
    /// The open beads under `root` that wait, each with what holds it: one entry per bead, blocker and kind, ordered
    /// by them; `NotFound` with no `root`.
    fn waiting<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<Wait>, TrackerError>>;
    /// The writes after `after` (a `seq`, 0 for all), oldest first.
    fn changes(&self, after: i64) -> BoxFuture<'_, Result<Vec<Change>, TrackerError>>;
    /// The root bead of a task (docs/design/contracts.md 5): the bead whose id is exactly `description`, else an open
    /// epic without labels created as `actor`, once per `key` — a repeat returns the bead of the first call.
    fn create<'a>(
        &'a self,
        actor: &'a str,
        key: &'a str,
        title: &'a str,
        description: &'a str,
    ) -> BoxFuture<'a, Result<Bead, TrackerError>>;
    /// The bead `id` with its comments and its parents with theirs (contracts 5.1); a parent the tracker no longer
    /// has ends the line, `NotFound` with no `id`.
    fn lineage<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Lineage, TrackerError>>;
    /// Appends the end of an attempt on the bead `id` to the tracker's audit trail as `actor`
    /// (`loop-manager:<run>:<attempt>`), with `entry`, a JSON object of what the attempt did (contracts 5.1). No write
    /// of the bead: `changes` sees nothing of it. Not idempotent: a repeated call appends the entry again.
    fn attempt_ended<'a>(
        &'a self,
        actor: &'a str,
        id: &'a str,
        entry: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Adds the comment `text` to the bead `id` as `actor` (`loop-manager:<run>:<attempt>`, contracts 5.1). Not
    /// idempotent: a repeated call adds it again.
    fn comment<'a>(&'a self, actor: &'a str, id: &'a str, text: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
}
