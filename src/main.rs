//! Command-line interface for `mnene`.
//!
//! This binary is a thin clap parser (CLI-001, RS-004): it shapes the
//! global flags, builds [`Config`](mnene::config::Config) from them plus
//! the process environment, opens the store, dispatches to the library,
//! and renders the result as text or JSON. Every decision about memories
//! themselves happens in the library crate.
//!
//! Parsing, shared-metadata routing (`meta version`/`license`/
//! `completions`/`doctor`/`agent`), and fatal-error rendering all go
//! through `tftio_lib::run_cli_from`, the same shared CLI runner
//! `prompter` and `clanker` use. `Verb` -- the clap enum this binary's
//! `Cli` parses its subcommand into -- lives in `mnene::verb` rather than
//! here; see that module's doc comment for why (in short: it must include
//! a `Meta` arm to parse at all, but that arm can never actually be
//! dispatched here, and this file has no unit-test harness of its own to
//! cover a dead arm with).

use std::path::PathBuf;

use clap::Parser;
use mnene::agent_surface::SURFACE;
use mnene::config::{Config, RawConfig};
use mnene::doctor::MneneDoctor;
use mnene::model::{Memory, MemoryId, MneneError, SearchHit, Tag};
use mnene::store::SqliteStore;
use mnene::store::recall::RecallOptions;
use mnene::store::scopes::ScopeSummary;
use mnene::store::search::SearchOptions;
use mnene::verb::{DomainVerb, Verb, domain_label, ensure_domain_verb, wrap_fatal};
use tftio_lib::{
    AGENT_TOKEN_ENV, AGENT_TOKEN_EXPECTED_ENV, AgentModeContext, FatalCliError, JsonOutput,
    LicenseType, ProcessEnv, RepoInfo, StandardCommand, ToolSpec, map_standard_command,
    run_cli_from,
};

/// Authoritative `ToolSpec` for `mnene`: the `tftio/mnene` repository
/// identity, `MIT` license, both shared capabilities (JSON and doctor
/// support), and the supervised agent surface declared in
/// [`mnene::agent_surface`].
static TOOL_SPEC: ToolSpec = ToolSpec::new(
    "mnene",
    "mnene",
    env!("CARGO_PKG_VERSION"),
    LicenseType::MIT,
    RepoInfo::new("tftio", "mnene"),
    true,
    true,
)
.with_agent_surface(&SURFACE);

/// Command-line arguments for `mnene`.
///
/// Provenance (`agent`, `context`, `session`, `scope`, and the write-path
/// `task`) is deliberately absent from this struct: those five values are
/// stored attribution, and T002 moved their collection out of clap and into
/// [`process_snapshot`], so an ordinary flag can never substitute a
/// different value for the one the process inherited from its environment.
/// `--json` and `--db` remain here because they select rendering and a
/// local resource rather than asserting who or what wrote a memory.
#[derive(Debug, Parser)]
#[command(name = "mnene", author, version, about)]
struct Cli {
    /// Render output as JSON instead of human-readable text.
    #[arg(long, global = true)]
    json: bool,

    /// Path to the `SQLite` database file, overriding the default location.
    ///
    /// Takes precedence over `MNENE_DB` (read in [`process_snapshot`]) when
    /// both are given; see [`run`].
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// The verb to run.
    #[command(subcommand)]
    verb: Verb,
}

/// Parses a CLI id argument into a [`MemoryId`].
///
/// # Errors
///
/// Returns [`MneneError::InvalidId`] when `raw` is not a hyphenated
/// `UUIDv7`.
fn parse_id(raw: &str) -> Result<MemoryId, MneneError> {
    raw.parse()
}

/// Parses a list of CLI tag arguments into validated [`Tag`]s.
///
/// # Errors
///
/// Returns [`MneneError::InvalidTag`] naming the first tag that fails
/// normalization.
fn parse_tags(raw: &[String]) -> Result<Vec<Tag>, MneneError> {
    raw.iter().map(|tag| Tag::new(tag)).collect()
}

/// One line of rendered `search` output: the memory id and its score,
/// score formatted to four decimal places in text mode.
fn render_search_hit_text(hit: &SearchHit) {
    println!("{} {:.4}", hit.id, hit.score);
}

