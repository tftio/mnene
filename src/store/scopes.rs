//! Scope inventory for `mnene` memories.
//!
//! [`SqliteStore::scopes`] lists every distinct scope value stored, across
//! every lifecycle state, with a count of memories and the distinct
//! `scope_source` values seen for that scope. This is a diagnostic verb: it
//! lets an operator find rows still named by directory (`scope_source =
//! "directory"`, or absent on a row written before the schema-2 migration)
//! and register their repositories in the project registry, per D005 and
//! D010.

use serde::{Deserialize, Serialize};

use crate::model::MneneError;
use crate::store::{SqliteStore, storage_error};

/// One scope's summary.
///
/// Its name, how many memories carry it (any lifecycle state), and the
/// distinct `scope_source` labels seen across those rows, sorted for
/// determinism. `"none"` stands in for a `NULL` `scope_source` -- either a
/// row written before the schema-2 migration, or one written by a build
/// that predates this field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeSummary {
    /// The scope value.
    pub scope: String,
    /// How many memories (any lifecycle state) carry this scope.
    pub count: u64,
    /// The distinct `scope_source` labels seen for this scope, sorted.
    pub sources: Vec<String>,
}

impl SqliteStore {
    /// Lists every distinct scope stored, newest-scope-name-last
    /// (alphabetical), with a count and the sources seen.
    ///
    /// Covers every lifecycle state (`active`, `superseded`, and
    /// `retracted`), not only active memories, so a scope that only ever
    /// held now-retracted rows is still reported.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn scopes(&self) -> Result<Vec<ScopeSummary>, MneneError> {
        let mut statement = self
            .conn()
            .prepare(
                "SELECT scope, scope_source, COUNT(*) FROM memories \
                 GROUP BY scope, scope_source ORDER BY scope ASC",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(storage_error)?;

        let mut summaries: Vec<ScopeSummary> = Vec::new();
        for row in rows {
            let (scope, source, count) = row.map_err(storage_error)?;
            let source_label = source.unwrap_or_else(|| "none".to_string());
            let count = u64::try_from(count).unwrap_or(0);

            match summaries.last_mut() {
                Some(last) if last.scope == scope => {
                    last.count += count;
                    if !last.sources.contains(&source_label) {
                        last.sources.push(source_label);
                    }
                }
                _ => summaries.push(ScopeSummary {
                    scope,
                    count,
                    sources: vec![source_label],
                }),
            }
        }

        for summary in &mut summaries {
            summary.sources.sort();
        }

        Ok(summaries)
    }
}

#[cfg(test)]
mod tests {
    use super::ScopeSummary;
    use crate::model::{Provenance, ScopeSource};
    use crate::store::SqliteStore;

    fn provenance(scope: &str, scope_source: Option<ScopeSource>) -> Provenance {
        Provenance {
            agent: "agent-a".to_string(),
            context: "default".to_string(),
            session: None,
            scope: scope.to_string(),
            scope_source,
            task: None,
        }
    }

    #[test]
    fn scopes_is_empty_for_an_empty_store() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        assert_eq!(Vec::<ScopeSummary>::new(), store.scopes()?);
        Ok(())
    }

    #[test]
    fn scopes_lists_distinct_scopes_with_counts_and_sources()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        store.put("a", &[], &provenance("kb", Some(ScopeSource::Remote)))?;
        store.put("b", &[], &provenance("kb", Some(ScopeSource::Remote)))?;
        store.put("c", &[], &provenance("mnene", Some(ScopeSource::Directory)))?;
        store.put("d", &[], &provenance("mnene", None))?;

        let scopes = store.scopes()?;
        assert_eq!(
            vec![
                ScopeSummary {
                    scope: "kb".to_string(),
                    count: 2,
                    sources: vec!["remote".to_string()],
                },
                ScopeSummary {
                    scope: "mnene".to_string(),
                    count: 2,
                    sources: vec!["directory".to_string(), "none".to_string()],
                },
            ],
            scopes
        );
        Ok(())
    }

    #[test]
    fn scopes_counts_every_lifecycle_state() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("kb", Some(ScopeSource::Env));

        store.put("stays active", &[], &prov)?;
        let retracted = store.put("gets retracted", &[], &prov)?;
        store.retract(retracted, &prov)?;

        let scopes = store.scopes()?;
        assert_eq!(1, scopes.len());
        let summary = scopes.first().ok_or("expected one scope summary")?;
        assert_eq!("kb", summary.scope);
        assert_eq!(2, summary.count);
        Ok(())
    }

    #[test]
    fn scopes_ids_query_failure_is_a_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        store.conn_mut().execute_batch("DROP TABLE memories;")?;

        let err = store.scopes();
        assert!(
            matches!(err, Err(crate::model::MneneError::Storage(_))),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn scope_summary_derives_debug_clone_eq() {
        // serde_json is confined to src/main.rs and src/mcp.rs (deny.toml,
        // RS-009); the round trip through --json is exercised there, in
        // tests/cli.rs's own `scopes` coverage instead.
        let summary = ScopeSummary {
            scope: "kb".to_string(),
            count: 3,
            sources: vec!["remote".to_string()],
        };
        let cloned = summary.clone();
        assert_eq!(summary, cloned);
        assert!(format!("{summary:?}").contains("kb"));
    }
}
