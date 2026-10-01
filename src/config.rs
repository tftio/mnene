//! Configuration resolution and project discovery for `mnene`.
//!
//! `Config::resolve` applies a fixed resolution order to the
//! raw optional values the process edge collected (`RawConfig`) plus a
//! starting directory. `scope` is resolved through a tiered order that ends
//! in the project's identity, not the checkout's name: `MNENE_SCOPE`, then
//! `tftio_lib::project` resolution (a `.clanker` declaration, a registry
//! path, a registry remote, or a slug derived from an unregistered remote),
//! then `CLANKER_SESSION_PROJECT`, then the name of the git repository
//! containing the working directory, then absent. Which tier answered is
//! recorded as [`crate::model::ScopeSource`].
//!
//! This module never calls `std::env`; every raw value arrives as an
//! argument, per RS-008.
//!
//! `Config` itself stays plain data with no accessor methods; validation
//! that composes it with [`crate::model::Provenance`] and
//! [`crate::model::MneneError`] (`MissingAgent`, `MissingScope`) lives in
//! [`crate::config::Config::require_agent`],
//! [`crate::config::Config::require_scope`], and
//! [`crate::config::Config::provenance`].

use std::fs;
use std::path::{Path, PathBuf};

use tftio_lib::project::{
    ProjectRoot, Source, default_registry_dir, discover_inputs, load_registry, project_root,
    repository_name, resolve as resolve_project,
};

use crate::model::ScopeSource;

/// The default context name used when neither `MNENE_CONTEXT` nor
/// `CLANKER_SESSION_CONTEXT` is set.
const DEFAULT_CONTEXT: &str = "default";

