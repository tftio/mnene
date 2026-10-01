//! Inheritance recall of active `mnene` memories.
//!
//! This module implements [`SqliteStore::recall`]: active memories in scope, with an optional
//! task filter, newest first. It runs a single read-only `SELECT` of
//! matching ids (no write transaction), then loads each full `Memory`
//! (with tags) via the store's private `load_memory` helper.

use rusqlite::params;

use crate::model::{Memory, MemoryId, MneneError};
use crate::store::{SqliteStore, load_memory, storage_error};

/// Options bounding a [`SqliteStore::recall`] query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallOptions {
    /// The maximum number of memories to return.
    pub limit: u32,
    /// Restricts recall to one scope; `None` recalls across all scopes.
    pub scope: Option<String>,
    /// Restricts recall to one task; `None` applies no task filter.
    pub task: Option<String>,
}

impl Default for RecallOptions {
    /// Defaults to a limit of 20, all scopes (`scope: None`), and no task
    /// filter (`task: None`).
    fn default() -> Self {
        Self {
            limit: 20,
            scope: None,
            task: None,
        }
    }
}

impl SqliteStore {
    /// Recalls active memories, most recently created first.
    ///
    /// Filters to `state = 'active'`, optionally bounded to one scope and
    /// one task, ordered by `created_at` descending then `id` descending
    /// (a deterministic tie-break for equal timestamps), and limited to
    /// `options.limit` rows. Superseded and retracted memories are never
    /// returned. This runs one read-only `SELECT` of matching ids, then
    /// loads each full [`Memory`] (with tags) by id; it opens no write
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn recall(&self, options: &RecallOptions) -> Result<Vec<Memory>, MneneError> {
        let ids = recall_ids(self, options)?;
        let mut memories = Vec::with_capacity(ids.len());
        for id in ids {
            memories.push(load_memory(self.conn(), id)?);
        }
        Ok(memories)
    }
}

