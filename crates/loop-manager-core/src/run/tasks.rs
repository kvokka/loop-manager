//! The task view of a project (docs/api/acp.md): the root beads with work under them and, per root, a tree of bead,
//! round and thread nodes built from the rows of `Reader::work`. Pure functions, no I/O.

use super::{WorkAttempt, WorkRun};
use crate::{driver::SessionRef, journal::rfc3339_ms};
use serde::Serialize;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashMap},
};

/// A bead as the view knows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BeadRef {
    pub title: String,
    pub parent: Option<String>,
}

/// What the run rows know of their beads (the newest run of a bead wins). The caller completes it with the
/// tracker's lineage for every `parent` that is not a key.
pub fn beads(runs: &[WorkRun]) -> BTreeMap<String, BeadRef> {
    runs.iter()
        .filter_map(|r| {
            let bead = BeadRef {
                title: r.bead_title.clone().unwrap_or_default(),
                parent: r.bead_parent.clone(),
            };
            Some((r.bead.clone()?, bead))
        })
        .collect()
}

/// A root bead with work under it.
#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub bead: String,
    pub title: String,
    pub updated_ms: i64,
    pub live: bool,
}

/// The root beads with work under them, most recently active first. `runs` and `attempts` are oldest first, as
/// `Reader::work` gives them.
pub fn tasks(runs: &[WorkRun], attempts: &[WorkAttempt], beads: &BTreeMap<String, BeadRef>) -> Vec<Task> {
    let mut tasks: BTreeMap<&str, Task> = BTreeMap::new();
    let mut root_of: HashMap<&str, &str> = HashMap::new();
    for p in placed(runs, beads) {
        let task = tasks.entry(p.root).or_insert_with(|| Task {
            bead: p.root.to_owned(),
            title: beads.get(p.root).map(|b| b.title.clone()).unwrap_or_default(),
            updated_ms: i64::MIN,
            live: false,
        });
        task.updated_ms = task.updated_ms.max(p.run.created_ms);
        task.live |= p.run.live;
        root_of.insert(&p.run.id, p.root);
    }
    for a in attempts {
        if let Some(task) = root_of.get(a.run.as_str()).and_then(|root| tasks.get_mut(root)) {
            task.updated_ms = task.updated_ms.max(a.started_ms).max(a.ended_ms.unwrap_or(i64::MIN));
        }
    }
    let mut tasks: Vec<Task> = tasks.into_values().collect();
    tasks.sort_by(|a, b| (Reverse(a.updated_ms), &a.bead).cmp(&(Reverse(b.updated_ms), &b.bead)));
    tasks
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Bead,
    Round,
    Thread,
}

/// The session of a thread node.
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub harness: String,
    pub session: SessionRef,
    pub workdir: Option<String>,
    /// The prompt keys of every attempt of the thread, in order.
    pub prompts: Vec<String>,
}

/// A node of a task's tree; `parent` `None` is the root bead itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub kind: Kind,
    pub title: String,
    pub live: bool,
    pub at_ms: i64,
    pub thread: Option<Thread>,
}

/// The nodes under the root bead `root`, parents before children (preorder), siblings by `at_ms` then id. Empty
/// when `root` is not a task. `runs` and `attempts` are oldest first, as `Reader::work` gives them.
pub fn tree(root: &str, runs: &[WorkRun], attempts: &[WorkAttempt], beads: &BTreeMap<String, BeadRef>) -> Vec<Node> {
    let mut tree = Tree::default();
    let mut anchors: HashMap<&str, (&WorkRun, Option<String>)> = HashMap::new();
    for p in placed(runs, beads).into_iter().filter(|p| p.root == root) {
        let anchor = match p.parent {
            Some((parent, m, parent_bead)) => {
                let (parent_run, up) = &anchors[parent];
                let round = tree.round(parent_run, m, up.clone());
                let chain = path(parent_bead, p.bead, beads).into_iter();
                Some(chain.fold(round, |up, bead| {
                    tree.bead(format!("b:{parent}.{m}:{bead}"), Some(up), bead, beads)
                }))
            }
            None => path(root, p.bead, beads)
                .into_iter()
                .fold(None, |up, bead| Some(tree.bead(format!("b:{bead}"), up, bead, beads))),
        };
        anchors.insert(&p.run.id, (p.run, anchor));
    }
    for a in attempts {
        if let Some((run, anchor)) = anchors.get(a.run.as_str()) {
            let round = tree.round(run, a.round, anchor.clone());
            tree.thread(round, a);
        }
    }
    tree.children(None).into_iter().flatten().collect()
}

/// A run in the view, oldest first.
struct Placed<'a> {
    run: &'a WorkRun,
    /// Its own bead, else its parent run's.
    bead: &'a str,
    /// The parent run in the view, the parent's round when the run was created and the parent's bead.
    parent: Option<(&'a str, u32, &'a str)>,
    /// The root bead of the task the run belongs to.
    root: &'a str,
}

/// The runs that have a bead, own or inherited: a run under a parent with one belongs to the parent's task, any
/// other run to the root of its own bead.
fn placed<'a>(runs: &'a [WorkRun], beads: &'a BTreeMap<String, BeadRef>) -> Vec<Placed<'a>> {
    let mut seen: HashMap<&str, (&str, &str)> = HashMap::new();
    let mut out = Vec::new();
    for run in runs {
        let parent = run.parent_run.as_deref().and_then(|p| Some((p, *seen.get(p)?)));
        let (bead, parent, root) = match (parent, run.parent_round) {
            (Some((p, (parent_bead, root))), Some(m)) => (
                run.bead.as_deref().unwrap_or(parent_bead),
                Some((p, m, parent_bead)),
                root,
            ),
            _ => {
                let Some(bead) = run.bead.as_deref() else { continue };
                (bead, None, line(bead, beads).last().copied().unwrap_or(bead))
            }
        };
        seen.insert(&run.id, (bead, root));
        out.push(Placed {
            run,
            bead,
            parent,
            root,
        });
    }
    out
}

