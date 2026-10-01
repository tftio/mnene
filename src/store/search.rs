//! Full-text search over `mnene` memories.
//!
//! This module implements [`SqliteStore::search`]: the FTS5 join, state and scope filters,
//! required tags, bm25 ordering with a recency tie-break, and the result
//! limit.

use rusqlite::ToSql;
use rusqlite::params_from_iter;

use crate::model::{MemoryId, MneneError, SearchHit, Tag};
use crate::query::{build_match_expression, score_from_bm25};
use crate::store::{SqliteStore, storage_error};

/// Options narrowing a [`SqliteStore::search`] call.
///
/// `scope: None` means all scopes are searched; `Some(scope)` restricts
/// results to memories recorded under that scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    /// The maximum number of hits to return.
    pub limit: u32,
    /// When `true`, `superseded` memories are included alongside `active`
    /// ones. `retracted` memories are never returned, regardless of this
    /// flag.
    pub include_superseded: bool,
    /// Restricts results to this scope when `Some`; searches every scope
    /// when `None`.
    pub scope: Option<String>,
    /// Every tag named here must be attached to a returned memory.
    pub required_tags: Vec<Tag>,
}

impl Default for SearchOptions {
    /// The default search: up to 10 hits, `active` memories only, every
    /// scope, no required tags.
    fn default() -> Self {
        Self {
            limit: 10,
            include_superseded: false,
            scope: None,
            required_tags: Vec::new(),
        }
    }
}

