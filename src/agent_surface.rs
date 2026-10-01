//! Agent capability declarations for `mnene`'s supervised agent surface.
//!
//! One [`tftio_lib::AgentCapability`] per task-level memory operation --
//! `put`, `get`, `search`, `recall`, `overwrite`, `retract`, and `scopes` --
//! following `kb`'s flat, one-capability-per-command pattern. `tftio-lib` renders each
//! capability into a skill named `mnene-<name>` (see
//! `tftio_lib::agent_skill::skill_name`). `meta` and `mcp` are deliberately
//! absent: `meta` is the shared inspection surface itself, and `mcp` is
//! transport bootstrap rather than a task-level operation, so neither
//! belongs in the supervised command surface or in generated skills.
//!
//! `--json` and `--db` are declared with an empty command path on every
//! capability because `src/main.rs` marks both `global = true` on the root
//! `Cli`: `tftio_lib::agent::apply_agent_surface` only keeps a flag whose
//! [`tftio_lib::FlagSelector::command_path`] equals the `clap::Command`
//! currently being filtered, and a `global = true` argument lives on the
//! root command rather than being cloned onto each subcommand -- the same
//! shape `clanker`'s own `--json` (`FlagSelector::new(&[], "json")`) and
//! `kb`'s own `--db` (`GLOBAL_DB_FLAG`) use.

use tftio_lib::{AgentCapability, AgentSurfaceSpec, CommandSelector, FlagSelector};

/// The global `--json` flag, shared by every capability.
const JSON_FLAG: FlagSelector = FlagSelector::new(&[], "json");
/// The global `--db` flag, shared by every capability: a storage-location
/// override, not provenance, so agents may set it freely.
const DB_FLAG: FlagSelector = FlagSelector::new(&[], "db");

const PUT_COMMAND: CommandSelector = CommandSelector::new(&["put"]);
const GET_COMMAND: CommandSelector = CommandSelector::new(&["get"]);
const SEARCH_COMMAND: CommandSelector = CommandSelector::new(&["search"]);
const RECALL_COMMAND: CommandSelector = CommandSelector::new(&["recall"]);
const OVERWRITE_COMMAND: CommandSelector = CommandSelector::new(&["overwrite"]);
const RETRACT_COMMAND: CommandSelector = CommandSelector::new(&["retract"]);
const SCOPES_COMMAND: CommandSelector = CommandSelector::new(&["scopes"]);

const PUT_TAG_FLAG: FlagSelector = FlagSelector::new(&["put"], "tag");
const SEARCH_LIMIT_FLAG: FlagSelector = FlagSelector::new(&["search"], "limit");
const SEARCH_INCLUDE_SUPERSEDED_FLAG: FlagSelector =
    FlagSelector::new(&["search"], "include-superseded");
const SEARCH_ALL_SCOPES_FLAG: FlagSelector = FlagSelector::new(&["search"], "all-scopes");
const SEARCH_TAG_FLAG: FlagSelector = FlagSelector::new(&["search"], "tag");
const RECALL_LIMIT_FLAG: FlagSelector = FlagSelector::new(&["recall"], "limit");
const RECALL_TASK_FLAG: FlagSelector = FlagSelector::new(&["recall"], "task");
const RECALL_ALL_SCOPES_FLAG: FlagSelector = FlagSelector::new(&["recall"], "all-scopes");
const OVERWRITE_TAG_FLAG: FlagSelector = FlagSelector::new(&["overwrite"], "tag");