/// Renders one memory's metadata-and-body block in text mode.
fn render_memory_text(memory: &Memory) {
    println!("id: {}", memory.id);
    println!("state: {}", memory.state);
    println!("scope: {}", memory.provenance.scope);
    println!(
        "scope_source: {}",
        memory
            .provenance
            .scope_source
            .map_or(String::new(), |source| source.to_string())
    );
    println!("task: {}", memory.provenance.task.as_deref().unwrap_or(""));
    println!("agent: {}", memory.provenance.agent);
    println!("context: {}", memory.provenance.context);
    println!(
        "session: {}",
        memory.provenance.session.as_deref().unwrap_or("")
    );
    println!("created_at: {}", memory.created_at);
    println!("closed_at: {}", memory.closed_at.as_deref().unwrap_or(""));
    println!("closed_by: {}", memory.closed_by.as_deref().unwrap_or(""));
    println!(
        "supersedes: {}",
        memory
            .supersedes
            .map(|id| id.to_string())
            .unwrap_or_default()
    );
    println!(
        "superseded_by: {}",
        memory
            .superseded_by
            .map(|id| id.to_string())
            .unwrap_or_default()
    );
    let tags: Vec<&str> = memory.tags.iter().map(Tag::as_str).collect();
    println!("tags: {}", tags.join(" "));
    println!();
    println!("{}", memory.body);
}

/// Renders a sequence of memories in text mode, one block per memory,
/// blocks separated by a line of `---`.
fn render_memories_text(memories: &[Memory]) {
    for (index, memory) in memories.iter().enumerate() {
        if index > 0 {
            println!("---");
        }
        render_memory_text(memory);
    }
}

/// Renders `scopes` output in text mode: one line per scope, `<scope>
/// <count> [<source>,<source>...]`.
fn render_scopes_text(scopes: &[ScopeSummary]) {
    for summary in scopes {
        println!(
            "{} {} [{}]",
            summary.scope,
            summary.count,
            summary.sources.join(",")
        );
    }
}

/// Serializes `value` to pretty JSON and prints it.
///
/// A serialization failure cannot occur for the types this binary
/// serializes (all `#[derive(Serialize)]` over plain data: strings,
/// options, and enums, with no interior mutability, non-finite floats, or
/// non-string map keys), so this deliberately does not give that
/// unreachable case its own local error type or mapping closure -- doing
/// so would add dead code with no test that could ever exercise it.
/// Letting `?` fall through to `anyhow`'s blanket conversion keeps the
/// (equally unreachable) failure path entirely inside `anyhow`'s own code,
/// not ours; [`MneneError`]'s own `From<anyhow::Error>` impl then bridges
/// that back to the domain error type `run_domain` returns.
///
/// # Errors
///
/// Returns an error if `serde_json` fails to render `value`.
fn print_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let rendered = serde_json::to_string_pretty(value)?;
    println!("{rendered}");
    Ok(())
}

/// Runs one domain verb against `store` using `config`, rendering the
/// result as JSON when `json` is set, or as human-readable text otherwise.
/// `Mcp` ignores `store` (already opened by [`run`] for the other verbs)
/// and instead serves the six data verbs as MCP tools over the process's
/// own stdin and stdout until EOF, through the same [`Config`] edge every
/// other verb uses.
///
/// # Errors
///
/// Returns any [`MneneError`] the verb's store call or argument parsing
/// produces.
fn run_domain(
    store: &mut SqliteStore,
    config: &Config,
    json: bool,
    verb: DomainVerb,
) -> Result<i32, MneneError> {
    match verb {
        DomainVerb::Put { body, tags } => run_put(store, config, json, &body, &tags),
        DomainVerb::Get { id } => run_get(store, json, &id),
        DomainVerb::Search {
            query,
            limit,
            include_superseded,
            all_scopes,
            tags,
        } => run_search(
            store,
            config,
            json,
            &query,
            &SearchArgs {
                limit,
                include_superseded,
                all_scopes,
                tags,
            },
        ),
        DomainVerb::Overwrite { id, body, tags } => {
            run_overwrite(store, config, json, &id, &body, &tags)
        }
        DomainVerb::Retract { id } => run_retract(store, config, json, &id),
        DomainVerb::Recall {
            limit,
            task,
            all_scopes,
        } => run_recall(store, config, json, limit, task, all_scopes),
        DomainVerb::Mcp => {
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            mnene::mcp::serve(config, stdin.lock(), stdout.lock())?;
            Ok(0)
        }
        DomainVerb::Scopes => run_scopes(store, json),
    }
}

