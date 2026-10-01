//! CLI integration tests for `mnene`.
//!
//! These tests drive the built binary with `assert_cmd`. Every one of
//! mnene's own env-backed inputs (see [`CONTROLLED_VARS`]) is either set to
//! an explicit value or removed with `env_remove`, so behavior never
//! depends on whatever happens to be set in the ambient environment. This
//! deliberately does *not* call `Command::env_clear()`: clearing the whole
//! environment would also drop `LLVM_PROFILE_FILE`, the variable
//! `cargo llvm-cov nextest` uses to collect coverage from the spawned
//! `mnene` child process, which would silently zero out this binary's
//! coverage. Covers every verb in text and JSON mode and every error path
//! reachable from the CLI.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;

/// Every environment variable `mnene` itself reads (mirrors the process
/// edge `process_snapshot` reads in `src/main.rs`). Every test removes or
/// sets each of these explicitly, so ambient values from the shell or test
/// runner can never leak into a test's behavior.
const CONTROLLED_VARS: [&str; 15] = [
    "MNENE_AGENT",
    "MNENE_CONTEXT",
    "MNENE_SESSION",
    "MNENE_SCOPE",
    "MNENE_TASK",
    "MNENE_DB",
    "CLANKER_SESSION_HARNESS",
    "CLANKER_SESSION_CONTEXT",
    "CLANKER_SESSION_ID",
    "CLANKER_SESSION_PROJECT",
    "XDG_DATA_HOME",
    "XDG_CONFIG_HOME",
    "HOME",
    "TFTIO_AGENT_TOKEN",
    "TFTIO_AGENT_TOKEN_EXPECTED",
];

/// Extracts a value from a `Result` expected to be `Ok`, without
/// `unwrap`/`expect`: an unexpected `Err` becomes a typed test failure.
macro_rules! expect_ok {
    ($result:expr) => {
        match $result {
            Ok(value) => Ok(value),
            Err(err) => Err(format!("expected Ok, got Err: {err}")),
        }
    };
}

/// A `Command` for the `mnene` binary with every variable in
/// [`CONTROLLED_VARS`] removed, and everything else (notably
/// `LLVM_PROFILE_FILE` and `PATH`) inherited from this test process. Every
/// caller must then set whichever of those variables the test needs.
fn base_command() -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("mnene")?;
    for var in CONTROLLED_VARS {
        command.env_remove(var);
    }
    Ok(command)
}

/// A full, isolated environment for one CLI invocation: a temporary
/// directory holding the `SQLite` database and a fake `HOME`, plus the
/// provenance variables every test sets explicitly.
struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("mnene.db");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home)?;
        Ok(Self {
            _dir: dir,
            db,
            home,
        })
    }

    /// A `Command` for the `mnene` binary with every mnene-relevant
    /// environment variable pinned: the standard provenance variables, and
    /// `MNENE_DB` pointed at this environment's temporary database.
    fn command(&self) -> Result<Command, Box<dyn std::error::Error>> {
        let mut command = base_command()?;
        command
            .env("MNENE_DB", &self.db)
            .env("MNENE_AGENT", "agent-a")
            .env("MNENE_CONTEXT", "test-context")
            .env("MNENE_SESSION", "session-a")
            .env("MNENE_SCOPE", "scope-a")
            .env("MNENE_TASK", "task-a")
            .env("HOME", &self.home)
            .current_dir(&self.home);
        Ok(command)
    }
}

/// Looks up `key` in a JSON object `value`, without `clippy::indexing_slicing`:
/// a missing field becomes a typed test failure rather than a panic.
fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .ok_or_else(|| format!("expected field {key:?} in {value}"))
}

/// Returns the first element of a JSON array `value`, without
/// `clippy::indexing_slicing`: an empty or non-array value becomes a typed
/// test failure rather than a panic.
fn first_element(value: &Value) -> Result<&Value, String> {
    value
        .as_array()
        .and_then(|array| array.first())
        .ok_or_else(|| format!("expected a non-empty JSON array, got {value}"))
}

/// Returns the length of a JSON array `value`, without
/// `clippy::indexing_slicing`: a non-array value becomes a typed test
/// failure rather than a panic.
fn array_len(value: &Value) -> Result<usize, String> {
    value
        .as_array()
        .map(Vec::len)
        .ok_or_else(|| format!("expected a JSON array, got {value}"))
}

/// Runs `put "<body>" [--tag T]...` in JSON mode and returns the minted id.
fn put(env: &Env, body: &str, tags: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let mut command = env.command()?;
    command.arg("--json").arg("put").arg(body);
    for tag in tags {
        command.arg("--tag").arg(tag);
    }
    let output = command.assert().success().get_output().stdout.clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or("put --json did not return an id field")?;
    Ok(id.to_string())
}

// --- put / get round trip ---

