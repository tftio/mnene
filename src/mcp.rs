//! The Model Context Protocol server loop for `mnene`.
//!
//! [`serve`](crate::mcp::serve) implements a synchronous JSON-RPC 2.0 loop
//! over a generic [`BufRead`](std::io::BufRead) input and
//! [`Write`](std::io::Write) output, so tests can drive it entirely in memory. It
//! opens a [`SqliteStore`](crate::store::SqliteStore) at `config.db` once,
//! then reads one JSON-RPC message per line until EOF, handling
//! `initialize`, `notifications/initialized`, `ping`, `tools/list`, and
//! `tools/call`, writing at most one JSON-RPC response line per input line
//! (nothing for a notification, a blank line, or EOF) and flushing after
//! each response.
//!
//! This is the only module in the library that imports `serde_json`, under
//! `INVARIANT-BYPASS(RS-009)` in `deny.toml`: the stdio MCP transport is
//! one of this project's two declared JSON compatibility boundaries.
//!
//! The six MCP tools (`put`, `get`, `search`, `overwrite`, `retract`,
//! `recall`) mirror the CLI verbs of the same names. A tool's domain
//! failure (a [`MneneError`](crate::model::MneneError)) is reported as a
//! normal JSON-RPC result carrying `isError: true` and the error's
//! `Display` text, never as a JSON-RPC error object; a protocol-level
//! failure (an unreadable message, an unknown method, an unknown tool, or
//! arguments that do not match a tool's schema) is reported as a
//! JSON-RPC error object instead.
//!
//! Provenance and scope validation for the `put`/`overwrite`/`retract`
//! tools and the scope-bounded `search`/`recall` tools go through the
//! shared [`crate::config::Config::provenance`] and [`crate::config::Config::require_scope`] helpers,
//! the same ones the CLI (`src/main.rs`) uses.

use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::model::{MemoryId, MneneError, Tag};
use crate::store::SqliteStore;
use crate::store::recall::RecallOptions;
use crate::store::search::SearchOptions;

/// JSON-RPC 2.0 "Parse error" code: the input line was not valid JSON.
const PARSE_ERROR: i64 = -32_700;
/// JSON-RPC 2.0 "Invalid Request" code: the message was not a well-formed
/// JSON-RPC request object.
const INVALID_REQUEST: i64 = -32_600;
/// JSON-RPC 2.0 "Method not found" code.
const METHOD_NOT_FOUND: i64 = -32_601;
/// JSON-RPC 2.0 "Invalid params" code, also used for an unknown tool name.
const INVALID_PARAMS: i64 = -32_602;

/// The MCP protocol version this server declares and accepts.
const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

/// Serves the six `mnene` data verbs as MCP tools over `input`/`output`
/// until `input` reaches EOF.
///
/// Opens a [`SqliteStore`] at `config.db`, then reads `input` line by
/// line, treating each non-blank line as one JSON-RPC 2.0 message (the
/// stdio MCP transport's newline-delimited framing). A malformed line
/// produces a JSON-RPC error response rather than stopping the loop. Each
/// response is written as one JSON line to `output`, followed by a flush.
///
/// # Errors
///
/// Returns [`MneneError::Storage`] wrapping the underlying text when
/// `config.db` cannot be opened, when a line cannot be read from `input`
/// (for example invalid UTF-8), or when a response cannot be written to or
/// flushed through `output`.
pub fn serve<R: BufRead, W: Write>(
    config: &Config,
    input: R,
    mut output: W,
) -> Result<(), MneneError> {
    let mut store = SqliteStore::open(&config.db)?;

    for line in input.lines() {
        let line = line.map_err(|err| MneneError::Storage(format!("stdio: {err}")))?;
        let Some(response) = handle_line(&line, &mut store, config) else {
            continue;
        };

        // `RpcResponse`'s fields are plain strings, an i64, and `Value`
        // (which itself always serializes: no map here carries a
        // non-string key, and a non-finite `f64` renders as `null` rather
        // than erroring), so this never actually falls back to the
        // default; `unwrap_or_default` avoids `.expect()` without adding
        // a closure body that would be uncoverable dead code.
        let mut rendered = serde_json::to_string(&response).unwrap_or_default();
        rendered.push('\n');
        output
            .write_all(rendered.as_bytes())
            .map_err(|err| MneneError::Storage(format!("stdio: {err}")))?;
        output
            .flush()
            .map_err(|err| MneneError::Storage(format!("stdio: {err}")))?;
    }

    Ok(())
}

/// One JSON-RPC 2.0 response, either a success or an error.
#[derive(Debug, serde::Serialize)]
struct RpcResponse {
    /// Always `"2.0"`.
    jsonrpc: &'static str,
    /// The request id this responds to, or `Value::Null` when it could
    /// not be recovered.
    id: Value,
    /// The success payload, present exactly when `error` is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    /// The error payload, present exactly when `result` is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcErrorBody>,
}

/// The body of a JSON-RPC 2.0 error object.
#[derive(Debug, serde::Serialize)]
struct RpcErrorBody {
    /// The JSON-RPC error code.
    code: i64,
    /// A short human-readable message.
    message: String,
}

/// Builds a success [`RpcResponse`] carrying `result`.
const fn success_response(id: Value, result: Value) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

/// Builds an error [`RpcResponse`] carrying `code` and `message`.
const fn error_response(id: Value, code: i64, message: String) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcErrorBody { code, message }),
    }
}