/// Runs `put`: stores a new memory and prints its minted id.
fn run_put(
    store: &mut SqliteStore,
    config: &Config,
    json: bool,
    body: &str,
    tags: &[String],
) -> Result<i32, MneneError> {
    let provenance = config.provenance()?;
    let tags = parse_tags(tags)?;
    let id = store.put(body, &tags, &provenance)?;
    if json {
        print_json(&serde_json::json!({ "id": id.to_string() }))?;
    } else {
        println!("{id}");
    }
    Ok(0)
}

/// Runs `get`: fetches one memory by id.
fn run_get(store: &SqliteStore, json: bool, id: &str) -> Result<i32, MneneError> {
    let id = parse_id(id)?;
    let memory = store.get(id)?;
    if json {
        print_json(&memory)?;
    } else {
        render_memory_text(&memory);
    }
    Ok(0)
}

/// The `search`-specific arguments `run_search` needs beyond `query`,
/// bundled so the function stays under clippy's argument-count limit.
struct SearchArgs {
    /// Maximum number of hits to return.
    limit: u32,
    /// Include superseded memories in results.
    include_superseded: bool,
    /// Search every scope instead of only the current one.
    all_scopes: bool,
    /// Tags a hit must carry every one of.
    tags: Vec<String>,
}

/// Runs `search`: bounded keyword search over memory bodies and tags.
fn run_search(
    store: &SqliteStore,
    config: &Config,
    json: bool,
    query: &str,
    args: &SearchArgs,
) -> Result<i32, MneneError> {
    let scope = if args.all_scopes {
        None
    } else {
        Some(config.require_scope()?.to_string())
    };
    let required_tags = parse_tags(&args.tags)?;
    let options = SearchOptions {
        limit: args.limit,
        include_superseded: args.include_superseded,
        scope,
        required_tags,
    };
    let hits = store.search(query, &options)?;
    if json {
        print_json(&hits)?;
    } else {
        for hit in &hits {
            render_search_hit_text(hit);
        }
    }
    Ok(0)
}

/// Runs `overwrite`: supersedes an active memory with a new one.
fn run_overwrite(
    store: &mut SqliteStore,
    config: &Config,
    json: bool,
    id: &str,
    body: &str,
    tags: &[String],
) -> Result<i32, MneneError> {
    let provenance = config.provenance()?;
    let id = parse_id(id)?;
    let tags = parse_tags(tags)?;
    let new_id = store.overwrite(id, body, &tags, &provenance)?;
    if json {
        print_json(&serde_json::json!({ "id": new_id.to_string() }))?;
    } else {
        println!("{new_id}");
    }
    Ok(0)
}

/// Runs `retract`: retires an active memory with no replacement.
fn run_retract(
    store: &mut SqliteStore,
    config: &Config,
    json: bool,
    id: &str,
) -> Result<i32, MneneError> {
    let provenance = config.provenance()?;
    let id = parse_id(id)?;
    store.retract(id, &provenance)?;
    if json {
        print_json(&serde_json::json!({ "id": id.to_string(), "state": "retracted" }))?;
    } else {
        println!("retracted {id}");
    }
    Ok(0)
}

/// Runs `recall`: lists active memories in scope, newest first.
///
/// The Surface says `recall [--task T]`: when `--task` is absent, recall
/// must not silently fall back to `config.task` (the invoking session's own
/// task), because that would hide every other task's memories in scope
/// from a fresh session's inheritance. Only an explicit `--task` narrows
/// recall by task.
fn run_recall(
    store: &SqliteStore,
    config: &Config,
    json: bool,
    limit: u32,
    task: Option<String>,
    all_scopes: bool,
) -> Result<i32, MneneError> {
    let scope = if all_scopes {
        None
    } else {
        Some(config.require_scope()?.to_string())
    };
    let options = RecallOptions { limit, scope, task };
    let memories = store.recall(&options)?;
    if json {
        print_json(&memories)?;
    } else {
        render_memories_text(&memories);
    }
    Ok(0)
}

