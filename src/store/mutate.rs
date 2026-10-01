//! Compare-and-supersede mutation of `mnene` memories.
//!
//! This module implements [`SqliteStore::overwrite`](crate::store::SqliteStore::overwrite)
//! and [`SqliteStore::retract`](crate::store::SqliteStore::retract). Both verbs run inside `BEGIN
//! IMMEDIATE`: they load the predecessor, and, only if it is still
//! `active`, close it with a guarded `UPDATE ... WHERE id = ? AND state =
//! 'active'`. A changed-row count other than one means a concurrent writer
//! closed the row between the read and the update; that path rolls the
//! transaction back and reports [`MneneError::NotActive`] with the
//! winner's state and successor.

use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::{insert_memory, load_memory, storage_error};
use crate::model::{MemoryId, MneneError, Provenance, State, Tag, now_rfc3339, validate_body};
use crate::store::SqliteStore;

/// Runs the guarded `UPDATE` that closes an active memory row, setting its
/// terminal `state`, `superseded_by` pointer (`None` for retraction), and
/// closing provenance.
///
/// Returns `Ok(true)` when exactly one row was closed, and `Ok(false)` when
/// zero rows matched (the row was not `active` at the moment of the
/// update). The caller is responsible for rolling back the transaction and
/// reporting a typed error when this returns `Ok(false)`.
///
/// # Errors
///
/// Returns [`MneneError::Storage`] on any underlying `SQLite` failure.
fn close_active(
    tx: &Connection,
    id: MemoryId,
    new_state: State,
    superseded_by: Option<MemoryId>,
    closed_at: &str,
    closed_by: &str,
) -> Result<bool, MneneError> {
    let id_text = id.to_string();
    let superseded_by_text = superseded_by.map(|s| s.to_string());

    let changed = tx
        .execute(
            "UPDATE memories SET state = ?1, superseded_by = ?2, closed_at = ?3, closed_by = ?4 \
             WHERE id = ?5 AND state = 'active'",
            params![
                new_state.as_str(),
                superseded_by_text,
                closed_at,
                closed_by,
                id_text,
            ],
        )
        .map_err(storage_error)?;

    Ok(changed == 1)
}

/// Re-reads `id` to build the typed error that reports why a
/// compare-and-supersede update failed: [`MneneError::NotFound`] if the row
/// has vanished (it cannot: rows are never deleted), or
/// [`MneneError::NotActive`] naming the row's current state and successor.
///
/// This is always called before the enclosing transaction rolls back, while
/// this connection still holds the write lock the failed guarded update
/// took out — nothing else can have changed the row since, so reading
/// through the still-open transaction and reading after rollback would see
/// the same row.
fn not_active_error(conn: &Connection, id: MemoryId) -> MneneError {
    match load_memory(conn, id) {
        Ok(memory) => MneneError::NotActive {
            id,
            state: memory.state,
            superseded_by: memory.superseded_by,
        },
        Err(err) => err,
    }
}

/// Finishes a compare-and-supersede attempt: commits `tx` and returns
/// `success` when `closed` is true (the guarded update in [`close_active`]
/// closed exactly one row); otherwise rolls `tx` back and reports
/// [`MneneError::NotActive`] (or [`MneneError::NotFound`]) for `id` via
/// [`not_active_error`].
///
/// Shared by [`SqliteStore::overwrite`] and [`SqliteStore::retract`] so
/// that the rollback-and-report path — unreachable through either verb in
/// a genuine race, because `BEGIN IMMEDIATE` serializes writers and so
/// guarantees `closed` is always true by the time a real caller gets here
/// — is exercised directly, by a unit test that manufactures a `false`
/// `closed` value against an already-closed row.
///
/// # Errors
///
/// Returns [`MneneError::Storage`] when the commit or rollback itself
/// fails. When `closed` is false, always returns a
/// [`MneneError::NotActive`] or [`MneneError::NotFound`] describing `id`'s
/// current state.
fn finish<T>(tx: Transaction<'_>, id: MemoryId, closed: bool, success: T) -> Result<T, MneneError> {
    if closed {
        tx.commit().map_err(storage_error)?;
        Ok(success)
    } else {
        let error = not_active_error(&tx, id);
        tx.rollback().map_err(storage_error)?;
        Err(error)
    }
}

