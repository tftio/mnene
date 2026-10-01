//! Multi-process concurrency test for `mnene`.
//!
//! These tests spawn several real `mnene` binaries as separate operating
//! system processes -- not threads inside this test binary -- racing to
//! `overwrite` or `retract` the same active memory id against the same
//! `SQLite` file. They exercise `SqliteStore::open`'s busy timeout
//! (`src/store/mod.rs`) and the compare-and-supersede transactions in
//! `src/store/mutate.rs` under genuine multi-process contention, which a
//! single-process, single-threaded test cannot reach: within one process
//! every `BEGIN IMMEDIATE` transaction is strictly ordered by whichever
//! thread calls `SqliteStore::overwrite`/`retract` first, but a real race
//! between operating system processes gives no such ordering guarantee.
//!
//! Every process is spawned with `std::process::Command::spawn` (not
//! `assert_cmd`'s synchronous `.assert()`, which would wait for each
//! process before starting the next) so that all racers are genuinely
//! running at once, and every child's stdout/stderr is captured only after
//! every child has already been spawned, mirroring `tests/cli.rs`'s
//! practice of controlling every `MNENE_*`/`CLANKER_*`/`XDG_DATA_HOME`/
//! `HOME` variable explicitly rather than calling `Command::env_clear`,
//! which would also drop `LLVM_PROFILE_FILE` and silently zero out this
//! binary's coverage under `cargo llvm-cov nextest`.
//!
//! Both tests assert only on outcome *shape* (exactly one winner, every
//! loser's typed error, the resulting store state) rather than on which
//! particular racer wins, so they are deterministic regardless of how the
//! operating system schedules the child processes.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use assert_cmd::cargo::cargo_bin;
use mnene::model::MemoryId;
use rusqlite::{Connection, TransactionBehavior};
use serde_json::Value;

/// Every environment variable `mnene` itself reads (mirrors
/// `tests/cli.rs`'s `CONTROLLED_VARS` and the `env` attributes on `Cli` in
/// `src/main.rs`). Every child process removes or sets each of these
/// explicitly so ambient values from the shell or test runner can never
/// leak into a race's outcome.
const CONTROLLED_VARS: [&str; 10] = [
    "MNENE_AGENT",
    "MNENE_CONTEXT",
    "MNENE_SESSION",
    "MNENE_SCOPE",
    "MNENE_TASK",
    "MNENE_DB",
    "CLANKER_SESSION_HARNESS",
    "CLANKER_SESSION_CONTEXT",
    "XDG_DATA_HOME",
    "HOME",
];

/// Number of `overwrite` processes racing in
/// [`eight_concurrent_overwrites_yield_exactly_one_winner`]; at least eight,
/// per the task.
const OVERWRITE_RACER_COUNT: usize = 8;

/// Number of `retract` processes racing in
/// [`mixed_retract_and_overwrite_race_yields_exactly_one_winner`].
const MIXED_RETRACT_COUNT: usize = 4;

/// Number of `overwrite` processes racing in
/// [`mixed_retract_and_overwrite_race_yields_exactly_one_winner`].
const MIXED_OVERWRITE_COUNT: usize = 4;

/// A shared environment for one race: a temporary directory holding the
/// `SQLite` database and a fake `HOME`, plus the scope and context every
/// racer in the test shares.
struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    home: PathBuf,
    scope: String,
    context: String,
}