#[test]
fn put_then_get_round_trips_through_json() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "hello memory", &["alpha", "beta"])?;

    let output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&output)?;

    let id_field = field(&memory, "id")?;
    assert_eq!(id, expect_ok!(id_field.as_str().ok_or("no id"))?);
    assert_eq!("hello memory", *field(&memory, "body")?);
    assert_eq!("active", *field(&memory, "state")?);
    let provenance = field(&memory, "provenance")?;
    assert_eq!("scope-a", *field(provenance, "scope")?);
    assert_eq!("agent-a", *field(provenance, "agent")?);
    assert_eq!("test-context", *field(provenance, "context")?);
    assert_eq!("session-a", *field(provenance, "session")?);
    assert_eq!("task-a", *field(provenance, "task")?);
    assert_eq!(
        Value::Array(vec!["alpha".into(), "beta".into()]),
        *field(&memory, "tags")?
    );
    Ok(())
}

#[test]
fn put_prints_the_bare_id_in_text_mode() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let output = env
        .command()?
        .arg("put")
        .arg("text mode body")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output)?;
    let trimmed = text.trim();

    assert_eq!(36, trimmed.len());
    assert!(trimmed.contains('-'));
    Ok(())
}

#[test]
fn get_in_text_mode_prints_a_labeled_block_with_a_trailing_body()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "text body", &["a-tag"])?;

    env.command()?
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("id: {id}")))
        .stdout(predicate::str::contains("state: active"))
        .stdout(predicate::str::contains("scope: scope-a"))
        .stdout(predicate::str::contains("task: task-a"))
        .stdout(predicate::str::contains("agent: agent-a"))
        .stdout(predicate::str::contains("context: test-context"))
        .stdout(predicate::str::contains("session: session-a"))
        .stdout(predicate::str::contains("tags: a-tag"))
        .stdout(predicate::str::contains("text body"));

    Ok(())
}

#[test]
fn get_in_text_mode_shows_supersedes_and_superseded_by_when_set()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let old = put(&env, "predecessor body", &[])?;
    let new_output = env
        .command()?
        .arg("overwrite")
        .arg(&old)
        .arg("successor body")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let new_id = String::from_utf8(new_output)?.trim().to_string();

    env.command()?
        .arg("get")
        .arg(&new_id)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("supersedes: {old}")));

    env.command()?
        .arg("get")
        .arg(&old)
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("superseded_by: {new_id}")));

    Ok(())
}

#[test]
fn get_of_unknown_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let unknown = "018f1e2a-1234-7abc-8abc-0123456789ab";

    env.command()?
        .arg("get")
        .arg(unknown)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "error: memory {unknown} not found"
        )));

    Ok(())
}

#[test]
fn get_of_an_invalid_id_is_invalid_id() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("get")
        .arg("not-a-uuid")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "error: invalid memory id: not-a-uuid",
        ));

    Ok(())
}

// --- put errors ---

#[test]
fn put_with_a_blank_body_is_empty_body() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("put")
        .arg("   ")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "error: memory body must not be blank",
        ));

    Ok(())
}

#[test]
fn put_with_an_invalid_tag_is_invalid_tag() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("put")
        .arg("body")
        .arg("--tag")
        .arg("...")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error: invalid tag: ..."));

    Ok(())
}

// --- search ---

#[test]
fn search_finds_a_hit_and_renders_id_and_score_in_text_mode()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "distinctivesearchterm here", &[])?;

    env.command()?
        .arg("search")
        .arg("distinctivesearchterm")
        .assert()
        .success()
        .stdout(predicate::str::is_match(format!(r"^{id} \d+\.\d{{4}}\n$"))?);

    Ok(())
}

#[test]
fn search_json_returns_a_hit_list() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "jsonsearchterm here", &[])?;

    let output = env
        .command()?
        .arg("--json")
        .arg("search")
        .arg("jsonsearchterm")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let hits: Value = serde_json::from_slice(&output)?;
    assert_eq!(1, array_len(&hits)?);
    let first = first_element(&hits)?;
    assert_eq!(id, *field(first, "id")?);

    Ok(())
}

#[test]
fn search_with_no_hits_is_empty() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("search")
        .arg("nothingmatchesthiseverwordxyz")
        .assert()
        .success()
        .stdout("");

    Ok(())
}

#[test]
fn search_is_bounded_to_scope_and_all_scopes_widens() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let _in_scope = put(&env, "scopedsearchword here", &[])?;

    let mut other_scope = env.command()?;
    other_scope.env("MNENE_SCOPE", "scope-b");
    other_scope
        .arg("put")
        .arg("scopedsearchword elsewhere")
        .assert()
        .success();

    let scoped = env
        .command()?
        .arg("--json")
        .arg("search")
        .arg("scopedsearchword")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let scoped_hits: Value = serde_json::from_slice(&scoped)?;
    assert_eq!(1, array_len(&scoped_hits)?);

    let widened = env
        .command()?
        .arg("--json")
        .arg("search")
        .arg("--all-scopes")
        .arg("scopedsearchword")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let widened_hits: Value = serde_json::from_slice(&widened)?;
    assert_eq!(2, array_len(&widened_hits)?);

    Ok(())
}