/// Runs `scopes`: lists every distinct scope value stored.
fn run_scopes(store: &SqliteStore, json: bool) -> Result<i32, MneneError> {
    let scopes = store.scopes()?;
    if json {
        print_json(&scopes)?;
    } else {
        render_scopes_text(&scopes);
    }
    Ok(0)
}

/// Routes `cli.verb`'s shared `Meta` commands through `tftio-lib`'s
/// metadata mapper; every other verb returns `None`, leaving it to
/// [`run`].
fn metadata_command(cli: &Cli) -> Option<StandardCommand> {
    match &cli.verb {
        Verb::Meta { command } => Some(map_standard_command(
            command,
            JsonOutput::from_flag(cli.json),
        )),
        _ => None,
    }
}

/// Every value this binary reads from the process environment, captured
/// once at the process edge by [`process_snapshot`] (RS-008).
///
/// `tftio_lib::ProcessEnv` (supervision state: the agent-mode token pair
/// and `HOME`) and [`RawConfig`] (stored provenance and storage location)
/// are both built from this one snapshot in [`main`] and [`run`]
/// respectively, so neither reads the environment on its own.
/// `tftio_lib::ProcessEnv`'s own `agent` field is supervision state and
/// stays distinct from [`Config`]'s own `agent` field, which is stored
/// provenance -- the two happen to share a name but never a value.
struct ProcessSnapshot {
    /// `tftio-lib`'s own supervision environment: the `TFTIO_AGENT_TOKEN`
    /// pair and `HOME`.
    tftio_env: ProcessEnv,
    /// `MNENE_AGENT`, stored provenance for writes.
    agent: Option<String>,
    /// `MNENE_CONTEXT`, selecting which per-context database to use.
    context: Option<String>,
    /// `MNENE_SESSION`, stored provenance for writes.
    session: Option<String>,
    /// `MNENE_SCOPE`, bounding `search` and `recall`.
    scope: Option<String>,
    /// `MNENE_TASK`, stored provenance for writes.
    ///
    /// Distinct from `recall`'s own local `--task` flag, which is a
    /// retrieval filter parsed by clap rather than provenance read here.
    task: Option<String>,
    /// `MNENE_DB`, the fallback database path when `--db` is absent.
    db: Option<PathBuf>,
    /// `CLANKER_SESSION_HARNESS`, clanker's fallback for `agent`.
    clanker_session_harness: Option<String>,
    /// `CLANKER_SESSION_CONTEXT`, clanker's fallback for `context`.
    clanker_session_context: Option<String>,
    /// `CLANKER_SESSION_ID`, clanker's fallback for `session`.
    clanker_session_id: Option<String>,
    /// `CLANKER_SESSION_PROJECT`, clanker's fallback for `scope` when
    /// `tftio_lib::project` resolution finds nothing (T013). clanker never
    /// sets `MNENE_SCOPE` itself.
    clanker_session_project: Option<String>,
    /// `XDG_DATA_HOME`, used to build the default `db` path.
    xdg_data_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`, used to locate the installed project registry.
    xdg_config_home: Option<PathBuf>,
    /// `HOME`, used to build the default `db` path when `XDG_DATA_HOME` is
    /// absent, and shared with `tftio_env.home`.
    home: Option<PathBuf>,
    /// The current working directory, the starting point for git
    /// discovery in [`Config::resolve`](mnene::config::Config::resolve).
    ///
    /// Kept as a `Result` rather than unwrapped here so a failure to read
    /// it is reported through [`FatalCliError`] under the command that was
    /// actually running, in [`run`], rather than aborting before argument
    /// parsing.
    current_dir: std::io::Result<PathBuf>,
}

/// Builds `Config` from the process snapshot and the `--db` override, opens
/// the store, and dispatches to [`run_domain`].
///
/// `tftio_lib::run_cli_from` only calls this for a non-`Meta` verb (see
/// [`metadata_command`]), so [`ensure_domain_verb`](mnene::verb::ensure_domain_verb)
/// never actually returns its `Err` case here; see `mnene::verb`'s doc
/// comment for why that arm still exists, and why it lives there rather
/// than being written inline in this function.
///
/// # Errors
///
/// Returns a [`FatalCliError`] from reading the current directory, opening
/// the store, or running the requested verb.
fn run(cli: Cli, snapshot: ProcessSnapshot) -> Result<i32, FatalCliError> {
    let json = cli.json;
    let output = JsonOutput::from_flag(json);
    let raw = RawConfig {
        agent: snapshot.agent,
        context: snapshot.context,
        session: snapshot.session,
        scope: snapshot.scope,
        task: snapshot.task,
        db: cli.db.or(snapshot.db),
        clanker_session_harness: snapshot.clanker_session_harness,
        clanker_session_context: snapshot.clanker_session_context,
        clanker_session_id: snapshot.clanker_session_id,
        clanker_session_project: snapshot.clanker_session_project,
        xdg_data_home: snapshot.xdg_data_home,
        xdg_config_home: snapshot.xdg_config_home,
        home: snapshot.home,
    };

    let domain_verb = ensure_domain_verb(cli.verb, output)?;
    let label = domain_label(&domain_verb);

    let current_dir = snapshot.current_dir.map_err(wrap_fatal(label, output))?;
    let config = Config::resolve(raw, &current_dir);

    let mut store = SqliteStore::open(&config.db).map_err(wrap_fatal(label, output))?;

    run_domain(&mut store, &config, json, domain_verb).map_err(wrap_fatal(label, output))
}

/// Reads every environment-sourced value this binary needs -- the shared
/// `TFTIO_AGENT_TOKEN` pair, `HOME`, the five `MNENE_*` provenance and
/// storage variables, the four `CLANKER_SESSION_*` fallbacks (including
/// `CLANKER_SESSION_PROJECT`, T013's scope tier), `XDG_DATA_HOME`,
/// `XDG_CONFIG_HOME`, and the current directory -- exactly once at the
/// process edge, into one typed [`ProcessSnapshot`] (RS-008). This is the
/// binary's only `#[allow(clippy::disallowed_methods)]` site.
#[allow(
    clippy::disallowed_methods,
    reason = "process environment read once at the process edge (REPO_INVARIANTS.md RS-008)"
)]
fn process_snapshot() -> ProcessSnapshot {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    ProcessSnapshot {
        tftio_env: ProcessEnv {
            agent: AgentModeContext::from_tokens(
                std::env::var(AGENT_TOKEN_ENV).ok(),
                std::env::var(AGENT_TOKEN_EXPECTED_ENV).ok(),
            ),
            home: home.clone(),
        },
        agent: std::env::var("MNENE_AGENT").ok(),
        context: std::env::var("MNENE_CONTEXT").ok(),
        session: std::env::var("MNENE_SESSION").ok(),
        scope: std::env::var("MNENE_SCOPE").ok(),
        task: std::env::var("MNENE_TASK").ok(),
        db: std::env::var_os("MNENE_DB").map(PathBuf::from),
        clanker_session_harness: std::env::var("CLANKER_SESSION_HARNESS").ok(),
        clanker_session_context: std::env::var("CLANKER_SESSION_CONTEXT").ok(),
        clanker_session_id: std::env::var("CLANKER_SESSION_ID").ok(),
        clanker_session_project: std::env::var("CLANKER_SESSION_PROJECT").ok(),
        xdg_data_home: std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        home,
        current_dir: std::env::current_dir(),
    }
}

fn main() {
    let snapshot = process_snapshot();
    let tftio_env = snapshot.tftio_env.clone();
    let exit_code = run_cli_from::<Cli, _, MneneDoctor, _, _>(
        &TOOL_SPEC,
        &tftio_env,
        std::env::args_os(),
        &MneneDoctor,
        metadata_command,
        move |cli| run(cli, snapshot),
    );
    std::process::exit(exit_code);
}