/// `put` writes a new memory and prints its minted id.
const PUT_CAPABILITY: AgentCapability = AgentCapability::new(
    "put",
    "Store a new memory and print its id. The id is a UUIDv7, so ids sort \
     chronologically by creation. In text mode the id is printed alone on \
     one line; with --json the response is {\"id\": \"<uuid>\"}",
    &[PUT_COMMAND],
    &[JSON_FLAG, DB_FLAG, PUT_TAG_FLAG],
)
.with_examples(&[
    "mnene put \"the release pipeline now requires a signed tag\"",
    "mnene --json put \"prefer rebase over merge on this repo\" --tag conventions --tag git",
])
.with_output(
    "text mode prints the new memory's id alone on stdout; --json prints \
     {\"id\": \"<uuid>\"} pretty-printed. Exit status is 0 on success.",
)
.with_constraints(
    "requires MNENE_AGENT (or the CLANKER_SESSION_HARNESS fallback) and a \
     scope (MNENE_SCOPE, or the git repository name discovered from the \
     working directory) to be resolvable \
     at the process edge; a write with neither fails with a typed \
     MissingAgent or MissingScope error rather than storing an unattributed \
     memory. The body must be non-blank after trimming.",
)
.with_when_to_use(
    "the agent has learned something durable that a later session in the \
     same scope should be told about at start -- a decision, a convention, \
     a fact about the environment -- and wants it retrievable by recall or \
     search",
)
.with_when_not_to_use(
    "the information is transient scratch relevant only to the current \
     turn or task step, or it already exists as an active memory that \
     needs correcting -- use overwrite instead of storing a duplicate",
);

/// `get` retrieves one memory by id.
const GET_CAPABILITY: AgentCapability = AgentCapability::new(
    "get",
    "Retrieve a single memory by its UUIDv7 id, whatever its lifecycle state",
    &[GET_COMMAND],
    &[JSON_FLAG, DB_FLAG],
)
.with_examples(&["mnene --json get 018f2f5e-1c2e-7c3a-9b2e-1a2b3c4d5e6f"])
.with_output(
    "text mode prints a metadata-and-body block (id, state, scope, task, \
     agent, context, session, created_at, closed_at, closed_by, \
     supersedes, superseded_by, tags, a blank line, then the body); \
     --json prints the same fields as a single JSON object. Exit status is \
     1, with a \"memory <id> not found\" message, when no memory has that id.",
)
.with_constraints("read-only; never writes or changes lifecycle state")
.with_when_to_use(
    "the exact memory id is already known -- for example from a prior put, \
     overwrite, search, or recall result -- and its full body and metadata \
     are needed",
)
.with_when_not_to_use(
    "the id is not known -- use search for keyword lookup or recall to \
     list active memories in scope instead of guessing an id",
);

/// `search` finds memories by keyword.
const SEARCH_CAPABILITY: AgentCapability = AgentCapability::new(
    "search",
    "Full-text keyword search over memory bodies in the current scope, \
     ranked by relevance",
    &[SEARCH_COMMAND],
    &[
        JSON_FLAG,
        DB_FLAG,
        SEARCH_LIMIT_FLAG,
        SEARCH_INCLUDE_SUPERSEDED_FLAG,
        SEARCH_ALL_SCOPES_FLAG,
        SEARCH_TAG_FLAG,
    ],
)
.with_examples(&[
    "mnene --json search \"deploy pipeline\"",
    "mnene --json search \"conventions\" --limit 5 --tag git",
    "mnene --json search \"legacy config\" --include-superseded --all-scopes",
])
.with_output(
    "text mode prints one line per hit, \"<id> <score>\" with the score to \
     four decimal places, ordered most relevant first; --json prints an \
     array of {\"id\": \"<uuid>\", \"score\": <number>} objects. An empty \
     result is an empty array or no lines, not an error.",
)
.with_constraints(
    "by default only memories in the resolvable scope are searched \
     (requires MNENE_SCOPE or git discovery unless --all-scopes is given); \
     retracted memories are never returned, and superseded memories are \
     excluded unless --include-superseded is given -- --include-superseded \
     never surfaces retracted memories",
)
.with_when_to_use(
    "looking for memories matching specific keywords or a tag, rather than \
     browsing everything active in scope",
)
.with_when_not_to_use(
    "the goal is a general orientation dump at session start rather than a \
     keyword lookup -- use recall instead",
);