#[test]
fn search_without_scope_or_all_scopes_is_missing_scope() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;
    let outside = dir.path().join("outside-any-repo");
    std::fs::create_dir_all(&outside)?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .current_dir(&outside);

    command
        .arg("search")
        .arg("anything")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error: scope is required"));

    Ok(())
}

// --- overwrite / retract ---

#[test]
fn overwrite_chain_prints_the_new_id_and_replaces_tags_with_tag()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let old = put(&env, "first version", &["old-tag"])?;

    let output = env
        .command()?
        .arg("overwrite")
        .arg(&old)
        .arg("second version")
        .arg("--tag")
        .arg("new-tag")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output)?;
    let new_id = text.trim().to_string();
    assert_eq!(36, new_id.len());
    assert_ne!(old, new_id);

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&new_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let successor: Value = serde_json::from_slice(&get_output)?;
    assert_eq!("second version", *field(&successor, "body")?);
    assert_eq!(
        Value::Array(vec!["new-tag".into()]),
        *field(&successor, "tags")?
    );
    assert_eq!(old, *field(&successor, "supersedes")?);

    let old_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&old)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let predecessor: Value = serde_json::from_slice(&old_output)?;
    assert_eq!("superseded", *field(&predecessor, "state")?);
    assert_eq!(new_id, *field(&predecessor, "superseded_by")?);

    Ok(())
}

#[test]
fn overwrite_without_tag_inherits_predecessor_tags() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let old = put(&env, "first version", &["inherited-tag"])?;

    let output = env
        .command()?
        .arg("--json")
        .arg("overwrite")
        .arg(&old)
        .arg("second version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let new_id = field(&value, "id")?
        .as_str()
        .ok_or("expected an id")?
        .to_string();

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&new_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let successor: Value = serde_json::from_slice(&get_output)?;
    assert_eq!(
        Value::Array(vec!["inherited-tag".into()]),
        *field(&successor, "tags")?
    );

    Ok(())
}

#[test]
fn overwrite_of_a_superseded_id_is_not_active_naming_the_successor()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let old = put(&env, "first version", &[])?;
    let new_id = env
        .command()?
        .arg("overwrite")
        .arg(&old)
        .arg("second version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let new_id = String::from_utf8(new_id)?.trim().to_string();

    env.command()?
        .arg("overwrite")
        .arg(&old)
        .arg("third version")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "is superseded and cannot be modified, superseded by {new_id}"
        )));

    Ok(())
}

#[test]
fn overwrite_of_an_unknown_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let unknown = "018f1e2a-1234-7abc-8abc-0123456789ab";

    env.command()?
        .arg("overwrite")
        .arg(unknown)
        .arg("body")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "error: memory {unknown} not found"
        )));

    Ok(())
}

#[test]
fn retract_then_get_shows_retracted() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "to retract", &[])?;

    env.command()?
        .arg("retract")
        .arg(&id)
        .assert()
        .success()
        .stdout(format!("retracted {id}\n"));

    let output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&output)?;
    assert_eq!("retracted", *field(&memory, "state")?);

    Ok(())
}

#[test]
fn retract_json_reports_id_and_state() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "to retract via json", &[])?;

    let output = env
        .command()?
        .arg("--json")
        .arg("retract")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    assert_eq!(id, *field(&value, "id")?);
    assert_eq!("retracted", *field(&value, "state")?);

    Ok(())
}

#[test]
fn retract_of_a_retracted_id_is_not_active_with_no_successor()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "to retract twice", &[])?;
    env.command()?.arg("retract").arg(&id).assert().success();

    env.command()?
        .arg("retract")
        .arg(&id)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "memory {id} is retracted and cannot be modified"
        )))
        .stderr(predicate::str::contains("superseded by").not());

    Ok(())
}

#[test]
fn retract_of_an_unknown_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let unknown = "018f1e2a-1234-7abc-8abc-0123456789ab";

    env.command()?
        .arg("retract")
        .arg(unknown)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(format!(
            "error: memory {unknown} not found"
        )));

    Ok(())
}

// --- recall ---

#[test]
fn recall_lists_active_memories_newest_first_in_text_mode() -> Result<(), Box<dyn std::error::Error>>
{
    let env = Env::new()?;
    let first = put(&env, "recall body one", &[])?;
    let second = put(&env, "recall body two", &[])?;

    let output = env
        .command()?
        .arg("recall")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output)?;

    let first_index = text
        .find(&first)
        .ok_or("expected the first id in recall output")?;
    let second_index = text
        .find(&second)
        .ok_or("expected the second id in recall output")?;
    assert!(second_index < first_index, "expected newest first");
    assert!(text.contains("---"));

    Ok(())
}

