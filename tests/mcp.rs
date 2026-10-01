//! Stdio MCP handshake tests for `mnene mcp`.
//!
//! These tests spawn the built binary with piped stdin/stdout and drive a
//! JSON-RPC 2.0 conversation over the newline-delimited framing
//! `src/mcp.rs`'s `serve` implements, exercising `src/main.rs`'s `Mcp`
//! verb end to end. Every `MNENE_*` provenance variable the process reads
//! is either removed or set to an explicit value, mirroring
//! `tests/cli.rs`'s `CONTROLLED_VARS` pattern; `Command::env_clear` is
//! never used, so `LLVM_PROFILE_FILE` still reaches the child and its
//! coverage is still collected.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};

use serde_json::{Value, json};

/// Every environment variable `mnene` itself reads (mirrors
/// `tests/cli.rs`'s `CONTROLLED_VARS`). Every session removes all of
/// these first, then sets explicitly whichever ones the test needs.
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

/// A running `mnene mcp` child process with buffered handles for writing
/// JSON-RPC requests and reading JSON-RPC responses one line at a time.
struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    /// Spawns `mnene mcp` with `MNENE_DB` pointed at `db`, `HOME` (and the
    /// current directory) pointed at `home`, and every `(key, value)` in
    /// `extra_env` set in addition. Every other controlled variable is
    /// removed.
    fn spawn(
        db: &Path,
        home: &Path,
        extra_env: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let bin = assert_cmd::cargo::cargo_bin("mnene");
        let mut command = Command::new(bin);
        for var in CONTROLLED_VARS {
            command.env_remove(var);
        }
        command
            .env("MNENE_DB", db)
            .env("HOME", home)
            .current_dir(home)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or("spawned child had no stdin")?;
        let stdout = child.stdout.take().ok_or("spawned child had no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }

    /// Writes one JSON-RPC message as a single newline-terminated line and
    /// flushes it.
    fn send(&mut self, message: &Value) -> Result<(), Box<dyn std::error::Error>> {
        self.send_raw(&message.to_string())
    }

    /// Writes `line` verbatim, followed by a newline, and flushes it. Used
    /// to send input that is not valid JSON.
    fn send_raw(&mut self, line: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut owned = line.to_string();
        owned.push('\n');
        self.stdin.write_all(owned.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Reads and parses one JSON-RPC response line.
    fn recv(&mut self) -> Result<Value, Box<dyn std::error::Error>> {
        let mut line = String::new();
        let bytes_read = self.stdout.read_line(&mut line)?;
        if bytes_read == 0 {
            return Err("unexpected EOF waiting for an MCP response".into());
        }
        let value: Value = serde_json::from_str(line.trim_end())?;
        Ok(value)
    }

    /// Closes stdin, signalling EOF to the server loop, and waits for the
    /// child to exit, returning its exit status.
    fn finish(self) -> Result<ExitStatus, Box<dyn std::error::Error>> {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        Ok(child.wait()?)
    }
}

/// The standard set of provenance variables a full session sets, mirroring
/// `tests/cli.rs`'s `Env::command`.
const FULL_PROVENANCE: [(&str, &str); 5] = [
    ("MNENE_AGENT", "agent-a"),
    ("MNENE_CONTEXT", "test-context"),
    ("MNENE_SESSION", "session-a"),
    ("MNENE_SCOPE", "scope-a"),
    ("MNENE_TASK", "task-a"),
];

/// [`FULL_PROVENANCE`] with `MNENE_AGENT` left unset, used to exercise the
/// `MissingAgent` domain error on a write tool.
const PROVENANCE_WITHOUT_AGENT: [(&str, &str); 4] = [
    ("MNENE_CONTEXT", "test-context"),
    ("MNENE_SESSION", "session-a"),
    ("MNENE_SCOPE", "scope-a"),
    ("MNENE_TASK", "task-a"),
];

/// A temporary database path and a fake `HOME` directory, both backed by
/// one temporary directory kept alive for the test's duration.
struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    home: PathBuf,
}

impl Fixture {
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
}

/// Looks up `key` in a JSON object `value`, without
/// `clippy::indexing_slicing`: a missing field becomes a typed test
/// failure rather than a panic.
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

/// Extracts a tool result's `isError` flag.
fn is_error(tool_result: &Value) -> Result<bool, String> {
    tool_result
        .get("isError")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("expected an isError boolean in {tool_result}"))
}

/// Extracts a tool result's rendered text, from `content[0].text`.
fn tool_text(tool_result: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let content = field(tool_result, "content")?;
    let first = first_element(content)?;
    let text = first
        .get("text")
        .and_then(Value::as_str)
        .ok_or("expected content[0].text to be a string")?;
    Ok(text.to_string())
}

/// Sends `initialize` and asserts the result names the server `mnene` and
/// declares the `tools` capability.
fn assert_initialize(session: &mut Session) -> Result<(), Box<dyn std::error::Error>> {
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2025-06-18" },
    }))?;
    let initialize_response = session.recv()?;
    let initialize_result = field(&initialize_response, "result")?;
    let server_info = field(initialize_result, "serverInfo")?;
    let server_name = server_info
        .get("name")
        .and_then(Value::as_str)
        .ok_or("expected serverInfo.name to be a string")?;
    if server_name != "mnene" {
        return Err(format!("expected serverInfo.name \"mnene\", got {server_name:?}").into());
    }
    field(field(initialize_result, "capabilities")?, "tools")?;

    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
    }))?;
    Ok(())
}

