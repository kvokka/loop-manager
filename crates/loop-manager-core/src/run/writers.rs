//! A rig's `max_writers` (docs/design/config.md "Project", contracts 6.1): the runs that work in the rig's own
//! directory at once. A run counts by its formula and its base, not by the step it is at: it holds its place from
//! `preparing` (parked and interrupted too) to its terminal state.

use super::{RunHandler, bases, row::Row};
use crate::{
    config::{Project, Scope, StepKind},
    writer::{Error, Tx},
};
use rusqlite::params;

impl RunHandler {
    /// Whether run `id` of `formula` on `rig` works in the rig's own directory: it works neither in a worktree nor
    /// in a workdir inherited from elsewhere, and an exec step, or an ai step of an agent that is not of scope root,
    /// is among the steps of its formula. A formula or agent that does not resolve counts.
    fn writes(
        &self,
        tx: &Tx<'_>,
        project: &Project,
        id: &str,
        rig: &str,
        formula: &str,
        as_agent: Option<&str>,
    ) -> Result<bool, Error> {
        let Ok((_, f)) = project.formula(formula) else {
            return Ok(true);
        };
        let steps = f.steps.iter().any(|step| match step.kind {
            StepKind::Exec { .. } => true,
            StepKind::Ai { .. } => (self
                .config
                .for_run_as(&project.name, rig, formula, &step.name, as_agent))
            .ok()
            .is_none_or(|(_, config)| config.scope != Scope::Root),
            StepKind::Drain { .. } | StepKind::Formula { .. } | StepKind::Await { .. } => false,
        });
        Ok(steps && !bases::isolated(tx, project, id)?)
    }

    /// Whether the queued `row` works in its rig's directory and the rig already has `max_writers` runs that do.
    /// A run in a drain or formula step lends its place to the runs that step started, and to theirs down the chain.
    pub(super) fn rig_full(&self, tx: &Tx<'_>, row: &Row) -> Result<bool, Error> {
        let Some(project) = self.config.projects.iter().find(|p| p.name == row.project) else {
            return Ok(false);
        };
        let Some(limit) = project.rigs.get(&row.rig).and_then(|r| r.max_writers) else {
            return Ok(false);
        };
        if !self.writes(tx, project, &row.id, &row.rig, &row.formula, row.as_agent.as_deref())? {
            return Ok(false);
        }
        let mut stmt = tx.sql().prepare(
            "WITH RECURSIVE lender(id, parent) AS ( \
                 SELECT p.id, p.parent_run FROM runs c JOIN runs p ON p.id = c.parent_run \
                 WHERE c.id = ?3 AND p.step_kind IN ('drain', 'formula') \
                 UNION ALL \
                 SELECT p.id, p.parent_run FROM lender JOIN runs p ON p.id = lender.parent \
                 WHERE p.step_kind IN ('drain', 'formula')) \
             SELECT id, formula, as_agent FROM runs \
             WHERE project = ?1 AND rig = ?2 AND state NOT IN ('queued', 'passed', 'failed', 'cancelled') \
               AND id NOT IN (SELECT id FROM lender)",
        )?;
        let live = stmt.query_map(params![row.project, row.rig, row.id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let live: Vec<_> = live.collect::<Result<_, _>>()?;
        let mut writers = 0;
        for (id, formula, as_agent) in live {
            writers += u32::from(self.writes(tx, project, &id, &row.rig, &formula, as_agent.as_deref())?);
        }
        Ok(writers >= limit)
    }
}