#[test]
fn recall_json_returns_a_memory_list() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "recall json body", &[])?;

    let output = env
        .command()?
        .arg("--json")
        .arg("recall")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memories: Value = serde_json::from_slice(&output)?;
    assert_eq!(1, array_len(&memories)?);
    let first = first_element(&memories)?;
    assert_eq!(id, *field(first, "id")?);

    Ok(())
}

#[test]
fn recall_all_scopes_widens_beyond_the_configured_scope() -> Result<(), Box<dyn std::error::Error>>
{
    let env = Env::new()?;
    put(&env, "in configured scope", &[])?;

    let mut other_scope = env.command()?;
    other_scope.env("MNENE_SCOPE", "scope-b");
    other_scope
        .arg("put")
        .arg("in another scope")
        .assert()
        .success();

    let scoped = env
        .command()?
        .arg("--json")
        .arg("recall")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let scoped_memories: Value = serde_json::from_slice(&scoped)?;
    assert_eq!(1, array_len(&scoped_memories)?);

    let widened = env
        .command()?
        .arg("--json")
        .arg("recall")
        .arg("--all-scopes")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let widened_memories: Value = serde_json::from_slice(&widened)?;
    assert_eq!(2, array_len(&widened_memories)?);

    Ok(())
}

#[test]
fn recall_without_explicit_task_does_not_filter_by_configured_task()
-> Result<(), Box<dyn std::error::Error>> {
    // MNENE_TASK is "task-a" for every put in this environment; recall with
    // no --task must still return memories recorded under a different task,
    // so a fresh session's inheritance is not silently narrowed to its own
    // task.
    let env = Env::new()?;
    let mut other_task = env.command()?;
    other_task.env("MNENE_TASK", "task-b");
    other_task
        .arg("put")
        .arg("under another task")
        .assert()
        .success();

    let output = env
        .command()?
        .arg("--json")
        .arg("recall")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memories: Value = serde_json::from_slice(&output)?;
    assert_eq!(1, array_len(&memories)?);

    Ok(())
}

#[test]
fn recall_with_explicit_task_filters_to_it() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let matching = put(&env, "matches the requested task", &[])?;

    let mut other_task = env.command()?;
    other_task.env("MNENE_TASK", "task-b");
    other_task
        .arg("put")
        .arg("does not match")
        .assert()
        .success();

    let output = env
        .command()?
        .arg("--json")
        .arg("recall")
        .arg("--task")
        .arg("task-a")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memories: Value = serde_json::from_slice(&output)?;
    assert_eq!(1, array_len(&memories)?);
    let first = first_element(&memories)?;
    assert_eq!(matching, *field(first, "id")?);

    Ok(())
}

#[test]
fn recall_without_scope_or_all_scopes_is_missing_scope() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;
    let outside = dir.path().join("outside-any-repo");
    std::fs::create_dir_all(&outside)?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .current_dir(&outside);

    command
        .arg("recall")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error: scope is required"));

    Ok(())
}

// --- MissingAgent ---

#[test]
fn put_without_an_agent_is_missing_agent() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_SCOPE", "scope-a")
        .env("HOME", &home)
        .current_dir(&home);

    command
        .arg("put")
        .arg("body")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error: agent is required"));

    Ok(())
}

// --- removed provenance flags (T002) ---

/// Each of the five provenance arguments clap used to accept as a public
/// flag (`--agent`, `--context`, `--session`, `--scope`, and a top-level
/// `--task`) must now be rejected as an unrecognized argument: provenance
/// is environment-only, so there is no flag left for any of them to bind
/// to. Every rejection is a clap usage error, exit status 2, before any
/// subcommand or store access runs.
#[test]
fn removed_provenance_flags_are_rejected_as_unrecognized_arguments()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    for flag in ["--agent", "--context", "--session", "--scope", "--task"] {
        env.command()?
            .arg(flag)
            .arg("forged")
            .arg("put")
            .arg("body")
            .assert()
            .failure()
            .code(2);
    }

    Ok(())
}

// --- CLANKER_* fallbacks are overridden by MNENE_* values ---

#[test]
fn mnene_agent_overrides_clanker_session_harness_at_the_cli()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let mut command = env.command()?;
    command.env("CLANKER_SESSION_HARNESS", "codex");

    let output = command
        .arg("--json")
        .arg("put")
        .arg("agent override body")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?
        .as_str()
        .ok_or("expected an id")?
        .to_string();

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    assert_eq!(
        "agent-a",
        *field(field(&memory, "provenance")?, "agent")?,
        "MNENE_AGENT must win over CLANKER_SESSION_HARNESS when both are set"
    );

    Ok(())
}