/// A protocol-level failure: the request was well-formed JSON-RPC but its
/// method was unknown, or its params did not match what the method (or,
/// for `tools/call`, the named tool) expects.
///
/// This is distinct from a [`MneneError`], which is a domain failure and
/// is reported inside a successful JSON-RPC result instead (see the
/// module documentation).
enum ProtocolError {
    /// The `method` named in the request has no handler.
    MethodNotFound,
    /// The request's `params`, or a tool's `arguments`, did not
    /// deserialize into what was expected. Carries a short detail string.
    InvalidParams(String),
}

impl ProtocolError {
    /// The JSON-RPC error code for this failure.
    const fn code(&self) -> i64 {
        match self {
            Self::MethodNotFound => METHOD_NOT_FOUND,
            Self::InvalidParams(_) => INVALID_PARAMS,
        }
    }

    /// The JSON-RPC error message for this failure.
    fn message(&self) -> String {
        match self {
            Self::MethodNotFound => "method not found".to_string(),
            Self::InvalidParams(detail) => format!("invalid params: {detail}"),
        }
    }
}

/// Parses one input line into a [`RpcResponse`], or `None` when the line
/// is blank or is a JSON-RPC notification (no `id` member), which never
/// gets a response.
///
/// This is deliberately forgiving about what it parses: rather than
/// deserializing straight into a strict request struct (which would
/// reject a message before an `id` could be recovered from it), it parses
/// into a [`Value`] first and reads `id`, `jsonrpc`, `method`, and
/// `params` out of it by hand, exactly as the module documentation
/// describes.
fn handle_line(line: &str, store: &mut SqliteStore, config: &Config) -> Option<RpcResponse> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parsed: Result<Value, serde_json::Error> = serde_json::from_str(trimmed);
    let Ok(value) = parsed else {
        return Some(error_response(
            Value::Null,
            PARSE_ERROR,
            "parse error: invalid JSON".to_string(),
        ));
    };

    let Some(object) = value.as_object() else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected a JSON object".to_string(),
        ));
    };

    let id_present = object.contains_key("id");
    let id = object.get("id").cloned().unwrap_or(Value::Null);

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return if id_present {
            Some(error_response(
                id,
                INVALID_REQUEST,
                "invalid request: missing or unsupported jsonrpc version".to_string(),
            ))
        } else {
            None
        };
    }

    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return if id_present {
            Some(error_response(
                id,
                INVALID_REQUEST,
                "invalid request: missing method".to_string(),
            ))
        } else {
            None
        };
    };

    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let outcome = handle_request(method, &params, store, config);

    if !id_present {
        return None;
    }

    Some(match outcome {
        Ok(result) => success_response(id, result),
        Err(err) => error_response(id, err.code(), err.message()),
    })
}

/// Dispatches one JSON-RPC method to its handler.
///
/// # Errors
///
/// Returns [`ProtocolError::MethodNotFound`] when `method` is not one of
/// `initialize`, `notifications/initialized`, `ping`, `tools/list`, or
/// `tools/call`, and [`ProtocolError::InvalidParams`] when `tools/call`'s
/// params or a tool's arguments do not match what is expected.
fn handle_request(
    method: &str,
    params: &Value,
    store: &mut SqliteStore,
    config: &Config,
) -> Result<Value, ProtocolError> {
    match method {
        "initialize" => Ok(handle_initialize(params)),
        "notifications/initialized" => Ok(Value::Null),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => handle_tools_call(params, store, config),
        _ => Err(ProtocolError::MethodNotFound),
    }
}

/// Builds the `initialize` result: the accepted protocol version, the
/// `tools` capability, and server identification.
fn handle_initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let protocol_version = match requested {
        Some(version) if version == SUPPORTED_PROTOCOL_VERSION => version,
        _ => SUPPORTED_PROTOCOL_VERSION,
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "mnene",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// The `tools/list` result: the six tools' names, descriptions, and input
/// schemas, mirroring the CLI's arguments for each verb.
fn tool_definitions() -> Value {
    json!([
        put_tool_definition(),
        get_tool_definition(),
        search_tool_definition(),
        overwrite_tool_definition(),
        retract_tool_definition(),
        recall_tool_definition(),
    ])
}

/// The `put` tool's definition, mirroring `mnene put BODY [--tag T]...`.
fn put_tool_definition() -> Value {
    json!({
        "name": "put",
        "description": "Store a new memory and return its id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "body": {
                    "type": "string",
                    "description": "The memory text to store; must be non-blank."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "A tag to attach; may be repeated."
                }
            },
            "required": ["body"]
        }
    })
}

/// The `get` tool's definition, mirroring `mnene get ID`.
fn get_tool_definition() -> Value {
    json!({
        "name": "get",
        "description": "Retrieve a memory by id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The memory id to fetch." }
            },
            "required": ["id"]
        }
    })
}

/// The `search` tool's definition, mirroring `mnene search QUERY ...`.
fn search_tool_definition() -> Value {
    json!({
        "name": "search",
        "description": "Search memories in scope by keyword.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query text." },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of hits to return."
                },
                "include_superseded": {
                    "type": "boolean",
                    "description": "Include superseded memories in results."
                },
                "all_scopes": {
                    "type": "boolean",
                    "description": "Search every scope instead of only the current one."
                },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Require this tag; may be repeated."
                }
            },
            "required": ["query"]
        }
    })
}

/// The `overwrite` tool's definition, mirroring `mnene overwrite ID BODY
/// [--tag T]...`.
fn overwrite_tool_definition() -> Value {
    json!({
        "name": "overwrite",
        "description": "Supersede an active memory with a new body.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the memory to supersede; must be active."
                },
                "body": { "type": "string", "description": "The new memory text." },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "A tag for the successor; replaces the predecessor's tags when given."
                }
            },
            "required": ["id", "body"]
        }
    })
}