/// `bead` and its ancestors in `beads`, upwards.
fn line<'a>(bead: &'a str, beads: &'a BTreeMap<String, BeadRef>) -> Vec<&'a str> {
    let mut line = vec![bead];
    let mut at = bead;
    while let Some(up) = beads
        .get(at)
        .and_then(|b| b.parent.as_deref())
        .filter(|up| beads.contains_key(*up))
    {
        if line.contains(&up) {
            break;
        }
        line.push(up);
        at = up;
    }
    line
}

/// The beads from below `from` down to `to`: the tracker chain when `from` is an ancestor of `to`, else `to`
/// alone; nothing when they are equal.
fn path<'a>(from: &str, to: &'a str, beads: &'a BTreeMap<String, BeadRef>) -> Vec<&'a str> {
    let line = line(to, beads);
    match line.iter().position(|b| *b == from) {
        Some(i) => line[..i].iter().rev().copied().collect(),
        None => vec![to],
    }
}

/// The nodes being built, each with the title it takes when a sibling has the same one.
#[derive(Default)]
struct Tree {
    nodes: HashMap<String, (Node, String)>,
    kids: HashMap<Option<String>, Vec<String>>,
}

impl Tree {
    fn add(&mut self, node: Node, alt: String) -> String {
        let id = node.id.clone();
        if !self.nodes.contains_key(&id) {
            self.kids.entry(node.parent.clone()).or_default().push(id.clone());
            self.nodes.insert(id.clone(), (node, alt));
        }
        id
    }

    fn bead(&mut self, id: String, parent: Option<String>, bead: &str, beads: &BTreeMap<String, BeadRef>) -> String {
        let title = match beads.get(bead) {
            Some(b) if !b.title.is_empty() => format!("{bead} {}", b.title),
            _ => bead.to_owned(),
        };
        let node = Node {
            id,
            parent,
            kind: Kind::Bead,
            title: title.clone(),
            live: false,
            at_ms: 0,
            thread: None,
        };
        self.add(node, title)
    }

    fn round(&mut self, run: &WorkRun, n: u32, parent: Option<String>) -> String {
        let created = rfc3339_ms(run.created_ms);
        let node = Node {
            id: format!("r:{}.{n}", run.id),
            parent,
            kind: Kind::Round,
            title: format!("Round {n}"),
            live: run.live && run.round == n,
            at_ms: 0,
            thread: None,
        };
        self.add(
            node,
            format!("Round {n} · {} {} · {}", &created[..10], &created[11..16], run.id),
        )
    }

    /// Adds attempt `a` to the thread of its session under `round`.
    fn thread(&mut self, round: String, a: &WorkAttempt) {
        let live = a.ended_ms.is_none();
        let same = |id: &&String| {
            self.nodes[*id]
                .0
                .thread
                .as_ref()
                .is_some_and(|t| t.session == a.session)
        };
        let found = self
            .kids
            .get(&Some(round.clone()))
            .into_iter()
            .flatten()
            .find(same)
            .cloned();
        if let Some((node, _)) = found.and_then(|id| self.nodes.get_mut(&id)) {
            node.live |= live;
            node.thread
                .iter_mut()
                .for_each(|t| t.prompts.extend(a.prompts.iter().cloned()));
            return;
        }
        let title = format!("{} ({})", a.step, a.harness);
        let alt = format!("{title} · {}", &rfc3339_ms(a.started_ms)[11..19]);
        let thread = Thread {
            harness: a.harness.clone(),
            session: a.session.clone(),
            workdir: a.workdir.clone(),
            prompts: a.prompts.clone(),
        };
        let node = Node {
            id: format!("t:{}.{}", a.run, a.n),
            parent: Some(round),
            kind: Kind::Thread,
            title,
            live,
            at_ms: a.started_ms,
            thread: Some(thread),
        };
        self.add(node, alt);
    }

    /// The subtrees under `parent` that hold a thread, in order, each in preorder, with sibling titles told apart.
    fn children(&self, parent: Option<&str>) -> Vec<Vec<Node>> {
        let kids = self.kids.get(&parent.map(str::to_owned)).into_iter().flatten();
        let mut subs: Vec<Vec<Node>> = kids.filter_map(|id| self.subtree(id)).collect();
        subs.sort_by(|a, b| (a[0].at_ms, &a[0].id).cmp(&(b[0].at_ms, &b[0].id)));
        let titles: Vec<String> = subs.iter().map(|s| s[0].title.clone()).collect();
        for node in subs.iter_mut().map(|s| &mut s[0]) {
            if titles.iter().filter(|t| **t == node.title).count() > 1 {
                node.title.clone_from(&self.nodes[&node.id].1);
            }
        }
        subs
    }

    /// `id` and its descendants in preorder; `None` when nothing under it is a thread.
    fn subtree(&self, id: &str) -> Option<Vec<Node>> {
        let mut node = self.nodes[id].0.clone();
        if node.kind == Kind::Thread {
            return Some(vec![node]);
        }
        let subs = self.children(Some(id));
        node.at_ms = subs.first()?[0].at_ms;
        if node.kind == Kind::Bead {
            node.live = subs.iter().any(|s| s[0].live);
        }
        Some(std::iter::once(node).chain(subs.into_iter().flatten()).collect())
    }
}

#[cfg(test)]
mod tests;