#[test]
fn mnene_context_overrides_clanker_session_context_at_the_cli()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let mut command = env.command()?;
    command.env("CLANKER_SESSION_CONTEXT", "clanker-ctx");

    let output = command
        .arg("--json")
        .arg("put")
        .arg("context override body")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?
        .as_str()
        .ok_or("expected an id")?
        .to_string();

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    assert_eq!(
        "test-context",
        *field(field(&memory, "provenance")?, "context")?,
        "MNENE_CONTEXT must win over CLANKER_SESSION_CONTEXT when both are set"
    );

    Ok(())
}

// --- --db ---

#[test]
fn db_flag_overrides_mnene_db() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let overridden = env.db.with_file_name("overridden.db");

    env.command()?
        .arg("--db")
        .arg(&overridden)
        .arg("put")
        .arg("stored under the overriding path")
        .assert()
        .success();

    assert!(overridden.exists(), "expected {overridden:?} to exist");
    assert!(
        !env.db.exists(),
        "MNENE_DB's path must not be used once --db is given"
    );

    Ok(())
}

// --- recall --task never rewrites stored provenance ---

#[test]
fn recall_task_filter_leaves_stored_task_provenance_unchanged()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let id = put(&env, "task provenance stays put", &[])?;

    env.command()?
        .arg("--json")
        .arg("recall")
        .arg("--task")
        .arg("task-a")
        .assert()
        .success();

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    assert_eq!(
        "task-a",
        *field(field(&memory, "provenance")?, "task")?,
        "running recall --task must not alter the memory's own stored task"
    );

    Ok(())
}

// --- SchemaVersion ---

#[test]
fn a_foreign_schema_version_is_reported() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    // Bootstrap the database, then force a foreign user_version directly.
    env.command()?
        .arg("put")
        .arg("bootstrap")
        .assert()
        .success();
    {
        let raw = rusqlite::Connection::open(&env.db)?;
        raw.pragma_update(None, "user_version", 7)?;
    }

    env.command()?
        .arg("get")
        .arg("018f1e2a-1234-7abc-8abc-0123456789ab")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "error: schema version 7 found, expected 2",
        ));

    Ok(())
}

// --- Storage ---

#[test]
fn a_db_path_whose_parent_is_a_regular_file_is_a_storage_error()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;
    let blocking_file = dir.path().join("not-a-directory");
    std::fs::write(&blocking_file, b"blocking")?;
    let db_path = blocking_file.join("mnene.db");

    let mut command = base_command()?;
    command
        .env("MNENE_DB", &db_path)
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .env("HOME", &home)
        .current_dir(&home);

    command
        .arg("get")
        .arg("018f1e2a-1234-7abc-8abc-0123456789ab")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("error: storage error:"));

    Ok(())
}

// --- CLANKER_* fallbacks ---

#[test]
fn agent_falls_back_to_clanker_session_harness() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let mut command = env.command()?;
    command
        .env_remove("MNENE_AGENT")
        .env("CLANKER_SESSION_HARNESS", "codex");

    let output = command
        .arg("--json")
        .arg("put")
        .arg("clanker agent body")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?
        .as_str()
        .ok_or("expected an id")?
        .to_string();

    let get_output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg(&id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    assert_eq!("codex", *field(field(&memory, "provenance")?, "agent")?);

    Ok(())
}

#[test]
fn clanker_session_context_changes_the_default_db_path_under_xdg_data_home()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let xdg = dir.path().join("xdg");
    std::fs::create_dir_all(&xdg)?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;

    let mut command = base_command()?;
    command
        .env("XDG_DATA_HOME", &xdg)
        .env("HOME", &home)
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .env("CLANKER_SESSION_CONTEXT", "clanker-ctx")
        .current_dir(&home);

    command
        .arg("put")
        .arg("body under clanker context")
        .assert()
        .success();

    let expected = xdg.join("mnene").join("clanker-ctx.db");
    assert!(expected.exists(), "expected {expected:?} to exist");

    Ok(())
}

// --- default db path ---

#[test]
fn default_db_path_is_under_xdg_data_home_when_set() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let xdg = dir.path().join("xdg");
    std::fs::create_dir_all(&xdg)?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;

    let mut command = base_command()?;
    command
        .env("XDG_DATA_HOME", &xdg)
        .env("HOME", &home)
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .env("MNENE_CONTEXT", "ctx")
        .current_dir(&home);

    command
        .arg("put")
        .arg("under xdg default")
        .assert()
        .success();

    let expected = xdg.join("mnene").join("ctx.db");
    assert!(expected.exists(), "expected {expected:?} to exist");

    Ok(())
}