impl Env {
    fn new(name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let db = dir.path().join("mnene.db");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home)?;
        Ok(Self {
            _dir: dir,
            db,
            home,
            scope: format!("{name}-scope"),
            context: format!("{name}-context"),
        })
    }

    /// A `Command` for the `mnene` binary with every variable in
    /// [`CONTROLLED_VARS`] removed, `MNENE_DB`/`MNENE_SCOPE`/
    /// `MNENE_CONTEXT`/`HOME` pinned to this environment, and `MNENE_AGENT`
    /// set to `agent`. `MNENE_SESSION` and `MNENE_TASK` are left removed;
    /// neither is required by `put`, `overwrite`, or `retract`.
    fn command(&self, agent: &str) -> Command {
        let bin = cargo_bin("mnene");
        let mut command = Command::new(bin);
        for var in CONTROLLED_VARS {
            command.env_remove(var);
        }
        command
            .env("MNENE_DB", &self.db)
            .env("MNENE_SCOPE", &self.scope)
            .env("MNENE_CONTEXT", &self.context)
            .env("MNENE_AGENT", agent)
            .env("HOME", &self.home)
            .current_dir(&self.home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// One racer's finished process: its exit code (`None` if it was killed by
/// a signal, which none of these commands ever send themselves) and its
/// captured stdout/stderr, decoded as UTF-8.
struct Finished {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Waits for every already-spawned child in `children` to finish, in the
/// order given. Because every child was spawned before this function was
/// called, waiting for them in sequence here does not serialize their
/// execution -- they are already racing each other by the time any `wait`
/// call happens.
fn wait_all(children: Vec<Child>) -> Result<Vec<Finished>, Box<dyn std::error::Error>> {
    let mut finished = Vec::with_capacity(children.len());
    for child in children {
        let output: Output = child.wait_with_output()?;
        finished.push(Finished {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout)?,
            stderr: String::from_utf8(output.stderr)?,
        });
    }
    Ok(finished)
}

/// Runs one `mnene` invocation to completion (used for the setup steps
/// that must happen before a race starts, where no concurrency is wanted)
/// and returns its captured output.
fn run_one(mut command: Command) -> Result<Finished, Box<dyn std::error::Error>> {
    let child = command.spawn()?;
    let mut all = wait_all(vec![child])?;
    all.pop()
        .ok_or_else(|| "expected one finished process".into())
}

/// Parses `text` as JSON and returns the string value of `field`.
fn json_string_field(text: &str, field: &str) -> Result<String, Box<dyn std::error::Error>> {
    let value: Value = serde_json::from_str(text)?;
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("expected string field {field:?} in {text}").into())
}

/// Puts one memory via a single, non-racing `mnene put --json` call and
/// returns its minted id.
fn seed_one_memory(env: &Env, body: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut command = env.command("seed-agent");
    command.arg("--json").arg("put").arg(body);
    let finished = run_one(command)?;
    if finished.code != Some(0) {
        return Err(format!(
            "seeding put failed: code={:?} stderr={}",
            finished.code, finished.stderr
        )
        .into());
    }
    json_string_field(&finished.stdout, "id")
}

/// Spawns `count` `overwrite <id> "<body_prefix>-<n>"` processes, one per
/// `MNENE_AGENT` in `0..count`, without waiting for any of them.
fn spawn_overwrite_racers(
    env: &Env,
    id: &str,
    body_prefix: &str,
    count: usize,
) -> Result<Vec<Child>, Box<dyn std::error::Error>> {
    let mut children = Vec::with_capacity(count);
    for index in 0..count {
        let mut command = env.command(&format!("overwrite-agent-{index}"));
        command
            .arg("--json")
            .arg("overwrite")
            .arg(id)
            .arg(format!("{body_prefix}-{index}"));
        children.push(command.spawn()?);
    }
    Ok(children)
}

/// Spawns `count` `retract <id>` processes, one per `MNENE_AGENT` in
/// `0..count`, without waiting for any of them.
fn spawn_retract_racers(
    env: &Env,
    id: &str,
    count: usize,
) -> Result<Vec<Child>, Box<dyn std::error::Error>> {
    let mut children = Vec::with_capacity(count);
    for index in 0..count {
        let mut command = env.command(&format!("retract-agent-{index}"));
        command.arg("--json").arg("retract").arg(id);
        children.push(command.spawn()?);
    }
    Ok(children)
}

/// Runs `get <id> --json` and returns the parsed JSON document.
fn get_json(env: &Env, id: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let mut command = env.command("checker-agent");
    command.arg("--json").arg("get").arg(id);
    let finished = run_one(command)?;
    if finished.code != Some(0) {
        return Err(format!(
            "get --json failed: code={:?} stderr={}",
            finished.code, finished.stderr
        )
        .into());
    }
    Ok(serde_json::from_str(&finished.stdout)?)
}

/// Runs `recall --json` and returns the parsed JSON array of memories.
fn recall_json(env: &Env) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    let mut command = env.command("checker-agent");
    command.arg("--json").arg("recall");
    let finished = run_one(command)?;
    if finished.code != Some(0) {
        return Err(format!(
            "recall --json failed: code={:?} stderr={}",
            finished.code, finished.stderr
        )
        .into());
    }
    let value: Value = serde_json::from_str(&finished.stdout)?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| "expected recall --json to return an array".into())
}