/// Sends `tools/list` and asserts it returns exactly the six data verbs.
fn assert_tools_list(session: &mut Session) -> Result<(), Box<dyn std::error::Error>> {
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
    }))?;
    let tools_response = session.recv()?;
    let tools = field(field(&tools_response, "result")?, "tools")?;
    let tools_array = tools
        .as_array()
        .ok_or("expected tools/list result.tools to be an array")?;
    let mut tool_names = tools_array
        .iter()
        .map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("expected a tool name in {tool}"))
        })
        .collect::<Result<Vec<&str>, String>>()?;
    tool_names.sort_unstable();
    let expected_names = ["get", "overwrite", "put", "recall", "retract", "search"];
    if tool_names != expected_names {
        return Err(format!("expected tools {expected_names:?}, got {tool_names:?}").into());
    }
    Ok(())
}

/// Sends `tools/call put` with `body` and `tags`, asserting success, and
/// returns the minted id.
fn run_put(
    session: &mut Session,
    body: &str,
    tags: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "put",
            "arguments": { "body": body, "tags": tags },
        },
    }))?;
    let put_response = session.recv()?;
    let put_result = field(&put_response, "result")?;
    if is_error(put_result)? {
        return Err(format!("expected put to succeed, got {put_result}").into());
    }
    let put_text = tool_text(put_result)?;
    let put_body: Value = serde_json::from_str(&put_text)?;
    let id = put_body
        .get("id")
        .and_then(Value::as_str)
        .ok_or("expected put's result text to carry an id")?
        .to_string();
    Ok(id)
}