#[test]
fn default_db_path_is_under_home_when_xdg_data_home_is_absent()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;

    let mut command = base_command()?;
    command
        .env("HOME", &home)
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .env("MNENE_CONTEXT", "ctx")
        .current_dir(&home);

    command
        .arg("put")
        .arg("under home default")
        .assert()
        .success();

    let expected = home.join(".local/share/mnene").join("ctx.db");
    assert!(expected.exists(), "expected {expected:?} to exist");

    Ok(())
}

// --- git discovery ---

/// Writes a minimal, hand-rolled `.git/HEAD` under `repo_dir`, exactly as
/// `src/config.rs`'s own git-discovery tests do: `mnene` never shells out
/// to `git`, so a real repository is not required, only the on-disk shape
/// `discover_git` reads by hand.
fn init_git_repo(repo_dir: &Path, branch: &str) -> Result<(), Box<dyn std::error::Error>> {
    let git_dir = repo_dir.join(".git");
    std::fs::create_dir_all(&git_dir)?;
    std::fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    Ok(())
}

#[test]
fn git_discovery_runs_even_when_scope_and_task_are_set_explicitly()
-> Result<(), Box<dyn std::error::Error>> {
    // `Config::resolve`'s `task` tier (`read_worktree_branch`) always runs,
    // unconditionally, before deciding whether `raw.task` already overrides
    // its result, so a real `.git` entry must resolve without error on
    // every invocation regardless of whether `MNENE_TASK` (or, since T013,
    // `MNENE_SCOPE`) ends up overriding it. Every other test in this file
    // runs outside any git repository, so this is the only place that
    // exercises a successful `project_root` -> `read_branch` chain through
    // the compiled binary (as opposed to `src/config.rs`'s own unit tests,
    // which exercise the library directly).
    let env = Env::new()?;
    let repo_dir = env.home.join("a-repo");
    init_git_repo(&repo_dir, "a-branch")?;

    env.command()?
        .current_dir(&repo_dir)
        .arg("--json")
        .arg("put")
        .arg("body under a discovered git repo")
        .assert()
        .success();

    Ok(())
}

// --- T013: scope resolves through tftio_lib::project ---

/// Writes an origin remote into `repo_dir`'s `.git/config`, on top of
/// [`init_git_repo`]'s `HEAD`.
fn init_git_repo_with_remote(
    repo_dir: &Path,
    branch: &str,
    origin: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    init_git_repo(repo_dir, branch)?;
    std::fs::write(
        repo_dir.join(".git").join("config"),
        format!("[remote \"origin\"]\n\turl = {origin}\n"),
    )?;
    Ok(())
}

/// Writes a project registry at `<registry_root>/tftio/projects.toml`, the
/// directory `tftio_lib::project::default_registry_dir` computes from
/// `XDG_CONFIG_HOME`.
fn write_registry(registry_root: &Path, contents: &str) -> Result<(), Box<dyn std::error::Error>> {
    let tftio_dir = registry_root.join("tftio");
    std::fs::create_dir_all(&tftio_dir)?;
    std::fs::write(tftio_dir.join("projects.toml"), contents)?;
    Ok(())
}

/// A `get --json` response's `provenance.scope` and `provenance.scope_source`
/// fields, as owned strings.
fn scope_and_source(memory: &Value) -> Result<(String, String), String> {
    let provenance = field(memory, "provenance")?;
    let scope = field(provenance, "scope")?
        .as_str()
        .ok_or("scope is not a string")?
        .to_string();
    let source = field(provenance, "scope_source")?
        .as_str()
        .ok_or("scope_source is not a string")?
        .to_string();
    Ok((scope, source))
}

#[test]
fn a_registered_origin_resolves_the_slug_with_source_remote()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    let repo_dir = dir.path().join("kb");
    std::fs::create_dir_all(&home)?;
    init_git_repo_with_remote(&repo_dir, "main", "git@github.com:tftio/kb.git")?;

    let registry_dir = dir.path().join("registry");
    write_registry(
        &registry_dir,
        "[project.kb]\nremotes = [\"github.com/tftio/kb\"]\n",
    )?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &registry_dir)
        .current_dir(&repo_dir);

    let output = command
        .arg("--json")
        .arg("put")
        .arg("body in a registered repo")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?.as_str().ok_or("expected an id")?;

    let mut get_command = base_command()?;
    get_command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &registry_dir)
        .current_dir(&repo_dir);
    let get_output = get_command
        .arg("--json")
        .arg("get")
        .arg(id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    let (scope, source) = expect_ok!(scope_and_source(&memory))?;
    assert_eq!("kb", scope);
    assert_eq!("remote", source);

    Ok(())
}

