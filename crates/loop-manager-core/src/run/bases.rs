//! The base of a run (docs/design/config.md "Formula" and "Bundled addons"): the directory and branch it works from
//! in its place. The core knows the lineage and the config; the git of a base is the bundled `worktree`'s, which gets
//! these facts as its parameters. One answer for the start of a run, its count under `max_writers` and that addon.

use crate::{
    config::{Place, Project},
    writer::{Error, Tx},
};
use rusqlite::{OptionalExtension, params};
use std::path::PathBuf;

/// The step parent of a run whose parent is `parent`: the parent while it is in a `drain` or `formula` step, which
/// made the run; a run a session started has its parent in an `ai` step and inherits nothing. At the end of a child
/// of a `formula` step its parent is still in that step: it waits for the hooks of its `run.finishing`, `worktree`'s
/// among them, so the end of the child still finds its base.
pub(super) fn step_parent(tx: &Tx<'_>, parent: Option<&str>) -> Result<Option<String>, Error> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let kind: Option<String> = tx
        .sql()
        .query_row("SELECT step_kind FROM runs WHERE id = ?1", [parent], |r| r.get(0))
        .optional()?;
    let of_step = matches!(kind.as_deref(), Some("drain" | "formula"));
    Ok(of_step.then(|| parent.to_owned()))
}

/// Where a run on `rig` with the step parent `parent` starts: the directory of its base, and the ancestors whose lazy
/// worktree is that base, nearest first (see [`Place::lazy`]). A child on its parent's rig inherits the parent's
/// workdir. Else the walk goes up the step parents: one with `worktree = true` in its formula has a lazy base here;
/// one without stops the walk at the place's own directory, its base being the checkout; one on this rig gives its
/// workdir.
pub(super) fn base(
    tx: &Tx<'_>,
    project: &Project,
    rig: &str,
    parent: Option<String>,
) -> Result<(PathBuf, Vec<String>), Error> {
    let mut lazy = Vec::new();
    let mut up = parent;
    while let Some(id) = up {
        let (at, formula, workdir, next): (String, String, String, Option<String>) = tx.sql().query_row(
            "SELECT rig, formula, workdir, parent_run FROM runs WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        if at == rig {
            return Ok((PathBuf::from(workdir), lazy));
        }
        if !keyed(project, &formula) {
            break;
        }
        lazy.push(id);
        up = step_parent(tx, next.as_deref())?;
    }
    Ok((home(project, rig), lazy))
}

/// The place of run `id` as the bundled `worktree` takes it (see [`Place`]).
pub(super) fn place(tx: &Tx<'_>, project: &Project, id: &str) -> Result<Place, Error> {
    let (rig, formula, parent, continued) = facts(tx, id)?;
    // A continued run works where the session it continues did, which says nothing of a base.
    let (dir, lazy) = match continued {
        true => (home(project, &rig), Vec::new()),
        false => base(tx, project, &rig, step_parent(tx, parent.as_deref())?)?,
    };
    let merge = match continued {
        true => None,
        false => merged(tx, id, parent.as_deref())?,
    };
    // The key of a `drain` step gives its children their worktree: one, whatever their formula says.
    let own = keyed(project, &formula) || merge.is_some();
    let rigs = match keyed(project, &formula) {
        true => {
            let mut stmt = tx
                .sql()
                .prepare("SELECT DISTINCT rig FROM runs WHERE parent_run = ?1 AND rig != ?2 ORDER BY rig")?;
            let rigs = stmt.query_map(params![id, rig], |r| r.get::<_, String>(0))?;
            let rigs: Vec<String> = rigs.collect::<Result<_, _>>()?;
            rigs.iter()
                .filter_map(|r| Some(project.place(r)?.path.clone()))
                .collect()
        }
        false => Vec::new(),
    };
    Ok(Place {
        dir,
        lazy,
        own,
        continued,
        rigs,
        merge,
    })
}

/// The title of the bead of run `id` when its step parent is in a `drain` step with `worktree = true`, by the
/// step's config snapshot: the run merges into its base under it (see [`Place::merge`]). A bead not read yet has an
/// empty title.
fn merged(tx: &Tx<'_>, id: &str, parent: Option<&str>) -> Result<Option<String>, Error> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let title: Option<String> = tx
        .sql()
        .query_row(
            "SELECT COALESCE(json_extract(c.bead_data, '$.title'), '') FROM runs c, runs p \
             WHERE c.id = ?1 AND p.id = ?2 AND c.bead IS NOT NULL \
             AND p.step_kind = 'drain' AND json_extract(p.config, '$.worktree') = 1",
            params![id, parent],
            |r| r.get(0),
        )
        .optional()?;
    Ok(title)
}

/// Whether run `id` works apart from its place's own directory, so `max_writers` does not count it: in a worktree of
/// its own (by its formula or the `drain` step that made it), in a lazy base of an ancestor, or in a workdir it
/// inherited from an ancestor elsewhere. A continued run counts by its formula alone.
pub(super) fn isolated(tx: &Tx<'_>, project: &Project, id: &str) -> Result<bool, Error> {
    let (rig, formula, parent, continued) = facts(tx, id)?;
    let own = keyed(project, &formula);
    if own || continued {
        return Ok(own);
    }
    if merged(tx, id, parent.as_deref())?.is_some() {
        return Ok(true);
    }
    let (dir, lazy) = base(tx, project, &rig, step_parent(tx, parent.as_deref())?)?;
    Ok(!lazy.is_empty() || dir != home(project, &rig))
}

/// The rig, formula and parent of run `id`, and whether it continues another's session.
fn facts(tx: &Tx<'_>, id: &str) -> Result<(String, String, Option<String>, bool), Error> {
    let facts = tx.sql().query_row(
        "SELECT rig, formula, parent_run, continue_from IS NOT NULL FROM runs WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(facts)
}

/// The own directory of place `rig`.
fn home(project: &Project, rig: &str) -> PathBuf {
    project.place(rig).map(|r| r.path.clone()).unwrap_or_default()
}

/// Whether `formula` has `worktree = true`; a formula that does not resolve has not.
pub(super) fn keyed(project: &Project, formula: &str) -> bool {
    project.formula(formula).is_ok_and(|(_, f)| f.worktree)
}
