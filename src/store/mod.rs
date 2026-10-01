//! SQLite-backed storage for `mnene` memories.
//!
//! This module implements [`SqliteStore::open`](crate::store::SqliteStore::open)
//! (schema bootstrap and `user_version` checking),
//! [`SqliteStore::put`](crate::store::SqliteStore::put), and
//! [`SqliteStore::get`](crate::store::SqliteStore::get), and declares the
//! `search`, `mutate`, `recall`, and `scopes` submodules that add further
//! `SqliteStore` methods. No `rusqlite` type appears in a public
//! signature: every function that must speak `rusqlite` either stays
//! `pub(crate)` or maps its errors to
//! [`MneneError::Storage`](crate::model::MneneError::Storage) before
//! returning.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior, params};

use crate::model::{
    Memory, MemoryId, MneneError, Provenance, ScopeSource, State, Tag, now_rfc3339, validate_body,
};

/// Full-text search over `mnene` memories.
pub mod search;

/// Compare-and-supersede mutation of `mnene` memories.
pub mod mutate;

/// Inheritance recall of active `mnene` memories.
pub mod recall;

/// Inventory of distinct scopes and their sources.
pub mod scopes;

/// The `user_version` this build expects a bootstrapped database to carry.
///
/// Version 2 adds the
/// nullable `scope_source` column; a version 1 database is migrated to it
/// in place by [`SqliteStore::open`], and no row is rescoped by that
/// migration.
const SCHEMA_VERSION: i64 = 2;

/// The schema created for a fresh (`user_version` 0) database, including
/// the `scope_source` column added in version 2. Executed as one batch inside an explicit transaction so the
/// tables, the FTS index, and the version stamp land together or not at
/// all.
const SCHEMA_SQL: &str = "
BEGIN IMMEDIATE;
CREATE TABLE memories (
  id            TEXT PRIMARY KEY,
  body          TEXT NOT NULL,
  state         TEXT NOT NULL CHECK (state IN ('active','superseded','retracted')),
  supersedes    TEXT REFERENCES memories(id),
  superseded_by TEXT REFERENCES memories(id),
  scope         TEXT NOT NULL,
  scope_source  TEXT,
  task          TEXT,
  agent         TEXT NOT NULL,
  context       TEXT NOT NULL,
  session       TEXT,
  created_at    TEXT NOT NULL,
  closed_at     TEXT,
  closed_by     TEXT
);
CREATE TABLE tags (
  memory_id TEXT NOT NULL REFERENCES memories(id),
  tag       TEXT NOT NULL,
  PRIMARY KEY (memory_id, tag)
);
CREATE VIRTUAL TABLE memories_fts USING fts5 (id UNINDEXED, body, tags);
CREATE INDEX memories_scope_state ON memories (scope, state, created_at);
PRAGMA user_version = 2;
COMMIT;
";

/// mnene's first in-place schema migration: a version 1 database
/// gains the nullable `scope_source` column, with every existing row's
/// value left `NULL` (`SQLite`'s default for a newly added column with no
/// `DEFAULT` clause). No row's `scope` changes; this migration only adds a
/// column, it never rescopes data. Rows whose stored scope should change
/// are corrected by an operator outside this migration, because the right
/// scope for an old row cannot be derived from the row itself.
const MIGRATE_V1_TO_V2_SQL: &str = "
BEGIN IMMEDIATE;
ALTER TABLE memories ADD COLUMN scope_source TEXT;
PRAGMA user_version = 2;
COMMIT;
";

/// A `SQLite`-backed store of memories, opened at a single per-context
/// database file.
///
/// The wrapped [`rusqlite::Connection`] is private: nothing outside
/// `src/store/` ever sees a `rusqlite` type, so storage stays wrapped
/// behind a domain-specific interface (ENG-010).
pub struct SqliteStore {
    conn: Connection,
}

/// Maps any `rusqlite` error to [`MneneError::Storage`], carrying the
/// underlying error text. This is the one place `rusqlite::Error` crosses
/// into the domain error type. Deliberately a free function rather than a
/// `From<rusqlite::Error>` impl: a trait impl on `MneneError` is public API
/// regardless of visibility keywords (it would let any downstream crate
/// call `.into()`/`?` on a `rusqlite::Error` into `MneneError`), which
/// would put a `rusqlite` type in the public surface (ENG-010).
pub(crate) fn storage_error(err: rusqlite::Error) -> MneneError {
    let boxed: Box<dyn std::error::Error> = Box::new(err);
    MneneError::Storage(boxed.to_string())
}