/// Splits `finished` racers into the ones that exited 0 and the ones that
/// exited 1, failing with a typed error if any racer exited with anything
/// else (including being killed, which none of these commands do to
/// themselves).
fn partition_by_exit_code(
    finished: Vec<Finished>,
) -> Result<(Vec<Finished>, Vec<Finished>), Box<dyn std::error::Error>> {
    let mut successes = Vec::new();
    let mut failures = Vec::new();
    for one in finished {
        match one.code {
            Some(0) => successes.push(one),
            Some(1) => failures.push(one),
            other => {
                return Err(format!(
                    "racer exited with unexpected code {other:?}, stderr={}",
                    one.stderr
                )
                .into());
            }
        }
    }
    Ok((successes, failures))
}

/// Race 1: at least eight `overwrite` processes against one active id.
/// Asserts exactly one succeeds with a parseable `UUIDv7` successor id,
/// every other exits 1 naming both the original id and the winner's
/// successor id, `get` on the original shows it superseded by that one
/// winner, and `recall` returns exactly that one active memory.
#[test]
fn eight_concurrent_overwrites_yield_exactly_one_winner() -> Result<(), Box<dyn std::error::Error>>
{
    let env = Env::new("overwrite-race")?;
    let original_id = seed_one_memory(&env, "original body")?;

    let children =
        spawn_overwrite_racers(&env, &original_id, "overwrite-body", OVERWRITE_RACER_COUNT)?;
    let finished = wait_all(children)?;
    let (successes, failures) = partition_by_exit_code(finished)?;

    assert_eq!(1, successes.len(), "expected exactly one overwrite to win");
    assert_eq!(OVERWRITE_RACER_COUNT - 1, failures.len());

    let winner_stdout = &successes
        .first()
        .ok_or("expected a successful racer")?
        .stdout;
    let winner_id = json_string_field(winner_stdout, "id")?;
    let parsed_winner: MemoryId = winner_id
        .parse()
        .map_err(|err| format!("winner id {winner_id:?} did not parse as a UUIDv7: {err}"))?;
    assert_eq!(winner_id, parsed_winner.to_string());

    // Every racer runs with `--json`, so a fatal domain error renders the
    // shared JSON error envelope on stdout (not stderr) per the shared
    // `tftio_lib::FatalCliError` contract.
    for failure in &failures {
        assert!(
            failure.stdout.contains(&original_id),
            "loser stdout {:?} did not name the original id {original_id}",
            failure.stdout
        );
        assert!(
            failure.stdout.contains(&winner_id),
            "loser stdout {:?} did not name the winner's successor id {winner_id}",
            failure.stdout
        );
    }

    let original_after = get_json(&env, &original_id)?;
    assert_eq!(
        Some("superseded"),
        original_after.get("state").and_then(Value::as_str)
    );
    assert_eq!(
        Some(winner_id.as_str()),
        original_after.get("superseded_by").and_then(Value::as_str)
    );

    let recalled = recall_json(&env)?;
    assert_eq!(1, recalled.len(), "expected recall to return one memory");
    let recalled_id = recalled
        .first()
        .and_then(|memory| memory.get("id"))
        .and_then(Value::as_str)
        .ok_or("expected the recalled memory to have an id")?;
    assert_eq!(winner_id, recalled_id);

    Ok(())
}