/// `recall` lists active memories in scope.
const RECALL_CAPABILITY: AgentCapability = AgentCapability::new(
    "recall",
    "List active memories in the current scope, newest first",
    &[RECALL_COMMAND],
    &[
        JSON_FLAG,
        DB_FLAG,
        RECALL_LIMIT_FLAG,
        RECALL_TASK_FLAG,
        RECALL_ALL_SCOPES_FLAG,
    ],
)
.with_examples(&[
    "mnene --json recall",
    "mnene --json recall --limit 50 --task rollout",
    "mnene --json recall --all-scopes",
])
.with_output(
    "text mode prints one metadata-and-body block per memory (the same \
     shape as get), newest first, separated by a line of \"---\"; --json \
     prints an array of the same object shape get returns. An empty result \
     is an empty array or no output, not an error.",
)
.with_constraints(
    "only active memories are returned, never superseded or retracted \
     ones; by default only the resolvable scope is included (requires \
     MNENE_SCOPE or git discovery unless --all-scopes is given); --task \
     narrows to memories recorded under one task and is never inferred \
     from MNENE_TASK -- omitting it returns every task's memories in scope",
)
.with_when_to_use(
    "at the start of a session, to load what earlier sessions in this \
     scope left behind before doing anything else",
)
.with_when_not_to_use(
    "a specific keyword or topic is already known -- use search instead, \
     which ranks by relevance rather than listing everything",
);

/// `overwrite` supersedes an active memory.
const OVERWRITE_CAPABILITY: AgentCapability = AgentCapability::new(
    "overwrite",
    "Supersede an active memory with a new body, retiring the old one and \
     minting a new id for the replacement",
    &[OVERWRITE_COMMAND],
    &[JSON_FLAG, DB_FLAG, OVERWRITE_TAG_FLAG],
)
.with_examples(&[
    "mnene --json overwrite 018f2f5e-1c2e-7c3a-9b2e-1a2b3c4d5e6f \"the pipeline now requires two approvals\"",
    "mnene --json overwrite 018f2f5e-1c2e-7c3a-9b2e-1a2b3c4d5e6f \"updated convention text\" --tag conventions",
])
.with_output(
    "text mode prints the new successor memory's id alone on stdout; \
     --json prints {\"id\": \"<uuid>\"} for the successor, pretty-printed. \
     The predecessor's id is unchanged but its state becomes superseded.",
)
.with_constraints(
    "requires MNENE_AGENT (or CLANKER_SESSION_HARNESS) and a scope, exactly \
     as put does; the target id must name a memory currently in the \
     active state -- overwriting an already-superseded or retracted memory \
     fails with a typed NotActive error naming its current state (and \
     successor, if superseded); when --tag is given the successor's tags \
     replace the predecessor's rather than merging with them, otherwise \
     the predecessor's tags carry over",
)
.with_when_to_use(
    "an existing active memory is now wrong or stale and should be \
     replaced with corrected text while preserving the supersession link",
)
.with_when_not_to_use(
    "the new information is additive rather than a correction -- put a new \
     memory instead of overwriting one, since overwrite always retires the \
     predecessor rather than appending to it",
);

/// `retract` retires an active memory with no replacement.
const RETRACT_CAPABILITY: AgentCapability = AgentCapability::new(
    "retract",
    "Retire an active memory with no replacement",
    &[RETRACT_COMMAND],
    &[JSON_FLAG, DB_FLAG],
)
.with_examples(&["mnene --json retract 018f2f5e-1c2e-7c3a-9b2e-1a2b3c4d5e6f"])
.with_output(
    "text mode prints \"retracted <id>\"; --json prints {\"id\": \"<uuid>\", \
     \"state\": \"retracted\"}.",
)
.with_constraints(
    "requires MNENE_AGENT (or CLANKER_SESSION_HARNESS) and a scope, exactly \
     as put and overwrite do; the target id must name a memory currently \
     in the active state -- retracting an already-superseded or retracted \
     memory fails with a typed NotActive error naming its current state",
)
.with_when_to_use(
    "an active memory is no longer true or useful and has no replacement \
     text -- for example a since-abandoned plan",
)
.with_when_not_to_use(
    "there is replacement text for the memory -- use overwrite instead so \
     the supersession link is recorded",
);

/// `scopes` lists every distinct scope value stored.
const SCOPES_CAPABILITY: AgentCapability = AgentCapability::new(
    "scopes",
    "List every distinct scope value stored, with a count and the \
     scope_source values seen, so scopes still named by directory can be \
     found and their repositories registered",
    &[SCOPES_COMMAND],
    &[JSON_FLAG, DB_FLAG],
)
.with_examples(&["mnene --json scopes"])
.with_output(
    "text mode prints one line per scope, \"<scope> <count> \
     [<source>,<source>...]\"; --json prints an array of {\"scope\": \"...\", \
     \"count\": <number>, \"sources\": [...]} objects.",
)
.with_constraints("read-only; covers every lifecycle state, not only active memories")
.with_when_to_use(
    "auditing which scopes are in use and how each was resolved, for \
     example before registering a repository in the project registry",
)
.with_when_not_to_use(
    "the goal is to read or search memories in one scope -- use get, \
     recall, or search instead",
);