/// Selects the ids of active memories matching `options`, newest first.
fn recall_ids(store: &SqliteStore, options: &RecallOptions) -> Result<Vec<MemoryId>, MneneError> {
    let sql = match (&options.scope, &options.task) {
        (Some(_), Some(_)) => {
            "SELECT id FROM memories WHERE state = 'active' AND scope = ?1 AND task = ?2 \
             ORDER BY created_at DESC, id DESC LIMIT ?3"
        }
        (Some(_), None) => {
            "SELECT id FROM memories WHERE state = 'active' AND scope = ?1 \
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        }
        (None, Some(_)) => {
            "SELECT id FROM memories WHERE state = 'active' AND task = ?1 \
             ORDER BY created_at DESC, id DESC LIMIT ?2"
        }
        (None, None) => {
            "SELECT id FROM memories WHERE state = 'active' \
             ORDER BY created_at DESC, id DESC LIMIT ?1"
        }
    };

    let mut statement = store.conn().prepare(sql).map_err(storage_error)?;
    let id_texts: Result<Vec<String>, rusqlite::Error> = match (&options.scope, &options.task) {
        (Some(scope), Some(task)) => statement
            .query_map(params![scope, task, options.limit], |row| row.get(0))
            .map_err(storage_error)?
            .collect(),
        (Some(scope), None) => statement
            .query_map(params![scope, options.limit], |row| row.get(0))
            .map_err(storage_error)?
            .collect(),
        (None, Some(task)) => statement
            .query_map(params![task, options.limit], |row| row.get(0))
            .map_err(storage_error)?
            .collect(),
        (None, None) => statement
            .query_map(params![options.limit], |row| row.get(0))
            .map_err(storage_error)?
            .collect(),
    };

    let mut ids = Vec::new();
    for text in id_texts.map_err(storage_error)? {
        ids.push(text.parse()?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::RecallOptions;
    use crate::model::{Memory, MemoryId, Provenance, Tag};
    use crate::store::SqliteStore;
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

    /// Returns a slice's first memory without indexing, so tests avoid
    /// `clippy::indexing_slicing`: an empty slice becomes a typed test
    /// failure instead of a panic. Deliberately not generic: a generic
    /// helper monomorphized once per call-site type produced a spurious
    /// zero-call instantiation under coverage instrumentation.
    fn first_of(items: &[Memory]) -> Result<&Memory, String> {
        items
            .first()
            .ok_or_else(|| "expected a first element".to_string())
    }

    #[test]
    fn first_of_reports_an_empty_slice() -> Result<(), String> {
        let items: [Memory; 0] = [];
        let err = expect_err!(first_of(&items))?;
        assert_eq!("expected a first element", err);
        Ok(())
    }

    fn provenance(scope: &str, task: Option<&str>) -> Provenance {
        Provenance {
            agent: "agent-a".to_string(),
            context: "default".to_string(),
            session: None,
            scope: scope.to_string(),
            scope_source: None,
            task: task.map(str::to_string),
        }
    }

    /// Sets a memory's `created_at` directly, bypassing the clock, so tests
    /// can construct a deterministic ordering.
    fn set_created_at(
        store: &mut SqliteStore,
        id: MemoryId,
        created_at: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let updated = store.conn_mut().execute(
            "UPDATE memories SET created_at = ?1 WHERE id = ?2",
            params![created_at, id.to_string()],
        );
        updated?;
        Ok(())
    }

    /// Sets a memory's `state` directly, bypassing `overwrite`/`retract`
    /// (landed in T007), so recall's exclusion of non-active rows can be
    /// tested ahead of that work.
    fn set_state(
        store: &mut SqliteStore,
        id: MemoryId,
        state: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let updated = store.conn_mut().execute(
            "UPDATE memories SET state = ?1 WHERE id = ?2",
            params![state, id.to_string()],
        );
        updated?;
        Ok(())
    }

    #[test]
    fn recall_on_empty_store_returns_empty() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let memories = store.recall(&RecallOptions::default())?;
        assert!(memories.is_empty());
        Ok(())
    }

    #[test]
    fn recall_orders_newest_first_with_id_tiebreak() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("mnene", None);

        let a = store.put("a", &[], &prov)?;
        let b = store.put("b", &[], &prov)?;
        let c = store.put("c", &[], &prov)?;

        // a and b share a timestamp (tie broken by id descending); c is
        // newer than both.
        set_created_at(&mut store, a, "2026-01-01T00:00:00Z")?;
        set_created_at(&mut store, b, "2026-01-01T00:00:00Z")?;
        set_created_at(&mut store, c, "2026-01-02T00:00:00Z")?;

        let memories = store.recall(&RecallOptions::default())?;
        let ids: Vec<MemoryId> = memories.iter().map(|memory| memory.id).collect();

        // a and b tie on created_at, so the query itself (not this
        // assertion) is what breaks the tie by id descending; sort here
        // only to build the expected order, with no untested branch.
        let mut expected_tail = [a, b];
        expected_tail.sort_by_key(|memory_id| std::cmp::Reverse(memory_id.to_string()));
        let [first, second] = expected_tail;
        assert_eq!(vec![c, first, second], ids);
        Ok(())
    }

    #[test]
    fn recall_bounds_to_a_scope() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let in_scope = store.put("in", &[], &provenance("scope-a", None))?;
        let _out_of_scope = store.put("out", &[], &provenance("scope-b", None))?;

        let options = RecallOptions {
            scope: Some("scope-a".to_string()),
            ..RecallOptions::default()
        };
        let memories = store.recall(&options)?;
        assert_eq!(1, memories.len());
        assert_eq!(in_scope, first_of(&memories)?.id);
        Ok(())
    }

    #[test]
    fn recall_with_no_scope_widens_to_all_scopes() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        store.put("in", &[], &provenance("scope-a", None))?;
        store.put("out", &[], &provenance("scope-b", None))?;

        let memories = store.recall(&RecallOptions::default())?;
        assert_eq!(2, memories.len());
        Ok(())
    }

    #[test]
    fn recall_filters_by_task() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let task_a = store.put("task a", &[], &provenance("mnene", Some("a")))?;
        store.put("task b", &[], &provenance("mnene", Some("b")))?;
        store.put("no task", &[], &provenance("mnene", None))?;

        let options = RecallOptions {
            task: Some("a".to_string()),
            ..RecallOptions::default()
        };
        let memories = store.recall(&options)?;
        assert_eq!(1, memories.len());
        assert_eq!(task_a, first_of(&memories)?.id);
        Ok(())
    }

    #[test]
    fn recall_bounds_by_scope_and_task_together() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let matching = store.put("match", &[], &provenance("scope-a", Some("a")))?;
        store.put("wrong scope", &[], &provenance("scope-b", Some("a")))?;
        store.put("wrong task", &[], &provenance("scope-a", Some("b")))?;

        let options = RecallOptions {
            limit: 20,
            scope: Some("scope-a".to_string()),
            task: Some("a".to_string()),
        };
        let memories = store.recall(&options)?;
        assert_eq!(1, memories.len());
        assert_eq!(matching, first_of(&memories)?.id);
        Ok(())
    }

    #[test]
    fn recall_applies_the_limit() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("mnene", None);

        for _ in 0..5 {
            store.put("body", &[], &prov)?;
        }

        let options = RecallOptions {
            limit: 2,
            ..RecallOptions::default()
        };
        let memories = store.recall(&options)?;
        assert_eq!(2, memories.len());
        Ok(())
    }

    #[test]
    fn recall_excludes_superseded_and_retracted() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("mnene", None);

        let active = store.put("active", &[], &prov)?;
        let superseded = store.put("superseded", &[], &prov)?;
        let retracted = store.put("retracted", &[], &prov)?;
        set_state(&mut store, superseded, "superseded")?;
        set_state(&mut store, retracted, "retracted")?;

        let memories = store.recall(&RecallOptions::default())?;
        let ids: Vec<MemoryId> = memories.iter().map(|memory| memory.id).collect();
        assert_eq!(vec![active], ids);
        Ok(())
    }

    #[test]
    fn recall_returns_tags_on_each_memory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance("mnene", None);
        let tags = vec![Tag::new("alpha")?, Tag::new("beta")?];

        store.put("tagged", &tags, &prov)?;

        let memories = store.recall(&RecallOptions::default())?;
        assert_eq!(1, memories.len());
        let rendered: Vec<&str> = first_of(&memories)?.tags.iter().map(Tag::as_str).collect();
        assert_eq!(vec!["alpha", "beta"], rendered);
        Ok(())
    }

    #[test]
    fn recall_default_has_documented_values() {
        let options = RecallOptions::default();
        assert_eq!(20, options.limit);
        assert_eq!(None, options.scope);
        assert_eq!(None, options.task);
    }

    #[test]
    fn recall_options_derives_debug_clone_eq() {
        let options = RecallOptions {
            limit: 5,
            scope: Some("scope-a".to_string()),
            task: Some("task-a".to_string()),
        };
        let cloned = options.clone();
        assert_eq!(options, cloned);
        assert!(format!("{options:?}").contains("scope-a"));
    }

    #[test]
    fn recall_ids_query_failure_is_a_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        store.conn_mut().execute_batch("DROP TABLE memories;")?;

        let err = expect_err!(store.recall(&RecallOptions::default()))?;
        assert!(
            matches!(err, crate::model::MneneError::Storage(_)),
            "got {err:?}"
        );
        Ok(())
    }
}