#[test]
fn an_unregistered_repository_with_no_remote_resolves_the_directory_name_with_source_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    let repo_dir = dir.path().join("unregistered-repo");
    std::fs::create_dir_all(&home)?;
    init_git_repo(&repo_dir, "main")?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .current_dir(&repo_dir);

    let output = command
        .arg("--json")
        .arg("put")
        .arg("body in an unregistered repo")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?.as_str().ok_or("expected an id")?;

    let mut get_command = base_command()?;
    get_command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("HOME", &home)
        .current_dir(&repo_dir);
    let get_output = get_command
        .arg("--json")
        .arg("get")
        .arg(id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    let (scope, source) = expect_ok!(scope_and_source(&memory))?;
    assert_eq!("unregistered-repo", scope);
    assert_eq!("directory", source);

    Ok(())
}

#[test]
fn clanker_session_project_resolves_in_an_unregistered_non_repository_directory_with_source_clanker()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;

    let mut command = base_command()?;
    command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .env("CLANKER_SESSION_PROJECT", "marker-project")
        .current_dir(&home);

    let output = command
        .arg("--json")
        .arg("put")
        .arg("body under a clanker session marker")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let id = field(&value, "id")?.as_str().ok_or("expected an id")?;

    let mut get_command = base_command()?;
    get_command
        .env("MNENE_DB", dir.path().join("mnene.db"))
        .env("HOME", &home)
        .current_dir(&home);
    let get_output = get_command
        .arg("--json")
        .arg("get")
        .arg(id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memory: Value = serde_json::from_slice(&get_output)?;
    let (scope, source) = expect_ok!(scope_and_source(&memory))?;
    assert_eq!("marker-project", scope);
    assert_eq!("clanker", source);

    Ok(())
}

#[test]
fn two_worktrees_of_one_registered_repository_share_recall()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home)?;
    let repo_dir = dir.path().join("project");
    let worktrees = repo_dir.join(".git").join("worktrees");
    let main_worktree = repo_dir.join("main");
    let feature_worktree = repo_dir.join("feature-x");

    let init_worktree = |worktree_dir: &Path, gitdir_dir: &Path, branch: &str| {
        std::fs::create_dir_all(worktree_dir)?;
        std::fs::create_dir_all(gitdir_dir)?;
        std::fs::write(
            worktree_dir.join(".git"),
            format!("gitdir: {}\n", gitdir_dir.display()),
        )?;
        std::fs::write(gitdir_dir.join("commondir"), "../..\n")?;
        std::fs::write(
            gitdir_dir.join("HEAD"),
            format!("ref: refs/heads/{branch}\n"),
        )
    };
    init_worktree(&main_worktree, &worktrees.join("main"), "main")?;
    init_worktree(&feature_worktree, &worktrees.join("feature-x"), "feature/x")?;
    std::fs::write(
        repo_dir.join(".git").join("config"),
        "[remote \"origin\"]\n\turl = git@github.com:tftio/project.git\n",
    )?;

    let registry_dir = dir.path().join("registry");
    write_registry(
        &registry_dir,
        "[project.project]\nremotes = [\"github.com/tftio/project\"]\n",
    )?;
    let db = dir.path().join("mnene.db");

    let mut put_command = base_command()?;
    put_command
        .env("MNENE_DB", &db)
        .env("MNENE_AGENT", "agent-a")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &registry_dir)
        .current_dir(&main_worktree);
    put_command
        .arg("--json")
        .arg("put")
        .arg("written from the main worktree")
        .assert()
        .success();

    let mut recall_command = base_command()?;
    recall_command
        .env("MNENE_DB", &db)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &registry_dir)
        .current_dir(&feature_worktree);
    let recall_output = recall_command
        .arg("--json")
        .arg("recall")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let memories: Value = serde_json::from_slice(&recall_output)?;
    assert_eq!(1, array_len(&memories)?);

    Ok(())
}

// --- T013: `scopes` ---

#[test]
fn scopes_lists_seeded_scopes_with_their_sources() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    put(&env, "in scope-a", &[])?;

    let mut other_scope = env.command()?;
    other_scope.env("MNENE_SCOPE", "scope-b");
    other_scope
        .arg("--json")
        .arg("put")
        .arg("in scope-b")
        .assert()
        .success();

    let output = env
        .command()?
        .arg("--json")
        .arg("scopes")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;
    let entries = value.as_array().ok_or("expected a JSON array")?;
    assert_eq!(2, entries.len());

    let scopes: Vec<&str> = entries
        .iter()
        .map(|entry| {
            field(entry, "scope").and_then(|s| s.as_str().ok_or_else(|| "not a string".to_string()))
        })
        .collect::<Result<Vec<&str>, String>>()?;
    assert_eq!(vec!["scope-a", "scope-b"], scopes);

    for entry in entries {
        assert_eq!(Value::from(1), *field(entry, "count")?);
        let sources = field(entry, "sources")?
            .as_array()
            .ok_or("expected a sources array")?;
        assert_eq!(&vec![Value::String("env".to_string())], sources);
    }

    let text_output = env
        .command()?
        .arg("scopes")
        .assert()
        .success()
        .stdout(predicate::str::contains("scope-a 1 [env]"))
        .stdout(predicate::str::contains("scope-b 1 [env]"));
    drop(text_output);

    Ok(())
}