/// The `retract` tool's definition, mirroring `mnene retract ID`.
fn retract_tool_definition() -> Value {
    json!({
        "name": "retract",
        "description": "Retire an active memory with no replacement.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "The id of the memory to retract; must be active."
                }
            },
            "required": ["id"]
        }
    })
}

/// The `recall` tool's definition, mirroring `mnene recall ...`.
fn recall_tool_definition() -> Value {
    json!({
        "name": "recall",
        "description": "List active memories in scope, newest first.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of memories to return."
                },
                "task": {
                    "type": "string",
                    "description": "Restrict to memories recorded under this task."
                },
                "all_scopes": {
                    "type": "boolean",
                    "description": "Recall across every scope instead of only the current one."
                }
            },
            "required": []
        }
    })
}

/// Handles `tools/call`: reads `name` and `arguments` out of `params` and
/// dispatches to the named tool.
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidParams`] when `params.name` is missing
/// or not a string, or when `name` is not a known tool.
fn handle_tools_call(
    params: &Value,
    store: &mut SqliteStore,
    config: &Config,
) -> Result<Value, ProtocolError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ProtocolError::InvalidParams("missing tool name".to_string()))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    dispatch_tool(name, &arguments, store, config)
}

/// Deserializes `arguments` into `T`, mapping a shape mismatch to
/// [`ProtocolError::InvalidParams`].
fn parse_args<T: serde::de::DeserializeOwned>(arguments: &Value) -> Result<T, ProtocolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|err| ProtocolError::InvalidParams(err.to_string()))
}

/// Renders a domain outcome as an MCP tool result: `isError: false` and
/// the success value's JSON text on [`Ok`], `isError: true` and the
/// error's `Display` text on [`Err`]. Either way this is a JSON-RPC
/// success result, per the module documentation.
fn tool_result(outcome: Result<Value, MneneError>) -> Value {
    match outcome {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{ "type": "text", "text": err.to_string() }],
            "isError": true,
        }),
    }
}

/// Converts a `Serialize` domain value to JSON, matching the CLI's
/// `--json` output for the same value. Domain types here (`Memory`,
/// `Vec<Memory>`, `SearchHit`, `Vec<SearchHit>`) never fail to serialize:
/// there is no map with non-string keys and no non-finite float (`bm25`
/// scores are always finite), so `unwrap_or(Value::Null)` never actually
/// takes its fallback; it is used instead of `.expect()` because RS-003
/// denies `expect_used`.
fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Resolves the scope a `search` or `recall` call is bounded to:
/// `Ok(None)` (every scope) when `all_scopes` is set, else
/// [`crate::config::Config::require_scope`].
fn resolve_scope(config: &Config, all_scopes: bool) -> Result<Option<String>, MneneError> {
    if all_scopes {
        return Ok(None);
    }
    Ok(Some(config.require_scope()?.to_string()))
}

/// Normalizes and validates a list of raw tag strings.
///
/// # Errors
///
/// Returns [`MneneError::InvalidTag`] naming the first tag that fails to
/// normalize to a non-empty value.
fn parse_tags(raw: &[String]) -> Result<Vec<Tag>, MneneError> {
    raw.iter().map(|tag| Tag::new(tag)).collect()
}

/// Dispatches one `tools/call` by name, deserializing its arguments and
/// rendering the domain outcome via [`tool_result`].
///
/// # Errors
///
/// Returns [`ProtocolError::InvalidParams`] when `name` is not one of the
/// six known tools, or when `arguments` does not match that tool's
/// schema.
fn dispatch_tool(
    name: &str,
    arguments: &Value,
    store: &mut SqliteStore,
    config: &Config,
) -> Result<Value, ProtocolError> {
    match name {
        "put" => {
            let args: PutArgs = parse_args(arguments)?;
            Ok(tool_result(call_put(store, config, &args)))
        }
        "get" => {
            let args: GetArgs = parse_args(arguments)?;
            Ok(tool_result(call_get(store, &args)))
        }
        "search" => {
            let args: SearchArgs = parse_args(arguments)?;
            Ok(tool_result(call_search(store, config, &args)))
        }
        "overwrite" => {
            let args: OverwriteArgs = parse_args(arguments)?;
            Ok(tool_result(call_overwrite(store, config, &args)))
        }
        "retract" => {
            let args: RetractArgs = parse_args(arguments)?;
            Ok(tool_result(call_retract(store, config, &args)))
        }
        "recall" => {
            let args: RecallArgs = parse_args(arguments)?;
            Ok(tool_result(call_recall(store, config, &args)))
        }
        other => Err(ProtocolError::InvalidParams(format!(
            "unknown tool: {other}"
        ))),
    }
}

/// Arguments for the `put` tool, mirroring `mnene put BODY [--tag T]...`.
#[derive(Debug, Deserialize)]
struct PutArgs {
    /// The memory text to store.
    body: String,
    /// Tags to attach.
    #[serde(default)]
    tags: Vec<String>,
}

/// Arguments for the `get` tool, mirroring `mnene get ID`.
#[derive(Debug, Deserialize)]
struct GetArgs {
    /// The memory id to fetch.
    id: String,
}

/// Arguments for the `search` tool, mirroring `mnene search QUERY ...`.
#[derive(Debug, Deserialize)]
struct SearchArgs {
    /// The search query text.
    query: String,
    /// Maximum number of hits to return.
    #[serde(default = "default_search_limit")]
    limit: u32,
    /// Include superseded memories in results.
    #[serde(default)]
    include_superseded: bool,
    /// Search every scope instead of only the current one.
    #[serde(default)]
    all_scopes: bool,
    /// Tags every hit must carry.
    #[serde(default)]
    tags: Vec<String>,
}

/// The CLI's default `search` limit, mirrored here for the `limit` field's
/// serde default.
const fn default_search_limit() -> u32 {
    10
}

/// Arguments for the `overwrite` tool, mirroring `mnene overwrite ID BODY
/// [--tag T]...`.
#[derive(Debug, Deserialize)]
struct OverwriteArgs {
    /// The id of the memory to supersede.
    id: String,
    /// The new memory text.
    body: String,
    /// Tags for the successor.
    #[serde(default)]
    tags: Vec<String>,
}