/// Sends `tools/call get` for `id`, asserting success and that the
/// returned memory matches `expected_body`, `expected_tags`, and
/// `expected_state`.
fn assert_get_matches(
    session: &mut Session,
    id: &str,
    expected_body: &str,
    expected_tags: &[&str],
    expected_state: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "get",
            "arguments": { "id": id },
        },
    }))?;
    let get_response = session.recv()?;
    let get_result = field(&get_response, "result")?;
    if is_error(get_result)? {
        return Err(format!("expected get to succeed, got {get_result}").into());
    }
    let get_text = tool_text(get_result)?;
    let memory: Value = serde_json::from_str(&get_text)?;

    let memory_id = memory
        .get("id")
        .and_then(Value::as_str)
        .ok_or("expected the fetched memory to carry an id")?;
    if memory_id != id {
        return Err(format!("expected id {id}, got {memory_id}").into());
    }
    let memory_body = memory
        .get("body")
        .and_then(Value::as_str)
        .ok_or("expected the fetched memory to carry a body")?;
    if memory_body != expected_body {
        return Err(format!("expected body {expected_body:?}, got {memory_body:?}").into());
    }
    let memory_tags = memory
        .get("tags")
        .and_then(Value::as_array)
        .ok_or("expected the fetched memory to carry tags")?;
    let tag_strings = memory_tags
        .iter()
        .map(|tag| {
            tag.as_str()
                .ok_or_else(|| format!("expected a string tag, got {tag}"))
        })
        .collect::<Result<Vec<&str>, String>>()?;
    if tag_strings != expected_tags {
        return Err(format!("expected tags {expected_tags:?}, got {tag_strings:?}").into());
    }
    let memory_state = memory
        .get("state")
        .and_then(Value::as_str)
        .ok_or("expected the fetched memory to carry a state")?;
    if memory_state != expected_state {
        return Err(format!("expected state {expected_state:?}, got {memory_state:?}").into());
    }
    Ok(())
}

/// Asserts `session` exits successfully once `finish` closes its stdin.
fn assert_exits_ok(session: Session) -> Result<(), Box<dyn std::error::Error>> {
    let status = session.finish()?;
    if !status.success() {
        return Err(format!("expected the process to exit 0 at EOF, got {status:?}").into());
    }
    Ok(())
}

#[test]
fn stdio_handshake_initializes_lists_tools_and_round_trips_a_memory()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut session = Session::spawn(&fixture.db, &fixture.home, &FULL_PROVENANCE)?;

    assert_initialize(&mut session)?;
    assert_tools_list(&mut session)?;
    let id = run_put(&mut session, "remember the launch code", &["ops"])?;
    assert_get_matches(
        &mut session,
        &id,
        "remember the launch code",
        &["ops"],
        "active",
    )?;

    assert_exits_ok(session)
}

#[test]
fn a_malformed_line_does_not_stop_the_loop_and_ping_still_answers()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut session = Session::spawn(&fixture.db, &fixture.home, &FULL_PROVENANCE)?;

    session.send_raw("{ this is not valid json")?;
    let malformed_response = session.recv()?;
    field(&malformed_response, "error")?;

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "ping",
    }))?;
    let ping_response = session.recv()?;
    field(&ping_response, "result")?;
    let ping_id = ping_response
        .get("id")
        .and_then(Value::as_i64)
        .ok_or("expected ping's response id to be an integer")?;
    if ping_id != 1 {
        return Err(format!("expected ping's response id to be 1, got {ping_id}").into());
    }

    assert_exits_ok(session)
}

#[test]
fn a_missing_agent_and_a_bogus_id_are_reported_as_tool_errors()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let mut session = Session::spawn(&fixture.db, &fixture.home, &PROVENANCE_WITHOUT_AGENT)?;

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "put",
            "arguments": { "body": "no agent set" },
        },
    }))?;
    let put_response = session.recv()?;
    let put_result = field(&put_response, "result")?;
    if !is_error(put_result)? {
        return Err(format!(
            "expected put without MNENE_AGENT to report isError: true, got {put_result}"
        )
        .into());
    }
    let put_text = tool_text(put_result)?;
    if !put_text.contains("agent") {
        return Err(format!(
            "expected the missing-agent message to name the agent, got {put_text:?}"
        )
        .into());
    }

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "get",
            "arguments": { "id": "not-a-real-id" },
        },
    }))?;
    let get_response = session.recv()?;
    let get_result = field(&get_response, "result")?;
    if !is_error(get_result)? {
        return Err(format!(
            "expected get of a bogus id to report isError: true, got {get_result}"
        )
        .into());
    }
    let get_text = tool_text(get_result)?;
    if !get_text.contains("invalid") {
        return Err(format!("expected an invalid-id message, got {get_text:?}").into());
    }

    assert_exits_ok(session)
}
