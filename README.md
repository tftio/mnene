# mnene

An agent-to-agent memory store.

`mnene` lets one harness session leave short, tagged notes for later sessions
working in the same project or task, and lets a later session retrieve them by
id, by keyword search, or in bulk as a startup inheritance (`recall`). It runs
no model of its own: ranking is SQLite FTS5 BM25 over the strings agents chose
to write, and provenance (who wrote a memory, in what context, scope, and
task) comes from the process environment, never from a command argument. It
is not a knowledge base, has no embeddings or reranker, and has no
human-facing browsing or editing surface; a memory's contents are opaque
except through the tool itself.

## Purpose and non-goals

- Agent-to-agent memory only: the writing agent is the only model on the
  write path, and the calling agent is the only model on the read path.
- No embedding model, no network access, and no long-running daemon other
  than the `mcp` stdio loop.
- No paraphrase retrieval: search is keyword-based FTS5, not semantic. Agents
  and a consistent tag vocabulary carry the burden of matching wording.
- No human browsing or editing surface, and no migration path other than the
  planned `dump`/`restore` verbs (not part of this surface).

## Verbs

`mnene` exposes eight subcommands. Every verb accepts `--json` to render
machine-readable JSON instead of human-readable text, and the global `--db`
option, which overrides the default database location (see
[Environment variables](#environment-variables) below). There is no flag for
agent, context, session, or scope: those values are inherited attribution
from the process environment only, never a command argument.

Exit status is 0 on success, 1 on a domain error (`error: <message>` printed
to stderr), and 2 on a usage error (a missing or malformed argument, reported
by the argument parser).

### `put BODY [--tag TAG]...`

Stores a new memory. `BODY` must be non-blank. Prints the new memory's id.

Text output is the bare id. JSON output is `{"id": "<uuid>"}`.

### `get ID`

Retrieves one memory by id, regardless of scope: an id is a capability. Text
output is a block of `key: value` lines (`id`, `state`, `scope`,
`scope_source`, `task`, `agent`, `context`, `session`, `created_at`,
`closed_at`, `closed_by`, `supersedes`, `superseded_by`, `tags`) followed by a
blank line and the body. Absent fields render as an empty string. JSON output
mirrors the same fields, with absent values as `null` and `tags` as an array:

```json
{
  "id": "01a06d9b-c67a-7d20-b87d-34b28cd8808d",
  "body": "...",
  "state": "superseded",
  "supersedes": null,
  "superseded_by": "01a06d9b-fdb5-77f2-acbc-376ffc0ceebf",
  "provenance": {
    "agent": "test-agent",
    "context": "personal",
    "session": null,
    "scope": "test-scope",
    "scope_source": "remote",
    "task": "t013-docs"
  },
  "tags": ["rust", "unit-test"],
  "created_at": "2026-09-04T18:08:40.570894Z",
  "closed_at": "2026-09-04T18:08:54.709605Z",
  "closed_by": "test-agent"
}
```

`scope_source` names which tier resolved `scope` (see
[Environment variables](#environment-variables)): `env`, `declared`, `path`,
`remote`, `derived`, `clanker`, or `directory`. It is `null` for a memory
written before schema version 2's migration, or when no tier could
resolve a scope for the write that produced it (unreachable for a stored
memory today, since every write verb requires a scope).

Fails with `NotFound` (exit 1) when no memory has the given id.

### `search QUERY [--limit N] [--include-superseded] [--all-scopes] [--tag TAG]...`

Keyword search over memory bodies and tags, bounded to the current scope
unless `--all-scopes` is given. Default limit is 10. `--tag` may be repeated;
when given, a hit must carry every named tag. `--include-superseded` also
returns `superseded` memories; `retracted` memories never appear, with or
without that flag.

Text output is one line per hit: `<id> <score>`, score formatted to four
decimal places. JSON output is an array of `{"id": "...", "score": <number>}`
objects. Score is the negated FTS5 `bm25()` value, so a higher score is a
better match; ties break by `created_at` descending. A query that matches
nothing returns an empty result with exit status 0, not an error.

### `overwrite ID BODY [--tag TAG]...`

Supersedes an `active` memory with a new one carrying `BODY`, and prints the
new memory's id (same output shapes as `put`). Tags are inherited from the
predecessor unless `--tag` is given, in which case the new tag set replaces
them entirely. `ID` must currently be `active`; otherwise the call fails with
`NotActive` naming the memory's current state and, when it has one, the
record that superseded it.

### `retract ID`

Retires an `active` memory with no replacement. `ID` must currently be
`active`, or the call fails with `NotActive`. Text output is
`retracted <id>`; JSON output is `{"id": "<uuid>", "state": "retracted"}`.

### `recall [--limit N] [--task TASK] [--all-scopes]`

Lists `active` memories in scope, newest first, as full records (the same
shape `get` returns for each). Default limit is 20. Without `--all-scopes`,
recall is bounded to the current scope; without `--task`, no task filter is
applied at all, so a fresh session's inheritance is never silently narrowed
to its own task by a value read from the environment. `--task` only narrows
recall when given explicitly.

### `mcp`

Serves the six data verbs above (everything except `mcp` itself) as MCP
tools over the process's own stdin and stdout, reading one JSON-RPC 2.0
message per line until stdin reaches EOF, then exiting 0. It handles
`initialize`, `notifications/initialized`, `ping`, `tools/list`, and
`tools/call`. A domain failure (for example `NotFound` or `NotActive`) comes
back as an ordinary tool result with `isError: true` and the error's message
as text; a protocol-level failure (malformed JSON, an unknown method, an
unknown tool, or arguments that do not match a tool's input schema) comes
back as a JSON-RPC error object. Tool results carry the same JSON shapes
`--json` produces on the CLI. Provenance for `put`, `overwrite`, and
`retract`, and scope bounding for `search` and `recall`, come only from the
environment `mnene mcp` was launched with, exactly as for the CLI verbs;
there is no per-call override.

### `scopes`

Lists every distinct scope value stored, across every lifecycle state (not
only `active`), with a count of memories and the distinct `scope_source`
values seen for that scope. This is a diagnostic verb, not a data verb: it is
not served by `mcp`, and it needs no resolvable scope of its own to run. It
exists so that rows still named by directory (`scope_source: "directory"`, or
`null` on a memory written before schema version 2) can be found and their
repositories registered in the project registry.

Text output is one line per scope: `<scope> <count> [<source>,<source>...]`.
JSON output is an array of `{"scope": "...", "count": <number>, "sources":
[...]}` objects, `sources` sorted and deduplicated. A store with no memories
yet returns an empty result, not an error.

## Shared CLI commands

`mnene` routes argument parsing, a shared metadata surface, and fatal-error
rendering through `tftio-lib`'s CLI runner (the same runner `prompter` and
`clanker` use), under a `meta` subcommand:

- `mnene meta version [--json]` -- prints the tool's version.
- `mnene meta license` -- prints the tool's license text.
- `mnene meta completions <shell>` -- generates a shell completion script for
  `bash`, `elvish`, `fish`, `powershell`, or `zsh`.
- `mnene meta doctor [--json]` -- runs health checks, including a
  `mnene`-specific check that exercises the bundled SQLite engine and FTS5
  virtual tables against an **in-memory** database created for the check
  alone. `meta doctor` never creates, opens, or modifies the configured
  persistent database (see [Environment variables](#environment-variables)).
- `mnene meta agent list` -- lists the task-level capabilities this build
  declares for supervised agent use (`put`, `get`, `search`, `recall`,
  `overwrite`, `retract`, `scopes`); `meta` and `mcp` are deliberately
  absent, since `meta` is the inspection surface itself and `mcp` is
  transport bootstrap rather than a task-level operation.
- `mnene meta agent describe NAME --format skill-md` -- renders one
  capability's full description, including its allowed flags, examples,
  output shape, and constraints, as a skill document. For example, `mnene
  meta agent describe put --format skill-md` renders the `put` capability.
- `mnene meta agent emit-skills --target claude --out DIR` (or `--target
  codex --out DIR`) -- writes one `SKILL.md` per declared capability under
  `DIR`, using the named runtime's directory layout. `--out` is required in
  every example here and writes only under the directory given; there is a
  separate `--install` flag that writes into a real `~/.claude/skills` or
  `~/.codex` layout, but no example in this README uses it, and doing so is
  a deliberate operator action outside normal `mnene` usage.

Every `meta` subcommand also accepts the global `--json` flag. Under
`--json`, a fatal CLI error -- one that would otherwise print `error:
<message>` to stderr -- instead prints a structured JSON error envelope to
**stdout**, for example:

```json
{"command":"get","error":{"code":"ERROR","details":{},"message":"memory <id> not found"},"ok":false}
```

Exit status remains 1 either way; this only changes the shape and stream of
the failure output when `--json` is given, and applies to every command,
`meta` included. It does not change the successful-output shapes for the
eight domain verbs, which are unaffected by this shared envelope (see
[Verbs](#verbs) above).

### Supervised agent mode

When the process environment carries a `TFTIO_AGENT_TOKEN` value equal to
`TFTIO_AGENT_TOKEN_EXPECTED`, `mnene` is running under agent supervision: the
visible command surface narrows to exactly the seven declared capabilities
above (`put`, `get`, `search`, `recall`, `overwrite`, `retract`, `scopes`)
plus their declared flags, and `meta` and `mcp` are both refused. `--agent-help` prints
that filtered surface as structured plain text. Outside supervision (no
matching token pair), the full command surface -- including `meta` and
`mcp` -- is available as usual.

## Environment variables

`agent`, `context`, `session`, `scope`, and (on write verbs) `task` are
stored provenance: attribution inherited from the process environment the
`mnene` process was launched with, not an authenticated identity. `mnene`
does not verify or prove who set these variables; a caller that controls the
environment of the child process it launches `mnene` in can supply whatever
values it likes, exactly as it can for any other environment variable. These
values only prevent an ordinary command-line flag from accidentally
overriding inherited attribution, because there is no such flag to begin
with (`--db` is the sole exception, since it names a local resource rather
than asserting provenance).

Every value below is resolved once, at the process edge: the five `MNENE_*`
provenance fields, the four `CLANKER_SESSION_*` fallbacks, `XDG_DATA_HOME`,
and `XDG_CONFIG_HOME` are read from the environment there; `--db` itself is
parsed by `clap` alongside the rest of the command line and merged into the
same snapshot immediately afterward, so the table's precedence order holds
regardless of which of the two ways a value arrived. Resolution order, most
specific first:

| Field | Source, in order |
|---|---|
| `agent` | `MNENE_AGENT`, else `CLANKER_SESSION_HARNESS`, else `MissingAgent` on write verbs (`put`, `overwrite`, `retract`) |
| `context` | `MNENE_CONTEXT`, else `CLANKER_SESSION_CONTEXT`, else `default` |
| `session` | `MNENE_SESSION`, else `CLANKER_SESSION_ID`, else absent |
| `scope` | `MNENE_SCOPE`, else `tftio_lib::project` resolution (below), else `CLANKER_SESSION_PROJECT` (non-empty), else the name of the git repository containing the working directory, else `MissingScope` on `put`, `overwrite`, and scoped `search`/`recall` |
| `task` | `MNENE_TASK`, else the current git branch, else absent |
| `db` | `--db`, else `MNENE_DB`, else `$XDG_DATA_HOME/mnene/<context>.db`, else `$HOME/.local/share/mnene/<context>.db`, else `<cwd>/.mnene/<context>.db` when none of those is set |

### Scope names the project

`scope` names the project the working directory belongs to, not the
repository or the worktree.
Per invocation, `mnene` tries each tier below in order and stops at the
first one that answers; which tier answered is recorded alongside the value
as `scope_source` (`env`, `declared`, `path`, `remote`, `derived`, `clanker`,
or `directory`, matching the row names below) and stored with every write.

1. **`env`** -- an explicit `MNENE_SCOPE`.
2. **`tftio_lib::project` resolution** against the registry installed at
   `${XDG_CONFIG_HOME:-~/.config}/tftio/projects.toml` (plus a machine-local
   `projects.local.toml` overlay), applied in this order:
   - **`declared`** -- a `project = "<slug>"` key in the `.clanker` at the
     working directory's project root (the repository top level inside a
     repository, the nearest ancestor declaring one outside of it).
   - **`path`** -- a registry `paths` entry that is the working directory or
     an ancestor of it (the most specific match wins).
   - **`remote`** -- the working directory's git origin remote, normalized
     and looked up among registry `remotes`.
   - **`derived`** -- a slug derived from an origin remote that has no
     registry entry (its last path segment, lowercased, non-alphanumeric
     runs collapsed to one hyphen).
3. **`clanker`** -- a non-empty `CLANKER_SESSION_PROJECT`, clanker's own
   session marker. clanker never sets `MNENE_SCOPE` itself, so this is a
   distinct tier rather than a second copy of the `env` value.
4. **`directory`** -- the name of the git repository containing the working
   directory, from its *common* git directory (the directory shared by every
   worktree): a repository whose common
   directory is `<repo>/.git` is named by the directory holding it, and a
   conventional bare clone is named by its own directory with the `.git`
   suffix removed. This is the last resort, so a repository with no remote
   and no registered path resolves to its repository name.

A registry that fails to load (missing permissions, malformed TOML) is
reported loudly on stderr and treated as empty, so a broken registry
degrades resolution -- falling through to `clanker` or `directory` -- rather
than failing the command outright.

Every tier above resolves per command, never once per session: a directory
change between two `mnene` invocations can change `scope`, exactly as it
always could through git discovery. Two linked worktrees of one repository
still resolve to the same scope, whichever tier answers, because a
repository's `.clanker`, its registered remote or path, and its common git
directory are all shared across its worktrees; `task` -- the branch, read
per worktree -- still distinguishes them, and `recall --task` filters on it.
Memories written before `scope_source` was introduced (or before a repository is registered) may
carry a worktree- or directory-derived scope, distinguishable by
`scope_source` and still reachable with `--all-scopes`; `mnene scopes` lists
every distinct scope in a database along with the sources seen, so such rows
can be found and their repositories registered.

`git` discovery (the `directory` tier and `task`'s branch) walks up from the
current directory looking for a `.git` entry (a directory for an ordinary
repository, or a `gitdir:` file for a linked worktree) and reads `commondir`
and `HEAD` directly, without shelling out to `git`. A detached `HEAD` yields
no task; a directory outside any git repository yields no `directory` tier
and no task.

`recall`'s own `--task` flag is a separate, local filter (absent by default)
and is not part of this table; it never falls back to the `task` resolved
above, so a session's own task never hides other tasks' memories from its
inheritance. Passing `--task` to `recall` narrows which stored memories come
back; it never changes what any memory's own stored `task` provenance is.

## States and supersession

Every memory is `active`, `superseded`, or `retracted`. Rows are otherwise
immutable: only `state`, `superseded_by`, `closed_at`, and `closed_by` ever
change after a memory is written.

- `overwrite` and `retract` both require the target to be `active`. Each
  runs the state change as one compare-and-supersede update inside an
  immediate SQLite transaction, changing exactly one row when it applies.
- On `overwrite`, the old id transitions to `superseded` with
  `superseded_by` set to the new memory's id; the new memory carries
  `supersedes` pointing back at the old one.
- On `retract`, the id transitions to `retracted` with no successor.
- When two writers race to overwrite or retract the same id, exactly one
  succeeds. Every other caller receives `NotActive { id, state,
  superseded_by }`, naming the record's current state and, once one exists,
  the id that won.
- `get` on a `superseded` or `retracted` id still succeeds and reports the
  state and (for `superseded`) the successor id; it is never an error to
  fetch an old id.
- `search` without `--include-superseded`, and `recall`, only ever return
  `active` memories. `search --include-superseded` additionally returns
  `superseded` memories. `retracted` memories never appear in `search` or
  `recall`, regardless of flags.
- Once `superseded` or `retracted`, a memory can never be overwritten or
  retracted again.

## Search semantics

`search` builds a SQLite FTS5 query from the input text: each
whitespace-and-punctuation-delimited token (splitting on every
non-alphanumeric character, not only whitespace, so a kebab-case tag matches
the terms FTS5 indexes) becomes a double-quoted prefix term, and the terms
are joined conjunctively, so every token must match as a prefix of some
indexed word. No user input can produce an FTS5 syntax error; a query with
no tokens at all (empty, or made only of punctuation) returns no hits rather
than failing. Ranking is BM25, negated so a higher score is a better match,
with ties broken by `created_at` descending. `--tag` requires every named
tag to be present, not any one of them. Results are bounded to the current
scope unless `--all-scopes` is given.

## Registering `mnene mcp` with a harness

`mnene mcp` speaks MCP over stdio, so it can be registered with any harness
that launches a JSON-RPC stdio server and sets its environment. A generic
configuration entry looks like:

```json
{
  "mcpServers": {
    "mnene": {
      "command": "mnene",
      "args": ["mcp"],
      "env": {
        "MNENE_AGENT": "my-harness",
        "MNENE_CONTEXT": "personal",
        "MNENE_SESSION": "session-id-if-known",
        "MNENE_SCOPE": "project-name",
        "MNENE_TASK": "branch-or-ticket-if-known",
        "MNENE_DB": "/path/to/context.db"
      }
    }
  }
}
```

The exact keys and location of this configuration are specific to each
harness; adapt the `env` block to however that harness passes environment
variables to a launched server. `mnene mcp` takes no per-call provenance
argument: every tool call it serves is attributed using only the environment
the server process was started with, so a harness that switches agent,
scope, or task must restart the server with new values.

## Storage model

Each context has exactly one SQLite database file, at the path resolved
above. The file uses WAL journaling, carries `user_version = 2`, and holds the
`memories`, `tags`, and `memories_fts` (FTS5) tables. Opening a database
whose `user_version` is neither the current version nor one version behind it
is a `SchemaVersion` error. A fresh path is initialized with the current
schema on first use.

`user_version 1` databases (every one created before `scope_source` was
introduced) migrate to `user_version 2` in place on open: `ALTER
TABLE memories ADD COLUMN scope_source` adds the nullable column this
version introduces. No row is deleted or rescoped by this migration -- every
migrated row's `scope_source` is `NULL` until it is next superseded, which is
what `get`, `recall`, and `mnene scopes` render as an absent source.

# Getting started

Toolchain, task execution, and hook tools are managed by mise.
The Rust toolchain is declared in `mise.toml`. `rust-toolchain.toml` is derived
from that declaration by `mise run update` and checked against it by
`mise run check:locks`, so rustup, rust-analyzer, and your IDE resolve the same
pin mise does. Never edit it by hand.
Entering the directory does not prepare, install, or regenerate anything: setup is
explicit, so no lockfile ever changes because someone walked into the repository.

```sh
mise trust --quiet
mise install
mise run setup:idea     # optional; regenerates the gitignored .idea/
```

Tools come from `mise activate <shell>` in an interactive shell, and from mise shims
for non-interactive processes such as editors and coding agents.

## Tasks

```sh
mise run check  # check-only hooks, as CI runs them
mise run lint   # manual autofix hooks
mise run test   # test suite
mise run ci     # full CI gate
```

Dependencies move on one deliberate command, and never on their own:

```sh
mise run update       # mise tools, cargo crates, prek hooks
mise run check:locks  # read-only; fails if a lockfile is stale
```

The generated Rust gate includes formatting, TOML formatting, shell linting,
spelling, clippy, nextest, docs, unused-dependency detection, advisory audit,
license/source policy, packaging, and complete line coverage.