#[test]
fn a_clap_usage_error_exits_2() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?.arg("not-a-verb").assert().failure().code(2);

    Ok(())
}

#[test]
fn version_flag_preserves_binary_name() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("--version")
        .assert()
        .success()
        .stdout(format!("mnene {}\n", env!("CARGO_PKG_VERSION")));

    Ok(())
}

#[test]
fn help_lists_every_verb() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("put"))
        .stdout(predicate::str::contains("get"))
        .stdout(predicate::str::contains("search"))
        .stdout(predicate::str::contains("overwrite"))
        .stdout(predicate::str::contains("retract"))
        .stdout(predicate::str::contains("recall"))
        .stdout(predicate::str::contains("mcp"))
        .stdout(predicate::str::contains("scopes"));

    Ok(())
}

/// Keeps the `Path` import used: several helpers above accept `&Path`
/// implicitly through `PathBuf` derefs, but this direct use documents the
/// intent and avoids an unused-import warning if that ever changes.
#[test]
fn env_struct_paths_are_absolute() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let db: &Path = &env.db;
    assert!(db.is_absolute());
    Ok(())
}

// --- shared `tftio-lib` metadata surface (T001) ---

#[test]
fn meta_version_prints_the_package_version_in_text_mode() -> Result<(), Box<dyn std::error::Error>>
{
    let env = Env::new()?;

    env.command()?
        .arg("meta")
        .arg("version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));

    Ok(())
}

#[test]
fn meta_version_json_reports_the_package_version() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    let output = env
        .command()?
        .arg("meta")
        .arg("version")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;

    assert_eq!(
        env!("CARGO_PKG_VERSION"),
        expect_ok!(field(&value, "version")?.as_str().ok_or("no version"))?
    );
    Ok(())
}

#[test]
fn meta_license_prints_the_mit_license_text() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("meta")
        .arg("license")
        .assert()
        .success()
        .stdout(predicate::str::contains("MIT"));

    Ok(())
}

#[test]
fn meta_completions_bash_prints_a_non_empty_script_naming_the_binary()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    let output = env
        .command()?
        .arg("meta")
        .arg("completions")
        .arg("bash")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let script = String::from_utf8(output)?;

    assert!(!script.trim().is_empty());
    assert!(script.contains("mnene"));
    Ok(())
}

#[test]
fn meta_doctor_text_reports_the_fts5_check_as_healthy() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    env.command()?
        .arg("meta")
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("fts5"))
        .stdout(predicate::str::contains("healthy"));

    assert!(
        !env.db.exists(),
        "meta doctor must not create the configured MNENE_DB path"
    );
    Ok(())
}

#[test]
fn meta_doctor_json_reports_the_fts5_check_passing() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    let output = env
        .command()?
        .arg("meta")
        .arg("doctor")
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;

    assert_eq!(Value::Bool(true), *field(&value, "ok")?);
    let checks = field(&value, "checks")?;
    let first_check = first_element(checks)?;
    assert_eq!(Value::Bool(true), *field(first_check, "passed")?);
    assert!(
        expect_ok!(
            field(first_check, "name")?
                .as_str()
                .ok_or("check name missing")
        )?
        .contains("fts5")
    );

    assert!(
        !env.db.exists(),
        "meta doctor --json must not create the configured MNENE_DB path"
    );
    Ok(())
}

#[test]
fn meta_doctor_does_not_create_an_explicit_db_path_override()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;
    let explicit_db = env.home.join("explicit-doctor-target.db");

    env.command()?
        .arg("--db")
        .arg(&explicit_db)
        .arg("meta")
        .arg("doctor")
        .assert()
        .success();

    assert!(
        !explicit_db.exists(),
        "meta doctor must not create an explicit --db override either"
    );
    Ok(())
}

#[test]
fn json_get_of_an_invalid_id_emits_a_parseable_shared_error_envelope_and_exits_1()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new()?;

    let output = env
        .command()?
        .arg("--json")
        .arg("get")
        .arg("not-a-uuid")
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&output)?;

    assert_eq!(Value::Bool(false), *field(&value, "ok")?);
    assert_eq!(Value::String("get".to_string()), *field(&value, "command")?);
    let error = field(&value, "error")?;
    assert!(
        expect_ok!(
            field(error, "message")?
                .as_str()
                .ok_or("error message missing")
        )?
        .contains("invalid memory id: not-a-uuid")
    );
    Ok(())
}