impl SqliteStore {
    /// Overwrites an active memory, minting a successor that points back at
    /// it via `supersedes`.
    ///
    /// Validates `body` before opening any transaction. Inside `BEGIN
    /// IMMEDIATE`: loads the predecessor (missing is [`MneneError::NotFound`]);
    /// if it is not [`State::Active`], returns [`MneneError::NotActive`]
    /// immediately, no successor row is created. Otherwise mints a
    /// successor id and timestamp, inherits the predecessor's tags when
    /// `tags` is empty (else uses `tags` as given), inserts the successor
    /// row with `supersedes` set to `id`, then closes the predecessor with
    /// a guarded update to `state = 'superseded'`. If that guarded update
    /// does not close exactly one row (a concurrent writer closed it
    /// first), the whole transaction is rolled back — leaving no successor
    /// row behind — and [`MneneError::NotActive`] is reported.
    ///
    /// The successor's provenance is `provenance` (the calling agent), not
    /// the predecessor's.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::EmptyBody`] when `body` is blank after
    /// trimming, [`MneneError::NotFound`] when `id` does not exist,
    /// [`MneneError::NotActive`] when `id` is not currently active, and
    /// [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn overwrite(
        &mut self,
        id: MemoryId,
        body: &str,
        tags: &[Tag],
        provenance: &Provenance,
    ) -> Result<MemoryId, MneneError> {
        let trimmed = validate_body(body)?;

        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;

        let predecessor = load_memory(&tx, id)?;
        if predecessor.state != State::Active {
            return Err(MneneError::NotActive {
                id,
                state: predecessor.state,
                superseded_by: predecessor.superseded_by,
            });
        }

        let successor_id = MemoryId::new();
        let now = now_rfc3339();
        let successor_tags: &[Tag] = if tags.is_empty() {
            &predecessor.tags
        } else {
            tags
        };

        let inserted = insert_memory(
            &tx,
            successor_id,
            &trimmed,
            successor_tags,
            provenance,
            Some(id),
            &now,
        );
        inserted?;

        let new_state = State::Superseded;
        let closed_result = close_active(
            &tx,
            id,
            new_state,
            Some(successor_id),
            &now,
            &provenance.agent,
        );
        let closed = closed_result?;

        finish(tx, id, closed, successor_id)
    }

    /// Retracts an active memory, retiring it with no replacement.
    ///
    /// Inside `BEGIN IMMEDIATE`: loads the memory (missing is
    /// [`MneneError::NotFound`]); if it is not [`State::Active`], returns
    /// [`MneneError::NotActive`] immediately. Otherwise closes it with a
    /// guarded update to `state = 'retracted'` and `superseded_by = NULL`.
    /// If that guarded update does not close exactly one row, the
    /// transaction is rolled back and [`MneneError::NotActive`] is
    /// reported.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::NotFound`] when `id` does not exist,
    /// [`MneneError::NotActive`] when `id` is not currently active, and
    /// [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn retract(&mut self, id: MemoryId, provenance: &Provenance) -> Result<(), MneneError> {
        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;

        let memory = load_memory(&tx, id)?;
        if memory.state != State::Active {
            return Err(MneneError::NotActive {
                id,
                state: memory.state,
                superseded_by: memory.superseded_by,
            });
        }

        let now = now_rfc3339();
        let closed = close_active(&tx, id, State::Retracted, None, &now, &provenance.agent)?;

        finish(tx, id, closed, ())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rusqlite::TransactionBehavior;

    use super::{close_active, finish, not_active_error};
    use crate::model::{MemoryId, MneneError, Provenance, State, Tag, now_rfc3339};
    use crate::store::SqliteStore;

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

    fn provenance(agent: &str) -> Provenance {
        Provenance {
            agent: agent.to_string(),
            context: "default".to_string(),
            session: Some("session-1".to_string()),
            scope: "mnene".to_string(),
            scope_source: Some(crate::model::ScopeSource::Env),
            task: Some("t007".to_string()),
        }
    }

    #[test]
    fn overwrite_chains_predecessor_and_successor() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let old = store.put("first version", &[], &writer)?;

        let overwriter = provenance("writer-b");
        let new = store.overwrite(old, "second version", &[], &overwriter)?;

        let predecessor = store.get(old)?;
        assert_eq!(State::Superseded, predecessor.state);
        assert_eq!(Some(new), predecessor.superseded_by);
        assert!(predecessor.closed_at.is_some());
        assert_eq!(Some("writer-b".to_string()), predecessor.closed_by);

        let successor = store.get(new)?;
        assert_eq!(Some(old), successor.supersedes);
        assert_eq!(State::Active, successor.state);
        assert_eq!("second version", successor.body);
        assert_eq!(overwriter, successor.provenance);
        assert_eq!(None, successor.superseded_by);

        // Invariant: after a second overwrite, every memory still has at
        // most one superseded_by.
        let newer = store.overwrite(new, "third version", &[], &writer)?;
        let successor_after = store.get(new)?;
        assert_eq!(Some(newer), successor_after.superseded_by);
        let old_after = store.get(old)?;
        assert_eq!(Some(new), old_after.superseded_by);
        Ok(())
    }

    #[test]
    fn overwrite_with_empty_tags_inherits_predecessor_tags()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let tags = vec![Tag::new("alpha")?, Tag::new("beta")?];
        let old = store.put("first version", &tags, &writer)?;

        let new = store.overwrite(old, "second version", &[], &writer)?;
        let successor = store.get(new)?;

        let rendered: Vec<&str> = successor.tags.iter().map(Tag::as_str).collect();
        assert_eq!(vec!["alpha", "beta"], rendered);
        Ok(())
    }

    #[test]
    fn overwrite_with_tags_replaces_predecessor_tags() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let old_tags = vec![Tag::new("alpha")?];
        let old = store.put("first version", &old_tags, &writer)?;

        let new_tags = vec![Tag::new("gamma")?];
        let new = store.overwrite(old, "second version", &new_tags, &writer)?;
        let successor = store.get(new)?;

        let rendered: Vec<&str> = successor.tags.iter().map(Tag::as_str).collect();
        assert_eq!(vec!["gamma"], rendered);
        Ok(())
    }

    #[test]
    fn overwrite_of_superseded_id_is_not_active() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let old = store.put("first version", &[], &writer)?;
        let new = store.overwrite(old, "second version", &[], &writer)?;

        let err = expect_err!(store.overwrite(old, "third version", &[], &writer))?;
        assert_eq!(
            MneneError::NotActive {
                id: old,
                state: State::Superseded,
                superseded_by: Some(new),
            },
            err
        );
        Ok(())
    }

    #[test]
    fn overwrite_of_retracted_id_is_not_active_with_no_successor()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;
        store.retract(id, &writer)?;

        let err = expect_err!(store.overwrite(id, "new body", &[], &writer))?;
        assert_eq!(
            MneneError::NotActive {
                id,
                state: State::Retracted,
                superseded_by: None,
            },
            err
        );
        Ok(())
    }

    #[test]
    fn overwrite_of_missing_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let unknown = MemoryId::new();

        let err = expect_err!(store.overwrite(unknown, "body", &[], &writer))?;
        assert_eq!(MneneError::NotFound(unknown), err);
        Ok(())
    }

    #[test]
    fn overwrite_rejects_blank_body_before_any_transaction()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;

        let err = expect_err!(store.overwrite(id, "   ", &[], &writer))?;
        assert_eq!(MneneError::EmptyBody, err);

        // No transaction side effects: the row count is unchanged and the
        // predecessor is untouched.
        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        assert_eq!(1, count);
        let memory = store.get(id)?;
        assert_eq!(State::Active, memory.state);
        Ok(())
    }

    #[test]
    fn retract_closes_an_active_memory_with_no_successor() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;

        let retractor = provenance("writer-b");
        store.retract(id, &retractor)?;

        let memory = store.get(id)?;
        assert_eq!(State::Retracted, memory.state);
        assert_eq!(None, memory.superseded_by);
        assert!(memory.closed_at.is_some());
        assert_eq!(Some("writer-b".to_string()), memory.closed_by);
        Ok(())
    }

    #[test]
    fn retract_of_superseded_id_is_not_active() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let old = store.put("body", &[], &writer)?;
        let new = store.overwrite(old, "new body", &[], &writer)?;

        let err = expect_err!(store.retract(old, &writer))?;
        assert_eq!(
            MneneError::NotActive {
                id: old,
                state: State::Superseded,
                superseded_by: Some(new),
            },
            err
        );
        Ok(())
    }

    #[test]
    fn retract_of_retracted_id_is_not_active() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;
        store.retract(id, &writer)?;

        let err = expect_err!(store.retract(id, &writer))?;
        assert_eq!(
            MneneError::NotActive {
                id,
                state: State::Retracted,
                superseded_by: None,
            },
            err
        );
        Ok(())
    }

    #[test]
    fn retract_of_missing_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let unknown = MemoryId::new();

        let err = expect_err!(store.retract(unknown, &writer))?;
        assert_eq!(MneneError::NotFound(unknown), err);
        Ok(())
    }

    #[test]
    fn failed_overwrite_leaves_memory_count_unchanged() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let old = store.put("body", &[], &writer)?;
        store.retract(old, &writer)?;

        let before: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;

        let err = expect_err!(store.overwrite(old, "new body", &[], &writer))?;
        assert!(matches!(err, MneneError::NotActive { .. }));

        let after: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn two_connection_race_yields_not_active_naming_first_successor()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("mnene.db");
        let writer = provenance("writer-a");

        let mut first = SqliteStore::open(&db_path)?;
        first.conn_mut().busy_timeout(Duration::from_secs(5))?;
        let id = first.put("body", &[], &writer)?;

        let mut second = SqliteStore::open(&db_path)?;
        second.conn_mut().busy_timeout(Duration::from_secs(5))?;

        let overwriter_a = provenance("writer-a");
        let first_successor = first.overwrite(id, "from first", &[], &overwriter_a)?;

        let overwriter_b = provenance("writer-b");
        let err = expect_err!(second.overwrite(id, "from second", &[], &overwriter_b))?;
        assert_eq!(
            MneneError::NotActive {
                id,
                state: State::Superseded,
                superseded_by: Some(first_successor),
            },
            err
        );
        Ok(())
    }

    #[test]
    fn close_active_returns_false_for_an_already_closed_row()
    -> Result<(), Box<dyn std::error::Error>> {
        // load_memory inside overwrite/retract's own transaction already
        // filters non-active rows before close_active ever runs, so the
        // changed-row-count-mismatch branch inside close_active is
        // reachable in single-threaded, single-transaction tests only by
        // calling it directly against a row that is already non-active.
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;
        store.retract(id, &writer)?;

        let now = now_rfc3339();
        let closed_result = close_active(
            store.conn(),
            id,
            State::Superseded,
            None,
            &now,
            &writer.agent,
        );
        let closed = closed_result?;
        assert!(!closed);

        // The row is unaffected by the failed attempt.
        let memory = store.get(id)?;
        assert_eq!(State::Retracted, memory.state);
        Ok(())
    }

    #[test]
    fn not_active_error_reports_not_found_when_the_row_has_vanished()
    -> Result<(), Box<dyn std::error::Error>> {
        // Rows are never deleted, so not_active_error's NotFound arm is
        // unreachable through overwrite or retract in practice; call it
        // directly against an id that was never written to exercise it.
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let unknown = MemoryId::new();

        let err = not_active_error(store.conn(), unknown);
        assert_eq!(MneneError::NotFound(unknown), err);
        Ok(())
    }

    #[test]
    fn finish_rolls_back_and_reports_not_active_when_not_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        // As with close_active's own mismatch branch, finish's
        // rollback-and-report path is unreachable through overwrite or
        // retract in a genuine race: BEGIN IMMEDIATE serializes writers, so
        // by the time a second writer's transaction can even begin, the
        // first has already committed and the early active-state check
        // catches it. finish is called directly here, with a manufactured
        // `closed = false`, to cover that path.
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;
        store.retract(id, &writer)?;

        let tx = store
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let err = expect_err!(finish(tx, id, false, ()))?;
        assert_eq!(
            MneneError::NotActive {
                id,
                state: State::Retracted,
                superseded_by: None,
            },
            err
        );

        // The transaction was rolled back rather than left open or
        // committed: the store is still usable afterward.
        let memory = store.get(id)?;
        assert_eq!(State::Retracted, memory.state);
        Ok(())
    }

    #[test]
    fn finish_commits_and_returns_success_when_closed() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let writer = provenance("writer-a");
        let id = store.put("body", &[], &writer)?;

        let tx = store
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let value = finish(tx, id, true, "ok")?;
        assert_eq!("ok", value);

        // Nothing was changed by finish itself: the memory is unaffected.
        let memory = store.get(id)?;
        assert_eq!(State::Active, memory.state);
        Ok(())
    }
}