impl SqliteStore {
    /// Opens (creating if necessary) the `SQLite` database at `path`.
    ///
    /// Creates `path`'s parent directories, opens the file, sets a busy
    /// timeout, enables the WAL journal mode and foreign key enforcement,
    /// then checks `PRAGMA user_version`: a fresh file (version 0) is
    /// bootstrapped with the schema and stamped to version 1; a file
    /// already at version 1 is used as-is; any other version is rejected.
    ///
    /// The busy timeout matters because `put`, `overwrite`, and `retract`
    /// each open a `BEGIN IMMEDIATE` transaction (see `src/store/mutate.rs`):
    /// `IMMEDIATE` takes `SQLite`'s write lock up front rather than at first
    /// write, so a second process's own `BEGIN IMMEDIATE` against the same
    /// file collides with it immediately. Without a busy timeout, `SQLite`'s
    /// default behavior is to fail that second `BEGIN IMMEDIATE` at once
    /// with `SQLITE_BUSY` rather than waiting for the first transaction to
    /// finish, so two `mnene` processes racing to overwrite or retract the
    /// same memory would see spurious storage errors instead of one clean
    /// winner and one typed `NotActive` loser. Setting a busy timeout makes
    /// `SQLite` retry internally for up to that long before giving up,
    /// which is enough for one process's short compare-and-supersede
    /// transaction to complete and free the lock for the next.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] when the parent directory cannot be
    /// created or the connection cannot be opened or configured, and
    /// [`MneneError::SchemaVersion`] when an existing database's
    /// `user_version` is neither 0 nor 1.
    pub fn open(path: &Path) -> Result<Self, MneneError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| MneneError::Storage(err.to_string()))?;
        }

        let conn = Connection::open(path).map_err(storage_error)?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(storage_error)?;
        let journal_mode_result: Result<String, rusqlite::Error> =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0));
        let _journal_mode = journal_mode_result.map_err(storage_error)?;

        let user_version_result: Result<i64, rusqlite::Error> =
            conn.query_row("PRAGMA user_version", [], |row| row.get(0));
        let found = user_version_result.map_err(storage_error)?;

        match found {
            0 => conn.execute_batch(SCHEMA_SQL).map_err(storage_error)?,
            1 => conn
                .execute_batch(MIGRATE_V1_TO_V2_SQL)
                .map_err(storage_error)?,
            version if version == SCHEMA_VERSION => {}
            found => {
                return Err(MneneError::SchemaVersion {
                    found,
                    expected: SCHEMA_VERSION,
                });
            }
        }

        Ok(Self { conn })
    }

    /// Returns a reference to the wrapped connection, for sibling store
    /// modules and tests that need to run their own statements against it.
    pub(crate) const fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Returns a mutable reference to the wrapped connection, for sibling
    /// store modules that open their own write transactions.
    pub(crate) const fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Stores a new memory and returns its freshly minted id.
    ///
    /// Validates `body`, mints a [`MemoryId`], and inserts the memory row,
    /// its tag rows, and its FTS row inside one `BEGIN IMMEDIATE`
    /// transaction: all three writes land, or none do.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::EmptyBody`] when `body` is blank after
    /// trimming, and [`MneneError::Storage`] on any underlying `SQLite`
    /// failure.
    pub fn put(
        &mut self,
        body: &str,
        tags: &[Tag],
        provenance: &Provenance,
    ) -> Result<MemoryId, MneneError> {
        let trimmed = validate_body(body)?;
        let id = MemoryId::new();
        let created_at = now_rfc3339();

        let tx = self
            .conn_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        insert_memory(&tx, id, &trimmed, tags, provenance, None, &created_at)?;
        tx.commit().map_err(storage_error)?;

        Ok(id)
    }

    /// Fetches a memory by id, including its tags.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::NotFound`] when no memory with `id` exists,
    /// and [`MneneError::Storage`] on any underlying `SQLite` failure.
    pub fn get(&self, id: MemoryId) -> Result<Memory, MneneError> {
        load_memory(self.conn(), id)
    }
}