impl SqliteStore {
    /// Searches memories by full-text relevance.
    ///
    /// Builds a safe FTS5 `MATCH` expression from `query` via
    /// [`build_match_expression`]; when that returns `None` (an empty or
    /// all-punctuation query), returns an empty result without touching the
    /// database. Otherwise runs one read-only `SELECT` joining
    /// `memories_fts` to `memories`, filtering by lifecycle state (`active`
    /// only by default, plus `superseded` when `options.include_superseded`
    /// is set; `retracted` rows are never returned), by `options.scope`
    /// when given, and requiring every tag in `options.required_tags` to be
    /// attached to the memory. Results are ordered by `bm25()` ascending
    /// (best match first) with ties broken by `created_at` descending, and
    /// capped at `options.limit`.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn search(
        &self,
        query: &str,
        options: &SearchOptions,
    ) -> Result<Vec<SearchHit>, MneneError> {
        let Some(match_expression) = build_match_expression(query) else {
            return Ok(Vec::new());
        };

        let state_filter = if options.include_superseded {
            "memories.state IN ('active','superseded')"
        } else {
            "memories.state = 'active'"
        };

        let mut sql = format!(
            "SELECT memories.id, bm25(memories_fts) AS rank \
             FROM memories_fts JOIN memories ON memories.id = memories_fts.id \
             WHERE memories_fts MATCH ? AND {state_filter}"
        );

        let mut bound_values: Vec<Box<dyn ToSql>> = vec![Box::new(match_expression)];

        if let Some(scope) = &options.scope {
            sql.push_str(" AND memories.scope = ?");
            bound_values.push(Box::new(scope.clone()));
        }

        for tag in &options.required_tags {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM tags \
                 WHERE tags.memory_id = memories.id AND tags.tag = ?)",
            );
            bound_values.push(Box::new(tag.as_str().to_string()));
        }

        sql.push_str(" ORDER BY bm25(memories_fts) ASC, memories.created_at DESC LIMIT ?");
        bound_values.push(Box::new(i64::from(options.limit)));

        let mut statement = self.conn().prepare(&sql).map_err(storage_error)?;
        let params = params_from_iter(&bound_values);
        let rows = statement
            .query_map(params, |row| {
                let id_text: String = row.get(0)?;
                let bm25: f64 = row.get(1)?;
                Ok((id_text, bm25))
            })
            .map_err(storage_error)?;

        let mut hits = Vec::new();
        for row in rows {
            let (id_text, bm25) = row.map_err(storage_error)?;
            let id: MemoryId = id_text.parse()?;
            hits.push(SearchHit {
                id,
                score: score_from_bm25(bm25),
            });
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchOptions, SqliteStore};
    use crate::model::{MemoryId, MneneError, Provenance, SearchHit, Tag};
    use rusqlite::params;

    /// Extracts the error from a `Result` expected to be `Err`, without
    /// `unwrap`/`expect`: an unexpected `Ok` becomes a typed test failure.
    macro_rules! expect_err {
        ($result:expr) => {
            match $result {
                Ok(_) => Err("expected an error but got Ok".to_string()),
                Err(err) => Ok(err),
            }
        };
    }

    /// Destructures a two-element slice of hits without indexing, for
    /// asserting on ordered pairs; an unexpected length becomes a typed
    /// test failure rather than a panic.
    fn expect_pair(items: &[SearchHit]) -> Result<(&SearchHit, &SearchHit), String> {
        match items {
            [first, second] => Ok((first, second)),
            other => Err(format!("expected exactly 2 items, got {}", other.len())),
        }
    }

    /// Destructures a one-element slice of hits without indexing; an
    /// unexpected length becomes a typed test failure rather than a panic.
    fn expect_one(items: &[SearchHit]) -> Result<&SearchHit, String> {
        match items {
            [only] => Ok(only),
            other => Err(format!("expected exactly 1 item, got {}", other.len())),
        }
    }

    #[test]
    fn expect_err_reports_an_unexpected_ok() {
        let ok: Result<(), MneneError> = Ok(());
        let result: Result<MneneError, String> = expect_err!(ok);
        assert_eq!(Err("expected an error but got Ok".to_string()), result);
    }

    fn provenance(scope: &str) -> Provenance {
        Provenance {
            agent: "agent-a".to_string(),
            context: "default".to_string(),
            session: Some("session-1".to_string()),
            scope: scope.to_string(),
            scope_source: None,
            task: Some("t006".to_string()),
        }
    }

    #[test]
    fn ranks_repeated_term_above_single_mention() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let strong = store.put("rust rust rust systems programming", &[], &prov)?;
        let weak = store.put("rust is one language among many", &[], &prov)?;

        let hits = store.search("rust", &SearchOptions::default())?;

        let (first, second) = expect_pair(&hits)?;
        assert_eq!(strong, first.id);
        assert_eq!(weak, second.id);
        assert!(first.score > second.score);
        Ok(())
    }

    #[test]
    fn scope_bounds_results_and_none_widens() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let in_a = store.put("widget calibration notes", &[], &provenance("scope-a"))?;
        let _in_b = store.put("widget calibration notes", &[], &provenance("scope-b"))?;

        let narrowed_to_a = SearchOptions {
            scope: Some("scope-a".to_string()),
            ..SearchOptions::default()
        };
        let scoped_a = store.search("widget", &narrowed_to_a)?;
        assert_eq!(in_a, expect_one(&scoped_a)?.id);

        let narrowed_to_unused_scope = SearchOptions {
            scope: Some("scope-c".to_string()),
            ..SearchOptions::default()
        };
        let scoped_b = store.search("widget", &narrowed_to_unused_scope)?;
        assert!(scoped_b.is_empty());

        let all_scopes = store.search("widget", &SearchOptions::default())?;
        assert_eq!(2, all_scopes.len());
        Ok(())
    }

    #[test]
    fn required_tags_must_all_be_present() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let both = Tag::new("alpha")?;
        let other = Tag::new("beta")?;
        let one_tag = Tag::new("gamma")?;

        let with_both = store.put("body one", &[both.clone(), other.clone()], &prov)?;
        let _with_one = store.put("body two", &[one_tag], &prov)?;

        let options = SearchOptions {
            required_tags: vec![both, other],
            ..SearchOptions::default()
        };
        let hits = store.search("body", &options)?;

        assert_eq!(with_both, expect_one(&hits)?.id);
        Ok(())
    }

    #[test]
    fn superseded_hidden_by_default_and_shown_with_flag() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let id = store.put("supersede me searchword", &[], &prov)?;
        let update = store.conn().execute(
            "UPDATE memories SET state = 'superseded' WHERE id = ?1",
            params![id.to_string()],
        );
        update?;

        let hidden = store.search("searchword", &SearchOptions::default())?;
        assert!(hidden.is_empty());

        let options = SearchOptions {
            include_superseded: true,
            ..SearchOptions::default()
        };
        let shown = store.search("searchword", &options)?;
        assert_eq!(id, expect_one(&shown)?.id);
        Ok(())
    }

    #[test]
    fn retracted_always_hidden_even_with_include_superseded()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let id = store.put("retract me searchword", &[], &prov)?;
        let update = store.conn().execute(
            "UPDATE memories SET state = 'retracted' WHERE id = ?1",
            params![id.to_string()],
        );
        update?;

        let options = SearchOptions {
            include_superseded: true,
            ..SearchOptions::default()
        };
        let hits = store.search("searchword", &options)?;
        assert!(hits.is_empty());
        Ok(())
    }

    #[test]
    fn empty_query_returns_empty_without_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let hits = store.search("", &SearchOptions::default())?;
        assert!(hits.is_empty());
        Ok(())
    }

    #[test]
    fn all_punctuation_query_returns_empty_without_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let hits = store.search("***---:::", &SearchOptions::default())?;
        assert!(hits.is_empty());
        Ok(())
    }

    #[test]
    fn limit_caps_the_number_of_hits() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        for _ in 0..3 {
            store.put("capped search term", &[], &prov)?;
        }

        let options = SearchOptions {
            limit: 2,
            ..SearchOptions::default()
        };
        let hits = store.search("capped", &options)?;
        assert_eq!(2, hits.len());
        Ok(())
    }

    #[test]
    fn tag_text_matches_via_fts_tags_column() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let tagword = Tag::new("distinctivetagword")?;
        let id = store.put("body without the tag term", &[tagword], &prov)?;

        let hits = store.search("distinctivetagword", &SearchOptions::default())?;
        assert_eq!(id, expect_one(&hits)?.id);
        Ok(())
    }

    #[test]
    fn hyphenated_query_matches_hyphenated_tag() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let tag = Tag::new("agent-memory")?;
        let id = store.put("unrelated body text", &[tag], &prov)?;

        let hits = store.search("agent-memory", &SearchOptions::default())?;
        assert_eq!(id, expect_one(&hits)?.id);
        Ok(())
    }

    #[test]
    fn ties_break_by_created_at_descending() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");

        let older = store.put("identical tie body", &[], &prov)?;
        let newer = store.put("identical tie body", &[], &prov)?;

        let update_older = store.conn().execute(
            "UPDATE memories SET created_at = '2020-01-01T00:00:00Z' WHERE id = ?1",
            params![older.to_string()],
        );
        update_older?;
        let update_newer = store.conn().execute(
            "UPDATE memories SET created_at = '2024-01-01T00:00:00Z' WHERE id = ?1",
            params![newer.to_string()],
        );
        update_newer?;

        let hits = store.search("identical tie body", &SearchOptions::default())?;
        let (first, second) = expect_pair(&hits)?;
        assert_eq!(newer, first.id);
        assert_eq!(older, second.id);
        Ok(())
    }

    #[test]
    fn expect_pair_reports_wrong_length_as_a_typed_error() -> Result<(), String> {
        let items = [
            SearchHit {
                id: MemoryId::new(),
                score: 1.0,
            },
            SearchHit {
                id: MemoryId::new(),
                score: 2.0,
            },
            SearchHit {
                id: MemoryId::new(),
                score: 3.0,
            },
        ];
        let err = expect_err!(expect_pair(&items))?;
        assert_eq!("expected exactly 2 items, got 3", err);
        Ok(())
    }

    #[test]
    fn expect_one_reports_wrong_length_as_a_typed_error() -> Result<(), String> {
        let items: [SearchHit; 0] = [];
        let err = expect_err!(expect_one(&items))?;
        assert_eq!("expected exactly 1 item, got 0", err);
        Ok(())
    }

    #[test]
    fn search_default_matches_documented_values() {
        let options = SearchOptions::default();
        assert_eq!(10, options.limit);
        assert!(!options.include_superseded);
        assert_eq!(None, options.scope);
        assert!(options.required_tags.is_empty());
    }

    #[test]
    fn search_options_are_comparable_and_cloneable() {
        let a = SearchOptions::default();
        let b = a.clone();
        assert_eq!(a, b);
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }

    #[test]
    fn search_of_a_row_with_a_broken_schema_is_a_storage_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("scope-a");
        let _id = store.put("doomed body", &[], &prov)?;

        store.conn().execute_batch("DROP TABLE memories_fts;")?;

        let err = expect_err!(store.search("doomed", &SearchOptions::default()))?;
        assert!(matches!(err, MneneError::Storage(_)), "got {err:?}");
        Ok(())
    }
}
