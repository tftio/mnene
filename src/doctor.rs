//! `tftio-lib` doctor adapter for `mnene`.
//!
//! [`MneneDoctor`](crate::doctor::MneneDoctor) implements
//! [`tftio_lib::DoctorChecks`] with a single,
//! non-destructive check: it opens an in-memory `SQLite` connection --
//! never touching [`crate::config::Config::db`] or any on-disk path -- and
//! creates an FTS5 virtual table, proving that the bundled engine's
//! full-text search support (the same feature
//! [`crate::store::SqliteStore`] relies on) is available. Because
//! `tftio_lib::run_cli_from` routes every metadata command, including
//! `meta doctor`, through the shared runner before `src/main.rs` ever
//! opens the configured persistent database, `meta doctor` never creates
//! or mutates it.

use tftio_lib::{DoctorCheck, DoctorChecks, RepoInfo};

/// The name reported by the FTS5 availability check.
const FTS5_CHECK_NAME: &str = "sqlite fts5 virtual table";

/// Doctor adapter reporting `mnene`'s package identity and health checks.
pub struct MneneDoctor;

impl DoctorChecks for MneneDoctor {
    fn repo_info() -> RepoInfo {
        RepoInfo::new("tftio", "mnene")
    }

    fn current_version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn tool_checks(&self) -> Vec<DoctorCheck> {
        vec![fts5_check(probe_fts5())]
    }
}

/// Pure mapping from a bundled-engine FTS5 probe result to a
/// [`DoctorCheck`].
///
/// Split out from [`probe_fts5`] (the actual I/O, one straight line) so
/// both branches can be exercised directly by a unit test: a real bundled
/// `SQLite` build always supports FTS5, so [`probe_fts5`]'s `Err` branch
/// is unreachable in practice and could otherwise never be covered.
fn fts5_check(result: Result<(), rusqlite::Error>) -> DoctorCheck {
    match result {
        Ok(()) => DoctorCheck::pass(FTS5_CHECK_NAME),
        Err(err) => DoctorCheck::fail(FTS5_CHECK_NAME, err.to_string()),
    }
}

/// Opens an in-memory `SQLite` connection and creates an FTS5 virtual
/// table, proving FTS5 availability without touching any on-disk path.
fn probe_fts5() -> Result<(), rusqlite::Error> {
    rusqlite::Connection::open_in_memory()?
        .execute_batch("CREATE VIRTUAL TABLE mnene_doctor_probe USING fts5(body);")
}

#[cfg(test)]
mod tests {
    use super::{DoctorCheck, DoctorChecks, FTS5_CHECK_NAME, MneneDoctor, fts5_check, probe_fts5};

    #[test]
    fn repo_info_reports_tftio_mnene() {
        let repo = MneneDoctor::repo_info();
        assert_eq!("tftio", repo.owner);
        assert_eq!("mnene", repo.name);
    }

    #[test]
    fn current_version_matches_package_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), MneneDoctor::current_version());
    }

    #[test]
    fn tool_checks_reports_one_passing_fts5_check() {
        let checks = MneneDoctor.tool_checks();
        assert_eq!(1, checks.len());
        assert_eq!(Some(true), checks.first().map(|check| check.passed));
    }

    #[test]
    fn probe_fts5_succeeds_against_the_bundled_engine() {
        assert!(probe_fts5().is_ok());
    }

    #[test]
    fn fts5_check_passes_on_ok() {
        let check = fts5_check(Ok(()));
        assert!(check.passed);
        assert_eq!(FTS5_CHECK_NAME, check.name);
        assert_eq!(None, check.message);
    }

    #[test]
    fn fts5_check_fails_on_err() {
        let check: DoctorCheck =
            fts5_check(Err(rusqlite::Error::InvalidParameterName("x".to_string())));
        assert!(!check.passed);
        assert_eq!(FTS5_CHECK_NAME, check.name);
        assert!(check.message.is_some());
    }
}