/// Inserts one memory's three rows: the `memories` row (always `state =
/// 'active'`), one `tags` row per distinct tag, and the `memories_fts` row
/// with tags joined by single spaces.
///
/// `supersedes` and `created_at` are taken as arguments rather than
/// computed here so that `overwrite` (T007) can reuse this for a successor
/// row that points back at its predecessor.
///
/// # Errors
///
/// Returns [`MneneError::Storage`] on any underlying `SQLite` failure.
pub(crate) fn insert_memory(
    conn: &Connection,
    id: MemoryId,
    body: &str,
    tags: &[Tag],
    provenance: &Provenance,
    supersedes: Option<MemoryId>,
    created_at: &str,
) -> Result<(), MneneError> {
    let id_text = id.to_string();
    let supersedes_text = supersedes.map(|s| s.to_string());

    let memory_row = conn.execute(
        "INSERT INTO memories \
         (id, body, state, supersedes, superseded_by, scope, scope_source, task, agent, context, session, created_at, closed_at, closed_by) \
         VALUES (?1, ?2, 'active', ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL, NULL)",
        params![
            id_text,
            body,
            supersedes_text,
            provenance.scope,
            provenance.scope_source.map(ScopeSource::as_str),
            provenance.task,
            provenance.agent,
            provenance.context,
            provenance.session,
            created_at,
        ],
    );
    memory_row.map_err(storage_error)?;

    let mut seen = HashSet::new();
    let mut distinct_tags: Vec<&str> = Vec::new();
    for tag in tags {
        if seen.insert(tag.as_str()) {
            distinct_tags.push(tag.as_str());
        }
    }

    for tag in &distinct_tags {
        let tag_row = conn.execute(
            "INSERT INTO tags (memory_id, tag) VALUES (?1, ?2)",
            params![id_text, tag],
        );
        tag_row.map_err(storage_error)?;
    }

    let tags_joined = distinct_tags.join(" ");
    let fts_row = conn.execute(
        "INSERT INTO memories_fts (id, body, tags) VALUES (?1, ?2, ?3)",
        params![id_text, body, tags_joined],
    );
    fts_row.map_err(storage_error)?;

    Ok(())
}

/// Loads one memory's tags, ordered by tag text ascending.
///
/// # Errors
///
/// Returns [`MneneError::Storage`] on any underlying `SQLite` failure, or
/// [`MneneError::InvalidTag`] if a stored tag somehow fails normalization.
pub(crate) fn load_tags(conn: &Connection, id: MemoryId) -> Result<Vec<Tag>, MneneError> {
    let id_text = id.to_string();
    let mut statement = conn
        .prepare("SELECT tag FROM tags WHERE memory_id = ?1 ORDER BY tag ASC")
        .map_err(storage_error)?;
    let rows = statement
        .query_map(params![id_text], |row| row.get::<_, String>(0))
        .map_err(storage_error)?;

    let mut tags = Vec::new();
    for row in rows {
        let text = row.map_err(storage_error)?;
        tags.push(Tag::new(&text)?);
    }
    Ok(tags)
}

/// Loads a full memory record (with tags) by id, for use both from
/// [`SqliteStore::get`] and from inside a transaction via `&Transaction`,
/// which derefs to `&Connection`.
///
/// # Errors
///
/// Returns [`MneneError::NotFound`] when no memory with `id` exists, and
/// [`MneneError::Storage`] on any underlying `SQLite` failure.
pub(crate) fn load_memory(conn: &Connection, id: MemoryId) -> Result<Memory, MneneError> {
    let id_text = id.to_string();

    let row = conn.query_row(
        "SELECT body, state, supersedes, superseded_by, scope, scope_source, task, agent, context, session, created_at, closed_at, closed_by \
         FROM memories WHERE id = ?1",
        params![id_text],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
            ))
        },
    );

    let (
        body,
        state_text,
        supersedes_text,
        superseded_by_text,
        scope,
        scope_source_text,
        task,
        agent,
        context,
        session,
        created_at,
        closed_at,
        closed_by,
    ) = match row {
        Ok(values) => values,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Err(MneneError::NotFound(id)),
        Err(err) => return Err(storage_error(err)),
    };

    let state: State = state_text.parse()?;
    let supersedes = supersedes_text.map(|text| text.parse()).transpose()?;
    let superseded_by = superseded_by_text.map(|text| text.parse()).transpose()?;
    let scope_source = scope_source_text.map(|text| text.parse()).transpose()?;
    let tags = load_tags(conn, id)?;

    Ok(Memory {
        id,
        body,
        state,
        supersedes,
        superseded_by,
        provenance: Provenance {
            agent,
            context,
            session,
            scope,
            scope_source,
            task,
        },
        tags,
        created_at,
        closed_at,
        closed_by,
    })
}