/// The complete agent capability surface `mnene` attaches to its
/// [`tftio_lib::ToolSpec`].
pub static SURFACE: AgentSurfaceSpec = AgentSurfaceSpec::new(&[
    PUT_CAPABILITY,
    GET_CAPABILITY,
    SEARCH_CAPABILITY,
    RECALL_CAPABILITY,
    OVERWRITE_CAPABILITY,
    RETRACT_CAPABILITY,
    SCOPES_CAPABILITY,
]);

#[cfg(test)]
mod tests {
    use super::SURFACE;

    /// Exactly the seven declared capabilities, in a stable order, each
    /// named after its command. Also exercises every `with_*` builder this
    /// module calls (`with_examples`, `with_output`, `with_constraints`,
    /// `with_when_to_use`, `with_when_not_to_use`) by checking that none of
    /// them silently dropped their field.
    #[test]
    fn declares_exactly_the_seven_task_level_capabilities() {
        let names: Vec<&str> = SURFACE
            .capabilities()
            .iter()
            .map(tftio_lib::AgentCapability::name)
            .collect();
        assert_eq!(
            names,
            vec![
                "put",
                "get",
                "search",
                "recall",
                "overwrite",
                "retract",
                "scopes"
            ]
        );

        for capability in SURFACE.capabilities() {
            let name = capability.name();
            assert!(capability.summary().is_some(), "{name}");
            assert!(!capability.commands().is_empty(), "{name}");
            let has_examples = capability
                .examples()
                .is_some_and(|examples| !examples.is_empty());
            assert!(has_examples, "{name}");
            assert!(capability.output().is_some(), "{name}");
            assert!(capability.constraints().is_some(), "{name}");
            assert!(capability.when_to_use().is_some(), "{name}");
            assert!(capability.when_not_to_use().is_some(), "{name}");
        }
    }

    /// Every capability allows the global `--json` and `--db` flags with an
    /// empty command path, matching how `src/main.rs` declares both as
    /// `global = true` on the root `Cli`.
    #[test]
    fn every_capability_allows_the_global_json_and_db_flags() {
        for capability in SURFACE.capabilities() {
            let globals: Vec<&str> = capability
                .flags()
                .iter()
                .filter(|flag| flag.command_path().is_empty())
                .map(tftio_lib::FlagSelector::long)
                .collect();
            assert_eq!(globals, vec!["json", "db"], "{}", capability.name());
        }
    }

    /// `put`, `search`, `recall`, and `overwrite` declare exactly the local
    /// flags `src/verb.rs` defines for those commands; `get` and `retract`
    /// declare none beyond the shared globals.
    #[test]
    fn local_flags_match_verb_rs() {
        let local_flags = |name: &str| -> Vec<&str> {
            let capability = SURFACE
                .capabilities()
                .iter()
                .find(|capability| capability.name() == name);
            assert!(capability.is_some(), "capability {name} declared");
            capability.map_or_else(Vec::new, |capability| {
                capability
                    .flags()
                    .iter()
                    .filter(|flag| !flag.command_path().is_empty())
                    .map(tftio_lib::FlagSelector::long)
                    .collect()
            })
        };

        assert_eq!(local_flags("put"), vec!["tag"]);
        assert_eq!(local_flags("get"), Vec::<&str>::new());
        assert_eq!(
            local_flags("search"),
            vec!["limit", "include-superseded", "all-scopes", "tag"]
        );
        assert_eq!(local_flags("recall"), vec!["limit", "task", "all-scopes"]);
        assert_eq!(local_flags("overwrite"), vec!["tag"]);
        assert_eq!(local_flags("retract"), Vec::<&str>::new());
        assert_eq!(local_flags("scopes"), Vec::<&str>::new());
    }
}