/// The raw optional values the process edge collected, before resolution.
///
/// Every field mirrors one environment variable or clap argument. This
/// struct carries no defaulting logic of its own; [`Config::resolve`]
/// applies the resolution order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RawConfig {
    /// `MNENE_AGENT`.
    pub agent: Option<String>,
    /// `MNENE_CONTEXT`.
    pub context: Option<String>,
    /// `MNENE_SESSION`.
    pub session: Option<String>,
    /// `MNENE_SCOPE`.
    pub scope: Option<String>,
    /// `MNENE_TASK`.
    pub task: Option<String>,
    /// `--db`, or `MNENE_DB` as a fallback when the flag is absent.
    pub db: Option<PathBuf>,
    /// `CLANKER_SESSION_HARNESS`, clanker's fallback for `agent`.
    pub clanker_session_harness: Option<String>,
    /// `CLANKER_SESSION_CONTEXT`, clanker's fallback for `context`.
    pub clanker_session_context: Option<String>,
    /// `CLANKER_SESSION_ID`, clanker's fallback for `session`.
    pub clanker_session_id: Option<String>,
    /// `CLANKER_SESSION_PROJECT`, clanker's fallback for `scope` when
    /// `tftio_lib::project` resolution finds nothing. clanker never sets
    /// `MNENE_SCOPE` itself (2026-09-15), so this is a real, distinct tier
    /// rather than a second copy of the same value.
    pub clanker_session_project: Option<String>,
    /// `XDG_DATA_HOME`, used to build the default `db` path.
    pub xdg_data_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`, used to locate the installed project registry
    /// (`${XDG_CONFIG_HOME:-~/.config}/tftio/projects.toml`).
    pub xdg_config_home: Option<PathBuf>,
    /// `HOME`, used to build the default `db` path when `XDG_DATA_HOME` is
    /// absent, and as the registry directory fallback and `~` expansion
    /// base when `XDG_CONFIG_HOME` is absent.
    pub home: Option<PathBuf>,
}

/// Resolved configuration for one `mnene` invocation.
///
/// Fields are plain data with no I/O or defaulting logic of their own;
/// [`Config::require_agent`], [`Config::require_scope`], and
/// [`Config::provenance`] are the accessor methods that validate `agent`
/// and `scope`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The agent attributed to writes made in this invocation, if known.
    pub agent: Option<String>,
    /// The context selecting which per-context database to use.
    pub context: String,
    /// The session identifier recorded as provenance on writes, if known.
    pub session: Option<String>,
    /// The scope bounding `search` and `recall`, if known.
    pub scope: Option<String>,
    /// Which tier resolved `scope`, if any resolved it.
    pub scope_source: Option<ScopeSource>,
    /// The task identifier recorded as provenance, if known.
    pub task: Option<String>,
    /// The path to the `SQLite` database file for this context.
    pub db: PathBuf,
}

impl Config {
    /// Returns the configured agent, or [`crate::model::MneneError::MissingAgent`] when
    /// neither `MNENE_AGENT` nor `CLANKER_SESSION_HARNESS` resolved to one.
    ///
    /// # Errors
    ///
    /// Returns [`crate::model::MneneError::MissingAgent`] when `self.agent` is `None`.
    pub fn require_agent(&self) -> Result<&str, crate::model::MneneError> {
        self.agent
            .as_deref()
            .ok_or(crate::model::MneneError::MissingAgent)
    }

    /// Returns the configured scope, or [`crate::model::MneneError::MissingScope`] when
    /// no tier resolved one.
    ///
    /// # Errors
    ///
    /// Returns [`crate::model::MneneError::MissingScope`] when `self.scope` is `None`.
    pub fn require_scope(&self) -> Result<&str, crate::model::MneneError> {
        self.scope
            .as_deref()
            .ok_or(crate::model::MneneError::MissingScope)
    }

    /// Builds [`Provenance`](crate::model::Provenance) for a write verb,
    /// requiring both an agent and a scope.
    ///
    /// # Errors
    ///
    /// Returns [`crate::model::MneneError::MissingAgent`] or [`crate::model::MneneError::MissingScope`]
    /// under the same conditions as [`Config::require_agent`] and
    /// [`Config::require_scope`], agent checked first.
    pub fn provenance(&self) -> Result<crate::model::Provenance, crate::model::MneneError> {
        let agent = self.require_agent()?.to_string();
        let scope = self.require_scope()?.to_string();
        Ok(crate::model::Provenance {
            agent,
            context: self.context.clone(),
            session: self.session.clone(),
            scope,
            scope_source: self.scope_source,
            task: self.task.clone(),
        })
    }

    /// Resolve a [`Config`] from `raw` and the directory a walk for project
    /// and branch discovery should start from, applying the resolution
    /// order in the module documentation.
    #[must_use]
    pub fn resolve(raw: RawConfig, start_dir: &Path) -> Self {
        let agent = raw.agent.or(raw.clanker_session_harness);
        let context = raw
            .context
            .or(raw.clanker_session_context)
            .unwrap_or_else(|| DEFAULT_CONTEXT.to_string());
        let session = raw.session.or(raw.clanker_session_id);

        let (scope, scope_source) = resolve_scope(
            raw.scope.as_deref(),
            start_dir,
            raw.clanker_session_project.as_deref(),
            raw.xdg_config_home.as_deref(),
            raw.home.as_deref(),
        );

        let branch = read_worktree_branch(start_dir);
        let task = raw.task.or(branch);

        let db = raw
            .db
            .unwrap_or_else(|| default_db_path(raw.xdg_data_home, raw.home, &context, start_dir));

        Self {
            agent,
            context,
            session,
            scope,
            scope_source,
            task,
            db,
        }
    }
}

/// Build the default database path from `XDG_DATA_HOME`, else `HOME`, else
/// (when neither is given) a path under `start_dir`.
///
/// A directory relative to the current working directory is not an
/// acceptable fallback for `mnene/<context>.db`, since the database would
/// silently move whenever the process is launched from a different
/// directory; `<start_dir>/.mnene/<context>.db` at least ties the fallback
/// to the directory `Config::resolve` was explicitly given.
fn default_db_path(
    xdg_data_home: Option<PathBuf>,
    home: Option<PathBuf>,
    context: &str,
    start_dir: &Path,
) -> PathBuf {
    let file_name = format!("{context}.db");
    if let Some(xdg) = xdg_data_home {
        xdg.join("mnene").join(file_name)
    } else if let Some(home) = home {
        home.join(".local/share/mnene").join(file_name)
    } else {
        start_dir.join(".mnene").join(file_name)
    }
}

/// Resolve `scope` for one invocation, applying the tier order: an explicit
/// `MNENE_SCOPE`; else `tftio_lib::project` resolution against the
/// installed registry (a `.clanker` declaration, a registered path, a
/// registered remote, or a slug derived from an unregistered remote); else
/// a non-empty `CLANKER_SESSION_PROJECT`; else the name of the git
/// repository containing `start_dir`; else neither.
///
/// A failure discovering `tftio_lib::project` inputs or loading the
/// registry is reported loudly on stderr (ENG-004) and treated as that
/// tier resolving nothing, so one unreadable `.clanker` file or malformed
/// registry never blocks scope resolution outright -- the remaining tiers
/// still run.
fn resolve_scope(
    raw_scope: Option<&str>,
    start_dir: &Path,
    clanker_session_project: Option<&str>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> (Option<String>, Option<ScopeSource>) {
    if let Some(scope) = raw_scope {
        return (Some(scope.to_string()), Some(ScopeSource::Env));
    }

    if let Some((slug, source)) = resolve_project_scope(start_dir, xdg_config_home, home) {
        return (Some(slug), Some(source));
    }

    if let Some(project) = clanker_session_project
        && !project.trim().is_empty()
    {
        return (Some(project.to_string()), Some(ScopeSource::Clanker));
    }

    if let Some(name) = directory_scope(start_dir) {
        return (Some(name), Some(ScopeSource::Directory));
    }

    (None, None)
}

/// Resolve `start_dir` through `tftio_lib::project`: discover any `.clanker`
/// declaration and origin remote, load the installed registry, and apply
/// the shared derivation order.
///
/// Returns `None` both when no tier resolves (no declaration, no matching
/// registry entry, and no remote to derive from) and when discovery itself
/// fails; a discovery or registry-load failure is reported on stderr first
/// (see the module documentation).
fn resolve_project_scope(
    start_dir: &Path,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<(String, ScopeSource)> {
    let inputs = match discover_inputs(start_dir) {
        Ok(inputs) => inputs,
        Err(err) => {
            eprintln!(
                "mnene: warning: project discovery failed for {}: {err}",
                start_dir.display()
            );
            return None;
        }
    };

    let registry = default_registry_dir(xdg_config_home, home).map_or_else(
        tftio_lib::project::Registry::default,
        |dir| match load_registry(&dir, home) {
            Ok(registry) => registry,
            Err(err) => {
                eprintln!(
                    "mnene: warning: failed to load the project registry at {}: {err}",
                    dir.display()
                );
                tftio_lib::project::Registry::default()
            }
        },
    );

    let resolution = resolve_project(
        inputs.declared.as_ref(),
        start_dir,
        inputs.remote.as_ref(),
        &registry,
    )?;

    let source = match resolution.source {
        Source::Declared => ScopeSource::Declared,
        Source::Path => ScopeSource::Path,
        Source::Remote => ScopeSource::Remote,
        Source::Derived => ScopeSource::Derived,
    };

    Some((resolution.slug.as_str().to_string(), source))
}

/// The last-resort scope tier: the name of the git repository containing
/// `start_dir`, taken from the *common* git directory shared by every
/// worktree via [`tftio_lib::project::repository_name`]. `None` outside any
/// git repository, or when `start_dir`'s only project root is a `.clanker`
/// declaration rather than a repository (that case is already handled by
/// [`resolve_project_scope`]).
fn directory_scope(start_dir: &Path) -> Option<String> {
    let root = project_root(start_dir)?;
    let common_dir = root.common_dir?;
    repository_name(&common_dir)
}

/// Read the branch name of the git worktree containing `start_dir`, for the
/// `task` field.
///
/// This is deliberately independent of `scope` resolution: `task` names a
/// branch, not a project, and stays worktree-local even when two linked
/// worktrees of one repository share a scope. `tftio_lib::project` exposes
/// no branch accessor (a branch is not part of project identity), so this
/// resolves the current worktree's own git directory from
/// [`ProjectRoot::path`] directly, the same way `resolve_head_dir` used to,
/// rather than duplicating the removed `find_git_entry` walk: `path` is
/// already the repository top level `tftio_lib::project::project_root`
/// found.
///
/// Never shells out: it reads `.git` and `HEAD` by hand.
fn read_worktree_branch(start_dir: &Path) -> Option<String> {
    let root = project_root(start_dir)?;
    let ProjectRoot {
        path,
        common_dir: Some(_),
    } = root
    else {
        return None;
    };

    let git_path = path.join(".git");
    let head_dir = if git_path.is_dir() {
        git_path
    } else {
        let contents = fs::read_to_string(&git_path).ok()?;
        let first_line = contents.lines().next()?;
        let raw_gitdir = first_line.strip_prefix("gitdir:")?.trim();
        path.join(raw_gitdir)
    };

    read_branch(&head_dir)
}

/// Read `<head_dir>/HEAD` and return the branch name when it holds a
/// symbolic ref to `refs/heads/<name>`.
///
/// Returns `None` for a detached `HEAD` (a bare object id) or when `HEAD`
/// cannot be read.
fn read_branch(head_dir: &Path) -> Option<String> {
    let contents = fs::read_to_string(head_dir.join("HEAD")).ok()?;
    let trimmed = contents.trim();
    let ref_target = trimmed.strip_prefix("ref: ")?;
    let name = ref_target.strip_prefix("refs/heads/")?;
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Config, RawConfig};
    use crate::model::ScopeSource;
    use std::fs;
    use std::path::Path;

    /// Initialize an ordinary (non-worktree) repository at `repo_dir` with
    /// `HEAD` pointing at `refs/heads/<branch>`, optionally with an origin
    /// remote.
    fn init_repo(repo_dir: &Path, branch: &str) -> Result<(), std::io::Error> {
        let git_dir = repo_dir.join(".git");
        fs::create_dir_all(&git_dir)?;
        fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))
    }

    /// Initialize an ordinary repository at `repo_dir` with an origin
    /// remote in its `config` file.
    fn init_repo_with_remote(
        repo_dir: &Path,
        branch: &str,
        origin: &str,
    ) -> Result<(), std::io::Error> {
        init_repo(repo_dir, branch)?;
        fs::write(
            repo_dir.join(".git").join("config"),
            format!("[remote \"origin\"]\n\turl = {origin}\n"),
        )
    }

    /// Initialize an ordinary repository at `repo_dir` with a detached
    /// `HEAD` (a bare object id, as `git checkout <sha>` leaves it).
    fn init_detached_repo(repo_dir: &Path) -> Result<(), std::io::Error> {
        let git_dir = repo_dir.join(".git");
        fs::create_dir_all(&git_dir)?;
        fs::write(
            git_dir.join("HEAD"),
            "4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
        )
    }

    /// Initialize a linked worktree at `worktree_dir`: a `.git` file
    /// pointing at `gitdir_dir`, which holds its own `HEAD` and the
    /// `commondir` file git writes to name the shared git directory.
    ///
    /// `commondir` holds a path relative to `gitdir_dir`, exactly as git
    /// writes it for a worktree under `<common>/worktrees/<name>`.
    fn init_worktree(
        worktree_dir: &Path,
        gitdir_dir: &Path,
        branch: &str,
    ) -> Result<(), std::io::Error> {
        fs::create_dir_all(worktree_dir)?;
        fs::create_dir_all(gitdir_dir)?;
        let gitdir_line = format!("gitdir: {}\n", gitdir_dir.display());
        fs::write(worktree_dir.join(".git"), gitdir_line)?;
        fs::write(gitdir_dir.join("commondir"), "../..\n")?;
        fs::write(
            gitdir_dir.join("HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
    }

    /// A `RawConfig` with no registry directory reachable (no `HOME`, no
    /// `XDG_CONFIG_HOME`), so `resolve_project_scope`'s registry load is
    /// always an empty, in-memory default and no test in this module
    /// touches a real filesystem path outside its own tempdir.
    fn raw_with_no_registry() -> RawConfig {
        RawConfig::default()
    }

    /// Writes `contents` to the `projects.toml` a `RawConfig` naming
    /// `registry_root` as `xdg_config_home` will resolve, per
    /// `tftio_lib::project::default_registry_dir`: `<xdg_config_home>/tftio/projects.toml`.
    fn write_registry(registry_root: &Path, contents: &str) -> Result<(), std::io::Error> {
        let tftio_dir = registry_root.join("tftio");
        fs::create_dir_all(&tftio_dir)?;
        fs::write(tftio_dir.join("projects.toml"), contents)
    }

    #[test]
    fn scope_falls_back_to_the_repository_name_with_no_remote_and_no_registry()
    -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path(), "feature-x")?;

        let config = Config::resolve(raw_with_no_registry(), dir.path());

        let expected = dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        assert_eq!(expected, config.scope);
        assert_eq!(Some(ScopeSource::Directory), config.scope_source);
        assert_eq!(Some("feature-x".to_string()), config.task);
        Ok(())
    }

    #[test]
    fn scope_and_task_are_found_from_a_nested_subdirectory() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path(), "main")?;
        let nested = dir.path().join("src").join("inner");
        fs::create_dir_all(&nested)?;

        let config = Config::resolve(raw_with_no_registry(), &nested);

        let expected = dir
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        assert_eq!(expected, config.scope);
        assert_eq!(Some("main".to_string()), config.task);
        Ok(())
    }

    #[test]
    fn detached_head_yields_no_task() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        init_detached_repo(dir.path())?;

        let config = Config::resolve(raw_with_no_registry(), dir.path());

        assert_eq!(None, config.task);
        Ok(())
    }

    #[test]
    fn a_linked_worktree_resolves_the_repository_not_the_worktree() -> Result<(), std::io::Error> {
        let root = tempfile::tempdir()?;
        let repo_dir = root.path().join("project");
        let worktree_dir = repo_dir.join("worktree-a");
        let gitdir_dir = repo_dir.join(".git").join("worktrees").join("worktree-a");
        init_worktree(&worktree_dir, &gitdir_dir, "worktree-branch")?;

        let config = Config::resolve(raw_with_no_registry(), &worktree_dir);

        assert_eq!(Some("project".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Directory), config.scope_source);
        assert_eq!(Some("worktree-branch".to_string()), config.task);
        Ok(())
    }

    #[test]
    fn two_worktrees_of_one_repository_share_a_scope() -> Result<(), std::io::Error> {
        let root = tempfile::tempdir()?;
        let repo_dir = root.path().join("project");
        let worktrees = repo_dir.join(".git").join("worktrees");
        let first = repo_dir.join("main");
        let second = repo_dir.join("feature-x");
        init_worktree(&first, &worktrees.join("main"), "main")?;
        init_worktree(&second, &worktrees.join("feature-x"), "feature/x")?;

        let first = Config::resolve(raw_with_no_registry(), &first);
        let second = Config::resolve(raw_with_no_registry(), &second);

        assert_eq!(first.scope, second.scope);
        assert_eq!(Some("project".to_string()), first.scope);
        assert_eq!(Some("main".to_string()), first.task);
        assert_eq!(Some("feature/x".to_string()), second.task);
        Ok(())
    }

    #[test]
    fn two_worktrees_of_one_registered_repository_share_a_scope_by_remote()
    -> Result<(), std::io::Error> {
        // The registered-remote counterpart of the directory-name test
        // above: with the shared origin remote registered under a slug,
        // both worktrees resolve that slug with source `remote`, exactly
        // as they shared a directory-derived scope before this task.
        let root = tempfile::tempdir()?;
        let repo_dir = root.path().join("project");
        let worktrees = repo_dir.join(".git").join("worktrees");
        let first = repo_dir.join("main");
        let second = repo_dir.join("feature-x");
        fs::create_dir_all(repo_dir.join(".git"))?;
        let origin_config = "[remote \"origin\"]\n\turl = git@github.com:tftio/kb.git\n";
        fs::write(repo_dir.join(".git").join("config"), origin_config)?;
        init_worktree(&first, &worktrees.join("main"), "main")?;
        init_worktree(&second, &worktrees.join("feature-x"), "feature/x")?;

        let registry_dir = tempfile::tempdir()?;
        let kb_remote_registry = "[project.kb]\nremotes = [\"github.com/tftio/kb\"]\n";
        write_registry(registry_dir.path(), kb_remote_registry)?;
        let raw = |dir: &Path| RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            home: Some(dir.to_path_buf()),
            ..RawConfig::default()
        };

        let resolved_first = Config::resolve(raw(root.path()), &first);
        let resolved_second = Config::resolve(raw(root.path()), &second);

        assert_eq!("kb", resolved_first.scope.as_deref().unwrap_or_default());
        assert_eq!(resolved_first.scope, resolved_second.scope);
        assert_eq!(Some(ScopeSource::Remote), resolved_first.scope_source);
        Ok(())
    }

    #[test]
    fn a_worktree_of_a_conventional_bare_clone_drops_the_git_suffix() -> Result<(), std::io::Error>
    {
        let root = tempfile::tempdir()?;
        let gitdir = root.path().join("project.git").join("worktrees").join("a");
        let worktree_dir = root.path().join("worktree-a");
        init_worktree(&worktree_dir, &gitdir, "topic")?;

        let config = Config::resolve(raw_with_no_registry(), &worktree_dir);

        assert_eq!(Some("project".to_string()), config.scope);
        Ok(())
    }

    #[test]
    fn a_git_file_without_a_gitdir_line_yields_no_scope_or_task() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".git"), "not a gitdir pointer\n")?;

        let config = Config::resolve(raw_with_no_registry(), dir.path());

        assert_eq!(None, config.scope);
        assert_eq!(None, config.task);
        Ok(())
    }

    #[test]
    fn a_directory_with_no_repository_yields_no_scope_or_task() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let config = Config::resolve(raw_with_no_registry(), dir.path());

        assert_eq!(None, config.scope);
        assert_eq!(None, config.scope_source);
        assert_eq!(None, config.task);
        Ok(())
    }

    #[test]
    fn explicit_scope_and_task_override_discovery() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        init_repo(dir.path(), "main")?;
        let raw = RawConfig {
            scope: Some("explicit-scope".to_string()),
            task: Some("explicit-task".to_string()),
            ..raw_with_no_registry()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(Some("explicit-scope".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Env), config.scope_source);
        assert_eq!(Some("explicit-task".to_string()), config.task);
        Ok(())
    }

    #[test]
    fn a_registered_origin_resolves_the_slug_with_source_remote() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("kb");
        init_repo_with_remote(&repo_dir, "main", "git@github.com:tftio/kb.git")?;

        let registry_dir = tempfile::tempdir()?;
        let kb_remote_registry = "[project.kb]\nremotes = [\"github.com/tftio/kb\"]\n";
        write_registry(registry_dir.path(), kb_remote_registry)?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &repo_dir);

        assert_eq!(Some("kb".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Remote), config.scope_source);
        Ok(())
    }

    #[test]
    fn an_unregistered_repository_with_no_remote_resolves_the_directory_name()
    -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("unregistered-repo");
        init_repo(&repo_dir, "main")?;

        let registry_dir = tempfile::tempdir()?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &repo_dir);

        assert_eq!(Some("unregistered-repo".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Directory), config.scope_source);
        Ok(())
    }

    #[test]
    fn clanker_session_project_resolves_in_an_unregistered_non_repository_directory()
    -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let raw = RawConfig {
            clanker_session_project: Some("marker-project".to_string()),
            ..raw_with_no_registry()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(Some("marker-project".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Clanker), config.scope_source);
        Ok(())
    }

    #[test]
    fn a_clanker_declared_non_repository_directory_yields_no_task() -> Result<(), std::io::Error> {
        // `read_worktree_branch` is called unconditionally by
        // `Config::resolve`; when `project_root` finds a root by the
        // outside-a-repository `.clanker` search (`common_dir: None`)
        // rather than a git repository, there is no worktree HEAD to read,
        // so it returns `None` without touching the filesystem again.
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join(".clanker"), "project = \"example\"\n")?;

        let config = Config::resolve(raw_with_no_registry(), dir.path());

        assert_eq!(Some("example".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Declared), config.scope_source);
        assert_eq!(None, config.task);
        Ok(())
    }

    #[test]
    fn blank_clanker_session_project_is_not_used() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let raw = RawConfig {
            clanker_session_project: Some("   ".to_string()),
            ..raw_with_no_registry()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(None, config.scope);
        Ok(())
    }

    #[test]
    fn a_declared_project_wins_over_a_disagreeing_registered_remote() -> Result<(), std::io::Error>
    {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("kb");
        init_repo_with_remote(&repo_dir, "main", "git@github.com:tftio/kb.git")?;
        fs::write(repo_dir.join(".clanker"), "project = \"kb-declared\"\n")?;

        let registry_dir = tempfile::tempdir()?;
        let kb_remote_registry = "[project.kb]\nremotes = [\"github.com/tftio/kb\"]\n";
        write_registry(registry_dir.path(), kb_remote_registry)?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &repo_dir);

        assert_eq!(Some("kb-declared".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Declared), config.scope_source);
        Ok(())
    }

    #[test]
    fn a_registered_path_resolves_a_non_repository_directory() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let example_project = dir.path().join("example");
        fs::create_dir_all(&example_project)?;

        let registry_dir = tempfile::tempdir()?;
        let contents = format!(
            "[project.example]\npaths = [\"{}\"]\n",
            example_project.display()
        );
        write_registry(registry_dir.path(), &contents)?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &example_project);

        assert_eq!(Some("example".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Path), config.scope_source);
        Ok(())
    }

    #[test]
    fn an_unregistered_remote_derives_a_slug() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("my_repo");
        init_repo_with_remote(&repo_dir, "main", "https://example.com/My_Repo")?;

        let registry_dir = tempfile::tempdir()?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &repo_dir);

        assert_eq!(Some("my-repo".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Derived), config.scope_source);
        Ok(())
    }

    #[test]
    fn a_malformed_registry_is_reported_and_scope_resolution_continues()
    -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("kb");
        init_repo(&repo_dir, "main")?;

        let registry_dir = tempfile::tempdir()?;
        write_registry(registry_dir.path(), "not valid toml =")?;
        let raw = RawConfig {
            xdg_config_home: Some(registry_dir.path().to_path_buf()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, &repo_dir);

        // The registry failed to load, so neither a path nor a remote tier
        // can answer; with no remote at all here either, resolution falls
        // all the way through to the directory-name tier -- a broken
        // registry degrades resolution, it does not fail the command.
        assert_eq!(Some("kb".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Directory), config.scope_source);
        Ok(())
    }

    #[test]
    fn a_malformed_clanker_declaration_is_reported_and_scope_resolution_continues()
    -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let repo_dir = dir.path().join("kb");
        init_repo(&repo_dir, "main")?;
        fs::write(repo_dir.join(".clanker"), "not valid toml =")?;

        let config = Config::resolve(raw_with_no_registry(), &repo_dir);

        assert_eq!(Some("kb".to_string()), config.scope);
        assert_eq!(Some(ScopeSource::Directory), config.scope_source);
        Ok(())
    }

    #[test]
    fn agent_prefers_mnene_over_clanker() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            agent: Some("claude".to_string()),
            clanker_session_harness: Some("codex".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(Some("claude".to_string()), config.agent);
        Ok(())
    }

    #[test]
    fn agent_falls_back_to_clanker_harness() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            clanker_session_harness: Some("codex".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(Some("codex".to_string()), config.agent);
        Ok(())
    }

    #[test]
    fn agent_is_absent_when_neither_source_is_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(None, config.agent);
        Ok(())
    }

    #[test]
    fn context_prefers_mnene_over_clanker() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            context: Some("work".to_string()),
            clanker_session_context: Some("personal".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!("work", config.context);
        Ok(())
    }

    #[test]
    fn context_falls_back_to_clanker_session_context() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            clanker_session_context: Some("personal".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!("personal", config.context);
        Ok(())
    }

    #[test]
    fn context_defaults_when_neither_source_is_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!("default", config.context);
        Ok(())
    }

    #[test]
    fn session_prefers_mnene_over_clanker() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            session: Some("session-123".to_string()),
            clanker_session_id: Some("01a06ebb-0000-7000-8000-000000000000".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(Some("session-123".to_string()), config.session);
        Ok(())
    }

    #[test]
    fn session_falls_back_to_the_clanker_session_id() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            clanker_session_id: Some("01a06ebb-0000-7000-8000-000000000000".to_string()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(
            Some("01a06ebb-0000-7000-8000-000000000000".to_string()),
            config.session
        );
        Ok(())
    }

    #[test]
    fn session_is_absent_when_not_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(None, config.session);
        Ok(())
    }

    #[test]
    fn explicit_db_path_is_used_verbatim() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let explicit = dir.path().join("custom").join("memories.db");
        let raw = RawConfig {
            db: Some(explicit.clone()),
            xdg_data_home: Some(dir.path().join("xdg")),
            home: Some(dir.path().join("home")),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(explicit, config.db);
        Ok(())
    }

    #[test]
    fn db_defaults_under_xdg_data_home_when_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let xdg = dir.path().join("xdg");
        let raw = RawConfig {
            xdg_data_home: Some(xdg.clone()),
            home: Some(dir.path().join("home")),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(xdg.join("mnene").join("default.db"), config.db);
        Ok(())
    }

    #[test]
    fn db_defaults_under_home_when_xdg_data_home_is_absent() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        let raw = RawConfig {
            home: Some(home.clone()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(
            home.join(".local/share/mnene").join("default.db"),
            config.db
        );
        Ok(())
    }

    #[test]
    fn db_falls_back_to_start_dir_when_neither_xdg_nor_home_is_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;

        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(dir.path().join(".mnene").join("default.db"), config.db);
        Ok(())
    }

    #[test]
    fn db_path_uses_the_resolved_context_name() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let xdg = dir.path().join("xdg");
        let raw = RawConfig {
            context: Some("work".to_string()),
            xdg_data_home: Some(xdg.clone()),
            ..RawConfig::default()
        };

        let config = Config::resolve(raw, dir.path());

        assert_eq!(xdg.join("mnene").join("work.db"), config.db);
        Ok(())
    }

    #[test]
    fn config_derives_debug_clone_eq() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let config = Config::resolve(RawConfig::default(), dir.path());
        let cloned = config.clone();

        assert_eq!(config, cloned);
        assert!(format!("{config:?}").contains("Config"));
        Ok(())
    }

    #[test]
    fn require_agent_returns_the_configured_agent() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            agent: Some("agent-a".to_string()),
            ..RawConfig::default()
        };
        let config = Config::resolve(raw, dir.path());

        assert_eq!(Ok("agent-a"), config.require_agent());
        Ok(())
    }

    #[test]
    fn require_agent_is_missing_agent_when_unset() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(
            Err(crate::model::MneneError::MissingAgent),
            config.require_agent()
        );
        Ok(())
    }

    #[test]
    fn require_scope_returns_the_configured_scope() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            scope: Some("scope-a".to_string()),
            ..RawConfig::default()
        };
        let config = Config::resolve(raw, dir.path());

        assert_eq!(Ok("scope-a"), config.require_scope());
        Ok(())
    }

    #[test]
    fn require_scope_is_missing_scope_when_unset() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(
            Err(crate::model::MneneError::MissingScope),
            config.require_scope()
        );
        Ok(())
    }

    #[test]
    fn provenance_composes_agent_scope_and_the_rest_of_config()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            agent: Some("agent-a".to_string()),
            context: Some("ctx".to_string()),
            session: Some("session-a".to_string()),
            scope: Some("scope-a".to_string()),
            task: Some("task-a".to_string()),
            ..RawConfig::default()
        };
        let config = Config::resolve(raw, dir.path());

        let provenance = config.provenance()?;
        assert_eq!("agent-a", provenance.agent);
        assert_eq!("ctx", provenance.context);
        assert_eq!(Some("session-a".to_string()), provenance.session);
        assert_eq!("scope-a", provenance.scope);
        assert_eq!(Some(ScopeSource::Env), provenance.scope_source);
        assert_eq!(Some("task-a".to_string()), provenance.task);
        Ok(())
    }

    #[test]
    fn provenance_reports_missing_agent_before_checking_scope() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let config = Config::resolve(RawConfig::default(), dir.path());

        assert_eq!(
            Err(crate::model::MneneError::MissingAgent),
            config.provenance()
        );
        Ok(())
    }

    #[test]
    fn provenance_reports_missing_scope_when_agent_is_set() -> Result<(), std::io::Error> {
        let dir = tempfile::tempdir()?;
        let raw = RawConfig {
            agent: Some("agent-a".to_string()),
            ..RawConfig::default()
        };
        let config = Config::resolve(raw, dir.path());

        assert_eq!(
            Err(crate::model::MneneError::MissingScope),
            config.provenance()
        );
        Ok(())
    }

    #[test]
    fn raw_config_default_has_every_field_absent() {
        let raw = RawConfig::default();

        assert_eq!(None, raw.agent);
        assert_eq!(None, raw.context);
        assert_eq!(None, raw.session);
        assert_eq!(None, raw.scope);
        assert_eq!(None, raw.task);
        assert_eq!(None, raw.db);
        assert_eq!(None, raw.clanker_session_harness);
        assert_eq!(None, raw.clanker_session_context);
        assert_eq!(None, raw.clanker_session_id);
        assert_eq!(None, raw.clanker_session_project);
        assert_eq!(None, raw.xdg_data_home);
        assert_eq!(None, raw.xdg_config_home);
        assert_eq!(None, raw.home);
    }
}