/// Race 2: a mix of `retract` and `overwrite` processes against one fresh
/// active id. Asserts exactly one process wins overall; if the winner is a
/// retraction, `get` shows `retracted` with a null `superseded_by` and
/// `recall` returns zero memories, otherwise the same overwrite assertions
/// as the first race hold.
#[test]
fn mixed_retract_and_overwrite_race_yields_exactly_one_winner()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new("mixed-race")?;
    let original_id = seed_one_memory(&env, "fresh body")?;

    let mut children = spawn_retract_racers(&env, &original_id, MIXED_RETRACT_COUNT)?;
    children.extend(spawn_overwrite_racers(
        &env,
        &original_id,
        "mixed-body",
        MIXED_OVERWRITE_COUNT,
    )?);
    let finished = wait_all(children)?;
    let (successes, failures) = partition_by_exit_code(finished)?;

    assert_eq!(1, successes.len(), "expected exactly one racer to win");
    assert_eq!(
        MIXED_RETRACT_COUNT + MIXED_OVERWRITE_COUNT - 1,
        failures.len(),
        "expected every other racer to have lost"
    );

    let winner = successes.first().ok_or("expected a successful racer")?;
    // `retract --json` prints {"id": ..., "state": "retracted"}; `overwrite
    // --json` prints {"id": ...} with no "state" field. That distinguishes
    // which verb actually won without tracking child process identities.
    let winner_state = serde_json::from_str::<Value>(&winner.stdout)?
        .get("state")
        .and_then(Value::as_str)
        .map(str::to_string);

    let original_after = get_json(&env, &original_id)?;

    if winner_state.as_deref() == Some("retracted") {
        assert_eq!(
            Some("retracted"),
            original_after.get("state").and_then(Value::as_str)
        );
        assert!(
            original_after
                .get("superseded_by")
                .is_some_and(Value::is_null),
            "expected a retracted memory to have a null superseded_by, got {original_after}"
        );

        let recalled = recall_json(&env)?;
        assert_eq!(
            0,
            recalled.len(),
            "expected recall to return no memories after a winning retract"
        );
    } else {
        let winner_id = json_string_field(&winner.stdout, "id")?;
        let parsed_winner: MemoryId = winner_id
            .parse()
            .map_err(|err| format!("winner id {winner_id:?} did not parse as a UUIDv7: {err}"))?;
        assert_eq!(winner_id, parsed_winner.to_string());

        // See the comment in the first race's assertions: `--json` racers
        // render a fatal domain error on stdout, not stderr.
        for failure in &failures {
            assert!(
                failure.stdout.contains(&original_id),
                "loser stdout {:?} did not name the original id {original_id}",
                failure.stdout
            );
        }

        assert_eq!(
            Some("superseded"),
            original_after.get("state").and_then(Value::as_str)
        );
        assert_eq!(
            Some(winner_id.as_str()),
            original_after.get("superseded_by").and_then(Value::as_str)
        );

        let recalled = recall_json(&env)?;
        assert_eq!(1, recalled.len(), "expected recall to return one memory");
        let recalled_id = recalled
            .first()
            .and_then(|memory| memory.get("id"))
            .and_then(Value::as_str)
            .ok_or("expected the recalled memory to have an id")?;
        assert_eq!(winner_id, recalled_id);
    }

    Ok(())
}

/// Proves `SqliteStore::open`'s busy timeout under real inter-process
/// contention: a raw `rusqlite::Connection` (standing in for another
/// `mnene` process's own open write transaction) holds `BEGIN IMMEDIATE`
/// on the database file while a `mnene put` process is spawned against the
/// same file. Without a busy timeout, that second process's own `BEGIN
/// IMMEDIATE` would fail at once with `SQLITE_BUSY`; with it, `SQLite`
/// retries internally until the raw connection commits, and the `put`
/// succeeds afterward.
#[test]
fn busy_timeout_lets_a_second_process_queue_instead_of_erroring()
-> Result<(), Box<dyn std::error::Error>> {
    let env = Env::new("busy-timeout")?;
    // Seed once through the binary first, so the database file is already
    // a bootstrapped `mnene` database (schema created, `user_version`
    // stamped) before the raw connection below takes out its lock.
    seed_one_memory(&env, "first body")?;

    let mut raw = Connection::open(&env.db)?;
    let held = raw.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let mut command = env.command("queued-agent");
    command.arg("--json").arg("put").arg("queued body");
    let child = command.spawn()?;

    std::thread::sleep(Duration::from_millis(200));
    held.commit()?;

    let finished = wait_all(vec![child])?
        .pop()
        .ok_or("expected one finished process")?;
    assert_eq!(
        Some(0),
        finished.code,
        "queued put failed: stderr={}",
        finished.stderr
    );

    let id = json_string_field(&finished.stdout, "id")?;
    let parsed: MemoryId = id
        .parse()
        .map_err(|err| format!("id {id:?} did not parse as a UUIDv7: {err}"))?;
    assert_eq!(id, parsed.to_string());

    Ok(())
}