#[cfg(test)]
mod tests {
    use super::{MneneError, SqliteStore, State};
    use crate::model::{MemoryId, Provenance, Tag};
    use std::path::Path;

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

    fn provenance() -> Provenance {
        Provenance {
            agent: "agent-a".to_string(),
            context: "default".to_string(),
            session: Some("session-1".to_string()),
            scope: "mnene".to_string(),
            scope_source: Some(crate::model::ScopeSource::Directory),
            task: Some("t005".to_string()),
        }
    }

    #[test]
    fn put_get_round_trip_preserves_every_provenance_field()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();

        let id = store.put("hello there", &[], &prov)?;
        let memory = store.get(id)?;

        assert_eq!(id, memory.id);
        assert_eq!("hello there", memory.body);
        assert_eq!(State::Active, memory.state);
        assert_eq!(None, memory.supersedes);
        assert_eq!(None, memory.superseded_by);
        assert_eq!(prov, memory.provenance);
        assert!(memory.tags.is_empty());
        assert_eq!(None, memory.closed_at);
        assert_eq!(None, memory.closed_by);
        assert!(!memory.created_at.is_empty());
        Ok(())
    }

    #[test]
    fn put_get_round_trip_with_no_session_or_task() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = Provenance {
            agent: "agent-b".to_string(),
            context: "default".to_string(),
            session: None,
            scope: "mnene".to_string(),
            scope_source: None,
            task: None,
        };

        let id = store.put("body text", &[], &prov)?;
        let memory = store.get(id)?;

        assert_eq!(prov, memory.provenance);
        Ok(())
    }

    #[test]
    fn put_persists_tags_ordered_and_deduplicated() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();

        let tags = vec![Tag::new("zeta")?, Tag::new("alpha")?, Tag::new("zeta")?];
        let id = store.put("tagged body", &tags, &prov)?;
        let memory = store.get(id)?;

        let rendered: Vec<&str> = memory.tags.iter().map(Tag::as_str).collect();
        assert_eq!(vec!["alpha", "zeta"], rendered);
        Ok(())
    }

    #[test]
    fn put_rejects_empty_body_and_writes_no_row() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();

        let err = expect_err!(store.put("   ", &[], &prov))?;
        assert_eq!(MneneError::EmptyBody, err);

        let count: i64 = store
            .conn()
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        assert_eq!(0, count);
        Ok(())
    }

    #[test]
    fn get_of_unknown_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let unknown = MemoryId::new();

        let err = expect_err!(store.get(unknown))?;
        assert_eq!(MneneError::NotFound(unknown), err);
        Ok(())
    }

    #[test]
    fn insert_memory_records_and_parses_a_supersedes_pointer()
    -> Result<(), Box<dyn std::error::Error>> {
        // insert_memory's own supersedes parameter, and load_memory's parse
        // of a non-null `supersedes` column, are only exercised once a
        // successor row exists; put() alone never creates one (that is
        // T007's overwrite). Calling insert_memory directly here, ahead of
        // T007, exercises both paths.
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();
        let predecessor = store.put("predecessor", &[], &prov)?;

        let successor = MemoryId::new();
        let inserted = super::insert_memory(
            store.conn(),
            successor,
            "successor",
            &[],
            &prov,
            Some(predecessor),
            &crate::model::now_rfc3339(),
        );
        inserted?;

        let memory = store.get(successor)?;
        assert_eq!(Some(predecessor), memory.supersedes);
        Ok(())
    }

    #[test]
    fn get_parses_a_superseded_by_pointer() -> Result<(), Box<dyn std::error::Error>> {
        // Only T007's overwrite ever sets `superseded_by` on a row; ahead
        // of that landing, set it directly to exercise load_memory's parse
        // of a non-null `superseded_by` column.
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();
        let predecessor = store.put("predecessor", &[], &prov)?;
        let successor = store.put("successor", &[], &prov)?;

        let updated = store.conn_mut().execute(
            "UPDATE memories SET superseded_by = ?1 WHERE id = ?2",
            rusqlite::params![successor.to_string(), predecessor.to_string()],
        );
        updated?;

        let memory = store.get(predecessor)?;
        assert_eq!(Some(successor), memory.superseded_by);
        Ok(())
    }

    #[test]
    fn get_of_a_row_with_a_broken_schema_is_a_storage_error()
    -> Result<(), Box<dyn std::error::Error>> {
        // Drops the memories table out from under an otherwise-valid store
        // so `load_memory`'s query fails with a real `rusqlite::Error`
        // other than `QueryReturnedNoRows`, exercising the `storage_error`
        // mapping on a genuine SQLite failure rather than an I/O failure.
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();
        let id = store.put("doomed", &[], &prov)?;

        store.conn_mut().execute_batch("DROP TABLE memories;")?;

        let err = expect_err!(store.get(id))?;
        assert!(matches!(err, MneneError::Storage(_)), "got {err:?}");
        Ok(())
    }

    #[test]
    fn open_on_a_directory_path_is_a_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        // Opening a path that is itself an existing directory fails inside
        // `rusqlite::Connection::open` with a genuine `rusqlite::Error`
        // (rather than the `std::io::Error` the parent-is-a-file case
        // above produces), exercising the `storage_error` mapping used at
        // that call site.
        let dir = tempfile::tempdir()?;

        let err = expect_err!(SqliteStore::open(dir.path()))?;
        assert!(matches!(err, MneneError::Storage(_)), "got {err:?}");
        Ok(())
    }

    #[test]
    fn schema_version_mismatch_is_reported() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("mnene.db");

        // Open once to bootstrap, then force a foreign user_version with a
        // raw connection before reopening through SqliteStore.
        let _ = SqliteStore::open(&db_path)?;
        let raw = rusqlite::Connection::open(&db_path)?;
        raw.pragma_update(None, "user_version", 7)?;
        drop(raw);

        let err = expect_err!(SqliteStore::open(&db_path))?;
        assert_eq!(
            MneneError::SchemaVersion {
                found: 7,
                expected: 2,
            },
            err
        );
        Ok(())
    }

    #[test]
    fn a_version_1_database_migrates_in_place_with_rows_intact_and_scope_source_null()
    -> Result<(), Box<dyn std::error::Error>> {
        // Builds a version-1 fixture by hand (the schema this build's
        // SCHEMA_SQL used before T013 added scope_source), writes a row
        // directly, then reopens it through SqliteStore and asserts the
        // migration's three invariants: user_version becomes 2, the row
        // survives untouched, and its scope_source is NULL (no row is
        // rescoped by the migration itself -- see D010).
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("v1-fixture.db");
        let raw = rusqlite::Connection::open(&db_path)?;
        let v1_schema_sql = "BEGIN IMMEDIATE;
             CREATE TABLE memories (
               id            TEXT PRIMARY KEY,
               body          TEXT NOT NULL,
               state         TEXT NOT NULL CHECK (state IN ('active','superseded','retracted')),
               supersedes    TEXT REFERENCES memories(id),
               superseded_by TEXT REFERENCES memories(id),
               scope         TEXT NOT NULL,
               task          TEXT,
               agent         TEXT NOT NULL,
               context       TEXT NOT NULL,
               session       TEXT,
               created_at    TEXT NOT NULL,
               closed_at     TEXT,
               closed_by     TEXT
             );
             CREATE TABLE tags (
               memory_id TEXT NOT NULL REFERENCES memories(id),
               tag       TEXT NOT NULL,
               PRIMARY KEY (memory_id, tag)
             );
             CREATE VIRTUAL TABLE memories_fts USING fts5 (id UNINDEXED, body, tags);
             CREATE INDEX memories_scope_state ON memories (scope, state, created_at);
             PRAGMA user_version = 1;
             COMMIT;";
        raw.execute_batch(v1_schema_sql)?;

        let fixture_id = MemoryId::new();
        let insert_memory_sql = "INSERT INTO memories \
             (id, body, state, supersedes, superseded_by, scope, task, agent, context, session, created_at, closed_at, closed_by) \
             VALUES (?1, 'pre-migration memory', 'active', NULL, NULL, 'mnene', 'legacy-task', 'agent-a', 'default', NULL, '2026-01-01T00:00:00Z', NULL, NULL)";
        let fixture_id_param = rusqlite::params![fixture_id.to_string()];
        raw.execute(insert_memory_sql, fixture_id_param)?;

        let insert_fts_sql =
            "INSERT INTO memories_fts (id, body, tags) VALUES (?1, 'pre-migration memory', '')";
        raw.execute(insert_fts_sql, rusqlite::params![fixture_id.to_string()])?;
        drop(raw);

        let mut store = SqliteStore::open(&db_path)?;

        let version: i64 = store
            .conn_mut()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(2, version);

        let memory = store.get(fixture_id)?;
        assert_eq!("pre-migration memory", memory.body);
        assert_eq!(State::Active, memory.state);
        assert_eq!("mnene", memory.provenance.scope);
        assert_eq!(None, memory.provenance.scope_source);
        assert_eq!(Some("legacy-task".to_string()), memory.provenance.task);

        Ok(())
    }

    #[test]
    fn reopening_an_existing_file_preserves_data_and_schema()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("mnene.db");
        let prov = provenance();

        let id = {
            let mut store = SqliteStore::open(&db_path)?;
            store.put("first", &[], &prov)?
        };

        let mut reopened = SqliteStore::open(&db_path)?;
        let memory = reopened.get(id)?;
        assert_eq!("first", memory.body);

        let version: i64 = reopened
            .conn_mut()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(2, version);

        let table_count_result: Result<i64, rusqlite::Error> = reopened.conn().query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories'",
            [],
            |row| row.get(0),
        );
        let table_count = table_count_result?;
        assert_eq!(1, table_count);
        Ok(())
    }

    #[test]
    fn open_creates_parent_directories() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let nested = dir.path().join("a").join("b").join("c");
        let db_path = nested.join("mnene.db");

        let _store = SqliteStore::open(&db_path)?;
        assert!(db_path.exists());
        Ok(())
    }

    #[test]
    fn put_writes_a_matching_fts_row() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut store = SqliteStore::open(&dir.path().join("mnene.db"))?;
        let prov = provenance();

        let tags = vec![Tag::new("alpha")?, Tag::new("beta")?];
        let id = store.put("searchable body", &tags, &prov)?;

        let fts_row_result: Result<(String, String), rusqlite::Error> = store.conn().query_row(
            "SELECT body, tags FROM memories_fts WHERE id = ?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        );
        let (fts_body, fts_tags) = fts_row_result?;
        assert_eq!("searchable body", fts_body);
        assert_eq!("alpha beta", fts_tags);
        Ok(())
    }

    #[test]
    fn open_on_unopenable_path_yields_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let file_path = dir.path().join("not-a-directory");
        std::fs::write(&file_path, b"blocking file")?;
        let db_path = file_path.join("nested").join("mnene.db");

        let err = expect_err!(SqliteStore::open(&db_path))?;
        assert!(matches!(err, MneneError::Storage(_)), "got {err:?}");
        Ok(())
    }

    #[test]
    fn open_on_empty_parent_component_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        // A relative path with no directory component (an empty parent)
        // exercises the branch that skips create_dir_all entirely.
        let dir = tempfile::tempdir()?;
        let original = std::env::current_dir()?;
        std::env::set_current_dir(dir.path())?;
        let result = SqliteStore::open(Path::new("mnene.db"));
        std::env::set_current_dir(original)?;
        result?;
        Ok(())
    }

    #[test]
    fn open_sets_a_five_second_busy_timeout() -> Result<(), Box<dyn std::error::Error>> {
        // Proves the busy timeout `SqliteStore::open` sets: `busy_timeout`
        // has no read-back accessor of its own, but `SQLite` exposes the
        // value the busy handler was configured with via
        // `PRAGMA busy_timeout`, in milliseconds. This exercises the new
        // `busy_timeout` call in `open` without needing genuine lock
        // contention; the multi-process race in tests/concurrency.rs is
        // what proves the *effect* (queuing instead of `SQLITE_BUSY`)
        // under real contention between separate `mnene` processes.
        let dir = tempfile::tempdir()?;
        let store = SqliteStore::open(&dir.path().join("mnene.db"))?;

        let millis: i64 = store
            .conn()
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
        assert_eq!(5000, millis);
        Ok(())
    }
}
