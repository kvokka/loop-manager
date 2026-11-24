//! The trackers of a config: the adapter of each project's tracker.

use loop_manager_core::{
    config::{Config, Project, TrackerLink},
    tracker::Adapter,
};
use std::path::Path;

/// The tracker adapters of this build: a tracker is a row here, and nothing else in the root knows its name.
pub(crate) const TRACKERS: &[&Adapter] = &[&loop_manager_br::ADAPTER];

/// The adapter of the tracker of the project in `dir`: the first that finds its tracker there.
pub(crate) fn of(dir: &Path) -> Option<&'static Adapter> {
    TRACKERS.iter().copied().find(|a| (a.link)(dir).present)
}

/// How the sessions and commands of the project in `dir` reach its tracker; with none, the link of the first adapter,
/// which still names its tracker explicitly.
pub(crate) fn link(dir: &Path) -> TrackerLink {
    (of(dir).unwrap_or(TRACKERS[0]).link)(dir)
}

/// The projects of `config` whose tracker `adapter` is.
pub(crate) fn projects<'a>(config: &'a Config, adapter: &Adapter) -> Vec<&'a Project> {
    (config.projects.iter())
        .filter(|p| of(&p.dir).is_some_and(|a| a.name == adapter.name))
        .collect()
}
