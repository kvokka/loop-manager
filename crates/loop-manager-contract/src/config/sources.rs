//! Packs and addons from git (docs/design/config.md, docs/guide/addons.md). The core never runs git: whoever loads
//! the config downloads each source first and passes the directory it went to, as [`Checkouts`].

use super::{
    files::{AddonDecl, AddonFile, OrchestratorFile, PackFile, ProjectFile, Source},
    items::{parent, resolve},
    load::Loader,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
};

/// A git repository as a config file writes it: pinned by `rev` to a tag or a full commit id; without it, the
/// latest revision.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct GitSource {
    pub git: String,
    pub rev: Option<String>,
}

impl Source {
    /// The repository of a source from git.
    fn git(&self) -> Option<GitSource> {
        match self {
            Source::Path(_) => None,
            Source::Git { git, rev, .. } => Some(GitSource {
                git: git.clone(),
                rev: rev.clone(),
            }),
        }
    }
}

/// Where each git source was downloaded to, or why it was not.
pub type Checkouts = BTreeMap<GitSource, Result<PathBuf, String>>;

/// A full lowercase hexadecimal commit id, SHA-1 or SHA-256.
pub fn commit_id(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The git sources the orchestrator file, its projects and their packs declare. A pack from git declares its addons
/// in a manifest that is read where `checkouts` has the pack, and an addon of a pack is no source when the
/// orchestrator file or the project's declares its name, which is known where `checkouts` has their addons: whoever
/// downloads asks again once those are there. Only the files are parsed; their errors are left to the load that
/// follows, which reports them all.
pub fn git_sources(orchestrator_file: &Path, checkouts: &Checkouts) -> BTreeSet<GitSource> {
    let mut loader = Loader {
        checkouts: checkouts.clone(),
        ..Loader::default()
    };
    let base = parent(orchestrator_file);
    let raw = match orchestrator_file.exists() {
        true => loader.read::<OrchestratorFile>(orchestrator_file).unwrap_or_default(),
        false => OrchestratorFile::default(),
    };
    let mut found = loader.git(orchestrator_file, &raw.addon);
    let outer = loader.names(orchestrator_file, &base, &raw.addon);
    for dir in raw.projects.iter().map(|d| resolve(&base, d)) {
        let file = dir.join("loop-manager.toml");
        let Some(project) = loader.read::<ProjectFile>(&file) else {
            continue;
        };
        found.extend(loader.git(&file, &project.addon));
        let own = loader.names(&file, &dir, &project.addon);
        for source in project.packs {
            let manifest = loader.root(&file, &dir, &source).map(|root| root.join("pack.toml"));
            if let (Some(outer), Some(own)) = (&outer, &own)
                && let Some(manifest) = manifest
                && let Some(mut pack) = loader.read::<PackFile>(&manifest)
            {
                let replaced = |d: &AddonDecl| d.name.as_ref().is_some_and(|n| outer.contains(n) || own.contains(n));
                pack.addon.retain(|d| !replaced(d));
                found.extend(loader.git(&manifest, &pack.addon));
            }
            found.extend(source.git());
        }
    }
    found
}

impl Loader {
    /// The git sources among the addons `file` declares.
    fn git(&mut self, file: &Path, decls: &[AddonDecl]) -> BTreeSet<GitSource> {
        let sources: Vec<Source> = decls.iter().filter_map(|d| self.addon_source(file, d)).collect();
        sources.iter().filter_map(Source::git).collect()
    }

    /// The names of the addons `file` declares, those a pack's declaration is replaced by; none while one of them
    /// from git is not in the checkouts, its name not known yet.
    fn names(&mut self, file: &Path, base: &Path, decls: &[AddonDecl]) -> Option<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        for decl in decls {
            let Some(source) = self.addon_source(file, decl) else {
                continue;
            };
            if source.git().is_some_and(|git| !self.checkouts.contains_key(&git)) {
                return None;
            }
            let manifest = self.root(file, base, &source).map(|root| root.join("addon.toml"));
            names.extend(manifest.and_then(|m| self.read::<AddonFile>(&m)).map(|a| a.name));
        }
        Some(names)
    }

    /// Where a declared addon is: exactly one of `path` and `git`, and `git` with `rev` (and `dir`, if any).
    pub(super) fn addon_source(&mut self, file: &Path, decl: &AddonDecl) -> Option<Source> {
        match (&decl.path, &decl.git, &decl.rev) {
            (Some(_), None, None) if decl.dir.is_some() => self.none(
                file,
                "`dir` is for an addon from git: a `path` names the addon's directory itself",
            ),
            (Some(path), None, None) => Some(Source::Path(path.clone())),
            (None, Some(git), Some(rev)) => Some(Source::Git {
                git: git.clone(),
                rev: Some(rev.clone()),
                dir: decl.dir.clone(),
            }),
            (None, Some(_), None) => self.none(
                file,
                "an addon from git needs `rev`, a tag or a full commit id: only a pack may follow the latest revision",
            ),
            _ => self.none(file, "an addon has either `path`, or `git` with `rev`"),
        }
    }

    /// The directory of a pack or addon that `file` in `base` declares: a path as is, a git source where it was
    /// downloaded to, or its `dir` there.
    pub(super) fn root(&mut self, file: &Path, base: &Path, source: &Source) -> Option<PathBuf> {
        let (repository, dir) = match source {
            Source::Path(path) => return Some(resolve(base, path)),
            Source::Git { dir, .. } => (source.git()?, dir),
        };
        let root = match self.checkouts.get(&repository) {
            Some(Ok(root)) => root.clone(),
            Some(Err(why)) => return self.none(file, format!("`{}`: {why}", repository.git)),
            None => return self.none(file, format!("`{}` is not downloaded", repository.git)),
        };
        match dir {
            None => Some(root),
            Some(dir) if dir.components().all(|c| matches!(c, Component::Normal(_))) => Some(root.join(dir)),
            Some(dir) => self.none(
                file,
                format!("`dir = \"{}\"` is not a path inside the repository", dir.display()),
            ),
        }
    }
}