/// Arguments for the `retract` tool, mirroring `mnene retract ID`.
#[derive(Debug, Deserialize)]
struct RetractArgs {
    /// The id of the memory to retract.
    id: String,
}

/// Arguments for the `recall` tool, mirroring `mnene recall ...`.
#[derive(Debug, Deserialize)]
struct RecallArgs {
    /// Maximum number of memories to return.
    #[serde(default = "default_recall_limit")]
    limit: u32,
    /// Restrict to memories recorded under this task.
    #[serde(default)]
    task: Option<String>,
    /// Recall across every scope instead of only the current one.
    #[serde(default)]
    all_scopes: bool,
}

/// The CLI's default `recall` limit, mirrored here for the `limit` field's
/// serde default.
const fn default_recall_limit() -> u32 {
    20
}

/// Runs `put`, returning `{"id": <new id>}` on success.
fn call_put(store: &mut SqliteStore, config: &Config, args: &PutArgs) -> Result<Value, MneneError> {
    let tags = parse_tags(&args.tags)?;
    let provenance = config.provenance()?;
    let id = store.put(&args.body, &tags, &provenance)?;
    Ok(json!({ "id": id.to_string() }))
}

/// Runs `get`, returning the full [`crate::model::Memory`] on success.
fn call_get(store: &SqliteStore, args: &GetArgs) -> Result<Value, MneneError> {
    let id: MemoryId = args.id.parse()?;
    let memory = store.get(id)?;
    Ok(to_json(&memory))
}

/// Runs `search`, returning the list of [`crate::model::SearchHit`] on
/// success.
fn call_search(
    store: &SqliteStore,
    config: &Config,
    args: &SearchArgs,
) -> Result<Value, MneneError> {
    let scope = resolve_scope(config, args.all_scopes)?;
    let required_tags = parse_tags(&args.tags)?;
    let options = SearchOptions {
        limit: args.limit,
        include_superseded: args.include_superseded,
        scope,
        required_tags,
    };
    let hits = store.search(&args.query, &options)?;
    Ok(to_json(&hits))
}

/// Runs `overwrite`, returning `{"id": <new id>}` on success.
fn call_overwrite(
    store: &mut SqliteStore,
    config: &Config,
    args: &OverwriteArgs,
) -> Result<Value, MneneError> {
    let id: MemoryId = args.id.parse()?;
    let tags = parse_tags(&args.tags)?;
    let provenance = config.provenance()?;
    let new_id = store.overwrite(id, &args.body, &tags, &provenance)?;
    Ok(json!({ "id": new_id.to_string() }))
}

/// Runs `retract`, returning `{"id": <id>, "state": "retracted"}` on
/// success.
fn call_retract(
    store: &mut SqliteStore,
    config: &Config,
    args: &RetractArgs,
) -> Result<Value, MneneError> {
    let id: MemoryId = args.id.parse()?;
    let provenance = config.provenance()?;
    store.retract(id, &provenance)?;
    Ok(json!({ "id": id.to_string(), "state": "retracted" }))
}

