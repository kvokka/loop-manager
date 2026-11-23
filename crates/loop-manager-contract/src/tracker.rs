//! The tracker interface (docs/design/contracts.md 5): the operations the core calls on the
//! tracker of a project. The daemon is the only claimer (invariant 3); the state of the orchestrator is the truth of
//! ownership and the assignee in the tracker mirrors it (docs/design/contracts.md 5).

use crate::{
    BoxFuture,
    config::{Project, TrackerLink},
    driver::Check,
};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path, sync::Arc};

/// A watch of a tracker: it lasts as long as this value.
pub type Watch = Box<dyn Send>;
/// What a watch calls at a write to its tracker.
pub type OnChange = Box<dyn Fn() + Send>;

/// A tracker adapter (docs/design/adapters.md "Adding a tracker"): everything the root of the build knows of one
/// tracker. The tracker of a project is that of the first adapter of the build whose `link` finds it in the project.
pub struct Adapter {
    /// The tracker's name: the binary mise resolves.
    pub name: &'static str,
    /// The versions of the binary the adapter is tested on, oldest and newest, inclusive; the daemon and `doctor` warn
    /// outside them.
    pub tested: [(u32, u32, u32); 2],
    /// How the sessions and commands of the project in the directory `dir` reach this tracker; `present` when the
    /// project has it.
    pub link: fn(dir: &Path) -> TrackerLink,
    /// The tracker of the project in `dir`, through `binary`.
    pub new: fn(binary: &Path, dir: &Path) -> Arc<dyn Tracker>,
    /// Calls `on_change` at writes to the tracker of the project in `dir` as long as the returned watch lives; a lost
    /// or extra call costs nothing, the reader goes by its cursor.
    pub watch: fn(dir: &Path, on_change: OnChange) -> Result<Watch, String>,
    /// The daemon's start, with the orchestrator config file `config` and the `projects` that have this tracker: the
    /// files it writes for the tracker's own tools; its warnings.
    pub start: fn(binary: &Path, config: &Path, projects: &[&Project]) -> Vec<String>,
    /// The checks of `loop-manager doctor` for the `projects` that have this tracker, beyond the binary's version.
    pub doctor: fn(binary: &Path, projects: &[&Project]) -> Vec<Check>,
}

/// An issue (contracts 5.2); `epic` is computed by the adapter, the rig of an issue is the prefix of its id.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Issue {
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

/// A comment on an issue (contracts 5.2).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Comment {
    pub author: String,
    pub created_at: String,
    pub text: String,
}

/// An issue with its comments, oldest first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Commented {
    #[serde(flatten)]
    pub issue: Issue,
    pub comments: Vec<Comment>,
}

/// What a prompt shows of an issue beyond its fields (contracts 5.2): its comments, and its parents with theirs, from
/// the root down to its own parent.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lineage {
    #[serde(flatten)]
    pub issue: Commented,
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
    /// The issue as it is when the change is read.
    pub issue: Issue,
}

/// An open issue under a root and what holds it (contracts 5.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    /// The waiting issue: under the root at any depth (not the root), with any status but `closed` and `tombstone`.
    pub issue: String,
    /// What holds it: an open blocker (any status but `closed` and `tombstone`) of a blocking dependency of the issue or
    /// of one of its parents at any height, the root and the issues above it included; or the issue itself or one of
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
    /// Someone else holds the issue: told by the state of the issue after the call.
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
            Self::NotFound => f.write_str("no such issue"),
            Self::Claimed { holder } => write!(f, "claimed by `{holder}`"),
            Self::OwnerMismatch { holder } => write!(f, "held by `{}`", holder.as_deref().unwrap_or("nobody")),
            Self::Unavailable { text } => write!(f, "tracker unavailable: {text}"),
            Self::Invalid { text } => write!(f, "invalid: {text}"),
        }
    }
}

/// The operations of contracts 5.1, those the core calls; `owner` is `loop-manager:<run id>`.
pub trait Tracker: Send + Sync {
    fn claim<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<Issue, TrackerError>>;
    /// Conditional on `assignee == owner`; clears the assignee and sets `open`, but a closed issue stays closed.
    fn release<'a>(&'a self, owner: &'a str, id: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Conditional on `assignee == owner`.
    fn close<'a>(&'a self, owner: &'a str, id: &'a str, reason: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Every ready issue, in the tracker's order (the core picks within an epic itself).
    fn ready(&self) -> BoxFuture<'_, Result<Vec<Issue>, TrackerError>>;
    /// The ids of the ready issues under `root`, at any depth, in the tracker's order; `NotFound` with no `root`.
    fn under<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<String>, TrackerError>>;
    /// The open issues under `root` that wait, each with what holds it: one entry per issue, blocker and kind, ordered
    /// by them; `NotFound` with no `root`.
    fn waiting<'a>(&'a self, root: &'a str) -> BoxFuture<'a, Result<Vec<Wait>, TrackerError>>;
    /// The writes after `after` (a `seq`, 0 for all), oldest first.
    fn changes(&self, after: i64) -> BoxFuture<'_, Result<Vec<Change>, TrackerError>>;
    /// The root issue of a task (docs/design/contracts.md 5): the issue whose id is exactly `description`, else an open
    /// epic without labels created as `actor`, once per `key` — a repeat returns the issue of the first call.
    fn create<'a>(
        &'a self,
        actor: &'a str,
        key: &'a str,
        title: &'a str,
        description: &'a str,
    ) -> BoxFuture<'a, Result<Issue, TrackerError>>;
    /// The issue `id` with its comments and its parents with theirs (contracts 5.1); a parent the tracker no longer
    /// has ends the line, `NotFound` with no `id`.
    fn lineage<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Lineage, TrackerError>>;
    /// Appends the end of an attempt on the issue `id` to the tracker's audit trail as `actor`
    /// (`loop-manager:<run>:<attempt>`), with `entry`, a JSON object of what the attempt did (contracts 5.1). No write
    /// of the issue: `changes` sees nothing of it. Not idempotent: a repeated call appends the entry again.
    fn attempt_ended<'a>(
        &'a self,
        actor: &'a str,
        id: &'a str,
        entry: &'a serde_json::Value,
    ) -> BoxFuture<'a, Result<(), TrackerError>>;
    /// Adds the comment `text` to the issue `id` as `actor` (`loop-manager:<run>:<attempt>`, contracts 5.1). Not
    /// idempotent: a repeated call adds it again.
    fn comment<'a>(&'a self, actor: &'a str, id: &'a str, text: &'a str) -> BoxFuture<'a, Result<(), TrackerError>>;
}