/// Runs `recall`, returning the list of [`crate::model::Memory`] on
/// success.
fn call_recall(
    store: &SqliteStore,
    config: &Config,
    args: &RecallArgs,
) -> Result<Value, MneneError> {
    let scope = resolve_scope(config, args.all_scopes)?;
    let options = RecallOptions {
        limit: args.limit,
        scope,
        task: args.task.clone(),
    };
    let memories = store.recall(&options)?;
    Ok(to_json(&memories))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use serde_json::{Value, json};

    use super::serve;
    use crate::config::Config;

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

    /// A small accessor over `serde_json::Value` that reaches into objects
    /// and arrays via the panic-free `Value::get`, so test assertions never
    /// use the panicking `[]` operator (RS-003 denies unchecked indexing,
    /// including in tests).
    trait ValueExt {
        /// The value at object key `key`, or `Value::Null` when `self` is
        /// not an object or has no such key.
        fn field(&self, key: &str) -> &Value;
        /// The value at array index `index`, or `Value::Null` when `self`
        /// is not an array or has no such index.
        fn at(&self, index: usize) -> &Value;
    }

    impl ValueExt for Value {
        fn field(&self, key: &str) -> &Value {
            const NULL: Value = Value::Null;
            self.get(key).unwrap_or(&NULL)
        }

        fn at(&self, index: usize) -> &Value {
            const NULL: Value = Value::Null;
            self.get(index).unwrap_or(&NULL)
        }
    }

    /// Builds a `Config` pointed at a fresh database under `dir`, with the
    /// given agent and scope.
    fn config(dir: &std::path::Path, agent: Option<&str>, scope: Option<&str>) -> Config {
        Config {
            agent: agent.map(str::to_string),
            context: "default".to_string(),
            session: Some("session-1".to_string()),
            scope: scope.map(str::to_string),
            scope_source: scope.map(|_| crate::model::ScopeSource::Env),
            task: None,
            db: dir.join("mnene.db"),
        }
    }

    /// Runs `serve` over `lines` joined with newlines, returning the
    /// parsed JSON value of every response line written to output.
    ///
    /// `serve` never writes a blank line (each response line is one
    /// compact JSON object), so every line `str::lines` yields here is
    /// non-blank; this does not re-check that.
    fn run(config: &Config, lines: &[&str]) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        let mut input_text = lines.join("\n");
        input_text.push('\n');
        let input = Cursor::new(input_text.into_bytes());
        let (writer, buffer) = TestWriter::new();

        serve(config, input, writer)?;

        let text = String::from_utf8(buffer.borrow().clone())?;
        let mut responses = Vec::new();
        for line in text.lines() {
            responses.push(serde_json::from_str(line)?);
        }
        Ok(responses)
    }

    /// Runs `serve` over a single request line.
    fn run_one(config: &Config, line: &str) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
        run(config, &[line])
    }

    /// Returns a slice's only element without indexing; an unexpected
    /// length becomes a typed test failure rather than a panic.
    fn expect_one<T>(items: &[T]) -> Result<&T, String> {
        match items {
            [only] => Ok(only),
            other => Err(format!("expected exactly 1 item, got {}", other.len())),
        }
    }

    /// Returns a slice's exactly two elements without indexing; an
    /// unexpected length becomes a typed test failure rather than a panic.
    fn expect_pair<T>(items: &[T]) -> Result<(&T, &T), String> {
        match items {
            [first, second] => Ok((first, second)),
            other => Err(format!("expected exactly 2 items, got {}", other.len())),
        }
    }

    /// Extracts a JSON string field's text, without indexing; a missing or
    /// non-string field becomes a typed test failure.
    fn expect_str<'a>(value: &'a Value, description: &str) -> Result<&'a str, String> {
        value
            .as_str()
            .ok_or_else(|| format!("expected {description} to be a string, got {value}"))
    }

    /// Extracts a JSON array's elements; a non-array value becomes a typed
    /// test failure.
    fn expect_array<'a>(value: &'a Value, description: &str) -> Result<&'a Vec<Value>, String> {
        value
            .as_array()
            .ok_or_else(|| format!("expected {description} to be an array, got {value}"))
    }

    /// A `Write` used by every `serve` test in this module, success and
    /// failure alike.
    ///
    /// `serve` is generic over its output type, so each distinct concrete
    /// `W` used in tests is a separate monomorphization with its own,
    /// separately tracked coverage; a `write_all`/`flush` failure test
    /// built on its own single-purpose type never touches the ordinary
    /// success path's `write_all`/`flush` calls (or vice versa) for that
    /// type's copy of `serve`'s body. Routing every test, success and
    /// failure, through this one type keeps `serve` to a single
    /// monomorphization whose branches are all exercised somewhere in the
    /// suite. `buffer` is behind `Rc<RefCell<_>>` so a test can still read
    /// what was written after `serve` has consumed the `TestWriter` value
    /// it was handed.
    struct TestWriter {
        buffer: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        fail_write: bool,
        fail_flush: bool,
    }

    impl TestWriter {
        /// A writer that records every write and never fails, returning a
        /// handle to the buffer it will accumulate.
        fn new() -> (Self, std::rc::Rc<std::cell::RefCell<Vec<u8>>>) {
            let buffer = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let writer = Self {
                buffer: std::rc::Rc::clone(&buffer),
                fail_write: false,
                fail_flush: false,
            };
            (writer, buffer)
        }

        /// A writer whose every `write` call fails.
        fn failing_write() -> Self {
            Self {
                buffer: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                fail_write: true,
                fail_flush: false,
            }
        }

        /// A writer that accepts every write but whose every `flush` call
        /// fails.
        fn failing_flush() -> Self {
            Self {
                buffer: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
                fail_write: false,
                fail_flush: true,
            }
        }
    }

    impl Write for TestWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_write {
                return Err(std::io::Error::other("write always fails"));
            }
            self.buffer.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                return Err(std::io::Error::other("flush always fails"));
            }
            Ok(())
        }
    }

    #[test]
    fn expect_one_reports_wrong_length_as_a_typed_error() -> Result<(), String> {
        let items = [1, 2, 3];
        let err = expect_err!(expect_one(&items))?;
        assert_eq!("expected exactly 1 item, got 3", err);
        Ok(())
    }

    #[test]
    fn expect_pair_reports_wrong_length_as_a_typed_error() -> Result<(), String> {
        let items = [1, 2, 3];
        let err = expect_err!(expect_pair(&items))?;
        assert_eq!("expected exactly 2 items, got 3", err);
        Ok(())
    }

    // expect_one and expect_pair are generic, and every call through `run`
    // and `call_tool` monomorphizes them over `Value` with a slice that is
    // always the right length (by construction of those helpers), so the
    // wrong-length arm above (monomorphized over `i32`) never exercises
    // the `Value` instantiation's copy of that same source line. These
    // two calls monomorphize over `Value` directly to cover that copy too.

    #[test]
    fn expect_one_reports_wrong_length_for_json_values() -> Result<(), String> {
        let items: [Value; 0] = [];
        let err = expect_err!(expect_one(&items))?;
        assert_eq!("expected exactly 1 item, got 0", err);
        Ok(())
    }

    #[test]
    fn expect_pair_reports_wrong_length_for_json_values() -> Result<(), String> {
        let items = [Value::Null];
        let err = expect_err!(expect_pair(&items))?;
        assert_eq!("expected exactly 2 items, got 1", err);
        Ok(())
    }

    #[test]
    fn expect_array_reports_a_non_array_value() -> Result<(), String> {
        let not_array = json!({ "not": "an array" });
        let err = expect_err!(expect_array(&not_array, "test value"))?;
        assert_eq!(
            format!("expected test value to be an array, got {not_array}"),
            err
        );
        Ok(())
    }

    #[test]
    fn expect_str_reports_a_non_string_value() -> Result<(), String> {
        let not_string = json!(42);
        let err = expect_err!(expect_str(&not_string, "test value"))?;
        assert_eq!("expected test value to be a string, got 42", err);
        Ok(())
    }

    #[test]
    fn eof_on_empty_input_returns_ok() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run(&cfg, &[])?;
        assert!(responses.is_empty());
        Ok(())
    }

    #[test]
    fn blank_line_is_skipped() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";

        let responses = run(&cfg, &["", ping])?;
        assert_eq!(1, responses.len());
        Ok(())
    }

    #[test]
    fn malformed_json_then_valid_request_both_get_handled() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let ping = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}";

        let responses = run(&cfg, &["{not json", ping])?;
        let (first, second) = expect_pair(&responses)?;

        assert_eq!(&Value::Null, first.field("id"));
        assert_eq!(json!(-32_700), *first.field("error").field("code"));
        assert_eq!(json!(1), *second.field("id"));
        assert_eq!(json!({}), *second.field("result"));
        Ok(())
    }

    #[test]
    fn non_object_message_is_invalid_request() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "42")?;
        let response = expect_one(&responses)?;
        assert_eq!(&Value::Null, response.field("id"));
        assert_eq!(json!(-32_600), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn missing_jsonrpc_with_id_is_invalid_request() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"id\":5,\"method\":\"ping\"}")?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(5), *response.field("id"));
        assert_eq!(json!(-32_600), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn missing_jsonrpc_without_id_is_dropped_silently() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"method\":\"ping\"}")?;
        assert!(responses.is_empty());
        Ok(())
    }

    #[test]
    fn missing_method_with_id_is_invalid_request() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"jsonrpc\":\"2.0\",\"id\":2}")?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(2), *response.field("id"));
        assert_eq!(json!(-32_600), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn missing_method_without_id_is_dropped_silently() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"jsonrpc\":\"2.0\"}")?;
        assert!(responses.is_empty());
        Ok(())
    }

    #[test]
    fn notification_initialized_produces_no_output() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let notification = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}";

        let responses = run_one(&cfg, notification)?;
        assert!(responses.is_empty());
        Ok(())
    }

    #[test]
    fn string_id_is_echoed_back() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let request = "{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":\"ping\"}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(json!("abc"), *response.field("id"));
        Ok(())
    }

    #[test]
    fn unknown_method_is_method_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"bogus\"}")?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(-32_601), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn initialize_returns_server_info_and_tools_capability()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        let result = response.field("result");
        assert_eq!(json!("mnene"), *result.field("serverInfo").field("name"));
        assert_eq!(json!({}), *result.field("capabilities").field("tools"));
        assert_eq!(json!("2025-06-18"), *result.field("protocolVersion"));
        Ok(())
    }

    #[test]
    fn initialize_echoes_an_accepted_protocol_version() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
                        \"params\":{\"protocolVersion\":\"2025-06-18\"}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(
            json!("2025-06-18"),
            *response.field("result").field("protocolVersion")
        );
        Ok(())
    }

    #[test]
    fn initialize_falls_back_for_an_unrecognized_protocol_version()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
                        \"params\":{\"protocolVersion\":\"1999-01-01\"}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(
            json!("2025-06-18"),
            *response.field("result").field("protocolVersion")
        );
        Ok(())
    }

    #[test]
    fn ping_returns_an_empty_object() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let responses = run_one(&cfg, "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}")?;
        let response = expect_one(&responses)?;
        assert_eq!(json!({}), *response.field("result"));
        Ok(())
    }

    #[test]
    fn tools_list_lists_the_six_tools_with_schemas() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        let tools = expect_array(response.field("result").field("tools"), "tools/list result")?;
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.field("name").as_str())
            .collect();
        assert_eq!(
            vec!["put", "get", "search", "overwrite", "retract", "recall"],
            names
        );
        for tool in tools {
            assert_eq!(json!("object"), *tool.field("inputSchema").field("type"));
        }
        Ok(())
    }

    #[test]
    fn tools_call_missing_name_is_invalid_params() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(-32_602), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn tools_call_defaults_a_missing_arguments_field_to_an_empty_object()
    -> Result<(), Box<dyn std::error::Error>> {
        // "recall" has no required arguments, so omitting "arguments"
        // entirely (rather than passing `{}`) still succeeds, exercising
        // handle_tools_call's fallback to an empty object.
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"recall\"}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(false), *response.field("result").field("isError"));
        Ok(())
    }

    #[test]
    fn tools_call_unknown_tool_is_invalid_params() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"bogus\",\"arguments\":{}}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(-32_602), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn tools_call_malformed_arguments_is_invalid_params() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        // "put" requires a string "body"; omitting it fails to deserialize
        // into PutArgs.
        let request = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"put\",\"arguments\":{}}}";

        let responses = run_one(&cfg, request)?;
        let response = expect_one(&responses)?;
        assert_eq!(json!(-32_602), *response.field("error").field("code"));
        Ok(())
    }

    #[test]
    fn every_tool_rejects_arguments_missing_its_required_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        // parse_args is generic over each tool's argument struct, so its
        // deserialize-failure path is monomorphized once per tool; "put"'s
        // is covered by the test above, and this covers the rest.
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        // Every field of RecallArgs has a serde default, so `{}` alone
        // deserializes fine; a wrong-typed `limit` is what fails for it.
        let bad_arguments: [(&str, Value); 5] = [
            ("get", json!({})),
            ("search", json!({})),
            ("overwrite", json!({})),
            ("retract", json!({})),
            ("recall", json!({ "limit": "not-a-number" })),
        ];

        for (tool, arguments) in bad_arguments {
            let request = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": tool, "arguments": arguments },
            });
            let request_line = request.to_string();
            let responses = run_one(&cfg, &request_line)?;
            let response = expect_one(&responses)?;
            assert_eq!(
                json!(-32_602),
                *response.field("error").field("code"),
                "{tool}"
            );
        }
        Ok(())
    }

    /// Runs a `tools/call` for `tool` with `arguments` and returns the
    /// tool result envelope (`{"content": [...], "isError": ...}`).
    fn call_tool(
        cfg: &Config,
        tool: &str,
        arguments: &Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        });
        let request_line = request.to_string();
        let responses = run_one(cfg, &request_line)?;
        let response = expect_one(&responses)?;
        Ok(response.field("result").clone())
    }

    /// Extracts a tool result envelope's `content[0].text` field.
    fn tool_text(envelope: &Value) -> Result<&str, Box<dyn std::error::Error>> {
        let text = envelope.field("content").at(0).field("text");
        Ok(expect_str(text, "content[0].text")?)
    }

    /// Parses a tool result envelope's `content[0].text` field back into
    /// JSON, for asserting on the success payload it carries.
    fn tool_text_json(envelope: &Value) -> Result<Value, Box<dyn std::error::Error>> {
        Ok(serde_json::from_str(tool_text(envelope)?)?)
    }

    #[test]
    fn put_then_get_round_trips_a_memory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_args = json!({ "body": "first memory", "tags": ["Alpha"] });

        let put_envelope = call_tool(&cfg, "put", &put_args)?;
        assert_eq!(json!(false), *put_envelope.field("isError"));
        let put_value = tool_text_json(&put_envelope)?;
        let id = expect_str(put_value.field("id"), "put result id")?;

        let get_envelope = call_tool(&cfg, "get", &json!({ "id": id }))?;
        assert_eq!(json!(false), *get_envelope.field("isError"));
        let memory = tool_text_json(&get_envelope)?;
        assert_eq!(json!("first memory"), *memory.field("body"));
        let tags: Vec<&str> = memory
            .field("tags")
            .as_array()
            .map_or_else(Vec::new, |tags| {
                tags.iter().filter_map(Value::as_str).collect()
            });
        assert_eq!(vec!["alpha"], tags);
        assert_eq!(json!("active"), *memory.field("state"));
        Ok(())
    }

    #[test]
    fn put_without_agent_is_missing_agent() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), None, Some("scope-a"));

        let envelope = call_tool(&cfg, "put", &json!({ "body": "body text" }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("agent is required", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn overwrite_without_scope_is_missing_scope() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let seeded = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_envelope = call_tool(&seeded, "put", &json!({ "body": "body text" }))?;
        let put_value = tool_text_json(&put_envelope)?;
        let id = expect_str(put_value.field("id"), "put result id")?;
        let overwrite_args = json!({ "id": id, "body": "new body" });

        let scopeless = config(dir.path(), Some("agent-a"), None);
        let envelope = call_tool(&scopeless, "overwrite", &overwrite_args)?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("scope is required", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn put_with_blank_body_is_empty_body() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let envelope = call_tool(&cfg, "put", &json!({ "body": "   " }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("memory body must not be blank", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn put_with_an_invalid_tag_is_invalid_tag() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_args = json!({ "body": "body text", "tags": ["---"] });

        let envelope = call_tool(&cfg, "put", &put_args)?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("invalid tag: ---", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn get_of_unknown_id_is_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let unknown = crate::model::MemoryId::new();

        let envelope = call_tool(&cfg, "get", &json!({ "id": unknown.to_string() }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!(format!("memory {unknown} not found"), tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn get_with_a_malformed_id_is_invalid_id() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));

        let envelope = call_tool(&cfg, "get", &json!({ "id": "not-a-uuid" }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("invalid memory id: not-a-uuid", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn search_with_default_scope_finds_a_matching_memory() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        call_tool(&cfg, "put", &json!({ "body": "a distinctive searchword" }))?;
        let search_args = json!({ "query": "distinctive", "tags": [] });

        let envelope = call_tool(&cfg, "search", &search_args)?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let hits = tool_text_json(&envelope)?;
        let hits = expect_array(&hits, "search result")?;
        assert_eq!(1, hits.len());
        Ok(())
    }

    #[test]
    fn search_with_all_scopes_widens_beyond_the_configured_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let writer = config(dir.path(), Some("agent-a"), Some("scope-a"));
        call_tool(&writer, "put", &json!({ "body": "cross scope searchword" }))?;
        let search_args = json!({ "query": "searchword", "all_scopes": true });

        let reader = config(dir.path(), Some("agent-b"), Some("scope-b"));
        let envelope = call_tool(&reader, "search", &search_args)?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let hits = tool_text_json(&envelope)?;
        let hits = expect_array(&hits, "search result")?;
        assert_eq!(1, hits.len());
        Ok(())
    }

    #[test]
    fn search_without_scope_is_missing_scope() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), None);

        let envelope = call_tool(&cfg, "search", &json!({ "query": "anything" }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("scope is required", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn overwrite_supersedes_an_active_memory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_envelope = call_tool(&cfg, "put", &json!({ "body": "first version" }))?;
        let put_value = tool_text_json(&put_envelope)?;
        let old_id = expect_str(put_value.field("id"), "put result id")?.to_string();
        let overwrite_args = json!({ "id": old_id, "body": "second version" });

        let envelope = call_tool(&cfg, "overwrite", &overwrite_args)?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let overwrite_value = tool_text_json(&envelope)?;
        let new_id = expect_str(overwrite_value.field("id"), "overwrite result id")?;
        assert_ne!(old_id, new_id);
        Ok(())
    }

    #[test]
    fn overwrite_of_a_non_active_id_is_not_active() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_envelope = call_tool(&cfg, "put", &json!({ "body": "first version" }))?;
        let put_value = tool_text_json(&put_envelope)?;
        let old_id = expect_str(put_value.field("id"), "put result id")?.to_string();
        let second_args = json!({ "id": old_id, "body": "second version" });
        call_tool(&cfg, "overwrite", &second_args)?;

        let third_args = json!({ "id": old_id, "body": "third version" });
        let envelope = call_tool(&cfg, "overwrite", &third_args)?;
        assert_eq!(json!(true), *envelope.field("isError"));
        let text = tool_text(&envelope)?;
        assert!(text.contains("cannot be modified"), "got {text}");
        Ok(())
    }

    #[test]
    fn retract_retires_an_active_memory() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_envelope = call_tool(&cfg, "put", &json!({ "body": "body text" }))?;
        let put_value = tool_text_json(&put_envelope)?;
        let id = expect_str(put_value.field("id"), "put result id")?.to_string();

        let envelope = call_tool(&cfg, "retract", &json!({ "id": id }))?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let retract_value = tool_text_json(&envelope)?;
        assert_eq!(json!(id), *retract_value.field("id"));
        assert_eq!(json!("retracted"), *retract_value.field("state"));
        Ok(())
    }

    #[test]
    fn retract_of_an_already_retracted_id_is_not_active() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let put_envelope = call_tool(&cfg, "put", &json!({ "body": "body text" }))?;
        let put_value = tool_text_json(&put_envelope)?;
        let id = expect_str(put_value.field("id"), "put result id")?.to_string();
        call_tool(&cfg, "retract", &json!({ "id": id.as_str() }))?;

        let envelope = call_tool(&cfg, "retract", &json!({ "id": id }))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        let text = tool_text(&envelope)?;
        assert!(text.contains("cannot be modified"), "got {text}");
        Ok(())
    }

    #[test]
    fn recall_returns_active_memories_newest_first() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        call_tool(&cfg, "put", &json!({ "body": "a memory" }))?;

        let envelope = call_tool(&cfg, "recall", &json!({}))?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let memories = tool_text_json(&envelope)?;
        let memories = expect_array(&memories, "recall result")?;
        assert_eq!(1, memories.len());
        Ok(())
    }

    #[test]
    fn recall_with_all_scopes_widens_beyond_the_configured_scope()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let writer = config(dir.path(), Some("agent-a"), Some("scope-a"));
        call_tool(&writer, "put", &json!({ "body": "a memory" }))?;

        let reader = config(dir.path(), Some("agent-b"), Some("scope-b"));
        let envelope = call_tool(&reader, "recall", &json!({ "all_scopes": true }))?;
        assert_eq!(json!(false), *envelope.field("isError"));
        let memories = tool_text_json(&envelope)?;
        let memories = expect_array(&memories, "recall result")?;
        assert_eq!(1, memories.len());
        Ok(())
    }

    #[test]
    fn recall_without_scope_is_missing_scope() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), None);

        let envelope = call_tool(&cfg, "recall", &json!({}))?;
        assert_eq!(json!(true), *envelope.field("isError"));
        assert_eq!("scope is required", tool_text(&envelope)?);
        Ok(())
    }

    #[test]
    fn serve_maps_a_write_failure_to_a_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let input = Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n".to_vec());

        let err = expect_err!(serve(&cfg, input, TestWriter::failing_write()))?;
        assert!(
            matches!(err, crate::model::MneneError::Storage(_)),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn serve_maps_a_flush_failure_to_a_storage_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let input = Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n".to_vec());

        let err = expect_err!(serve(&cfg, input, TestWriter::failing_flush()))?;
        assert!(
            matches!(err, crate::model::MneneError::Storage(_)),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn serve_maps_an_invalid_utf8_line_to_a_storage_error() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        let input = Cursor::new(vec![0xFF, 0xFE, b'\n']);
        let (writer, _buffer) = TestWriter::new();

        let err = expect_err!(serve(&cfg, input, writer))?;
        assert!(
            matches!(err, crate::model::MneneError::Storage(_)),
            "got {err:?}"
        );
        Ok(())
    }

    #[test]
    fn serve_reports_a_storage_error_when_the_database_cannot_be_opened()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let mut cfg = config(dir.path(), Some("agent-a"), Some("scope-a"));
        cfg.db = dir.path().to_path_buf();
        let input = Cursor::new(Vec::new());
        let (writer, _buffer) = TestWriter::new();

        let err = expect_err!(serve(&cfg, input, writer))?;
        assert!(
            matches!(err, crate::model::MneneError::Storage(_)),
            "got {err:?}"
        );
        Ok(())
    }
}
