//! CLI verb shapes and the narrow domain-dispatch adapter around them.
//!
//! [`Verb`](crate::verb::Verb) is the clap-derived enum `src/main.rs`'s `Cli` parses its
//! subcommand into. It lives here, in the library crate, rather than in
//! `src/main.rs` for one specific reason: `Verb` must include a `Meta`
//! variant so `mnene meta version`/`license`/`completions`/`doctor`/`agent`
//! parse at all, but `tftio_lib::run_cli_from` always routes a `Meta`
//! command through its shared metadata router *before* the domain runner
//! `src/main.rs` passes it (`run`) is ever called -- so a `Verb::Meta` arm
//! inside that domain runner can never execute in a real invocation.
//! `[[bin]] test = false` in `Cargo.toml` means `src/main.rs` has no unit
//! tests of its own; every line in it is covered only by spawning the
//! compiled binary from `tests/cli.rs`. A truly unreachable arm written
//! directly in `src/main.rs` would therefore permanently violate RS-007's
//! 100% line coverage floor, and REPO_INVARIANTS.md is explicit that the
//! fix is to restructure the code so the line is testable, not to exclude
//! it.
//!
//! [`ensure_domain_verb`](crate::verb::ensure_domain_verb) is that
//! restructuring: it is the one place that
//! matches every `Verb` variant including `Meta`, and it lives in this
//! library crate where `#[cfg(test)]` unit tests run under `cargo
//! nextest`/`cargo test` and count toward coverage. It converts a `Verb`
//! into a [`DomainVerb`](crate::verb::DomainVerb) -- the same shape minus
//! `Meta` -- so
//! `src/main.rs`'s own dispatch match is exhaustive over the seven domain
//! verbs and `Mcp` only, with no dead arm of its own.
//!
//! [`wrap_fatal`](crate::verb::wrap_fatal) plays the same role for a second
//! recurring shape: mapping
//! a domain or I/O error into a [`tftio_lib::FatalCliError`] carrying the
//! right command label. It returns the mapping closure rather than being a
//! closure written inline at each `src/main.rs` call site, so the closure
//! body -- and the question of whether it actually runs -- lives here too.

use clap::Subcommand;
use tftio_lib::{FatalCliError, JsonOutput};

/// The eight verbs `mnene` serves, plus the shared `tftio-lib` metadata
/// surface.
#[derive(Debug, Subcommand)]
pub enum Verb {
    /// Store a new memory and print its id.
    Put {
        /// The memory text to store; must be non-blank.
        body: String,
        /// A tag to attach; may be given more than once.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Retrieve a memory by id.
    Get {
        /// The memory id to fetch.
        id: String,
    },
    /// Search memories in scope by keyword.
    Search {
        /// The search query text.
        query: String,
        /// Maximum number of hits to return.
        #[arg(long, default_value_t = 10)]
        limit: u32,
        /// Include superseded memories in results.
        #[arg(long)]
        include_superseded: bool,
        /// Search every scope instead of only the current one.
        #[arg(long)]
        all_scopes: bool,
        /// Require this tag; may be given more than once.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Supersede an active memory with a new body.
    Overwrite {
        /// The id of the memory to supersede; must be `active`.
        id: String,
        /// The new memory text.
        body: String,
        /// A tag for the successor; may be given more than once. Replaces
        /// the predecessor's tags when given; inherited otherwise.
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Retire an active memory with no replacement.
    Retract {
        /// The id of the memory to retract; must be `active`.
        id: String,
    },
    /// List active memories in scope, newest first.
    Recall {
        /// Maximum number of memories to return.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Restrict to memories recorded under this task.
        #[arg(long)]
        task: Option<String>,
        /// Recall across every scope instead of only the current one.
        #[arg(long)]
        all_scopes: bool,
    },
    /// Serve the six data verbs as MCP tools over stdio until EOF.
    Mcp,
    /// List every distinct scope value stored, with a count and the
    /// `scope_source` values seen.
    Scopes,
    /// Shared `tftio-lib` metadata commands: `version`, `license`,
    /// `completions`, `doctor`, and `agent`.
    ///
    /// Always intercepted by the shared metadata router before reaching
    /// domain dispatch; see the module documentation.
    Meta {
        /// The metadata subcommand to route.
        #[command(subcommand)]
        command: tftio_lib::MetaCommand,
    },
}

/// The eight verbs `mnene` serves, without the shared `Meta` surface.
///
/// The exact shape of [`Verb`] minus its `Meta` variant, produced by
/// [`ensure_domain_verb`] so that domain dispatch can match exhaustively
/// with no arm for a command the shared metadata router already handled.
#[derive(Debug, PartialEq, Eq)]
pub enum DomainVerb {
    /// Store a new memory and print its id.
    Put {
        /// The memory text to store; must be non-blank.
        body: String,
        /// Tags to attach.
        tags: Vec<String>,
    },
    /// Retrieve a memory by id.
    Get {
        /// The memory id to fetch.
        id: String,
    },
    /// Search memories in scope by keyword.
    Search {
        /// The search query text.
        query: String,
        /// Maximum number of hits to return.
        limit: u32,
        /// Include superseded memories in results.
        include_superseded: bool,
        /// Search every scope instead of only the current one.
        all_scopes: bool,
        /// Required tags.
        tags: Vec<String>,
    },
    /// Supersede an active memory with a new body.
    Overwrite {
        /// The id of the memory to supersede; must be `active`.
        id: String,
        /// The new memory text.
        body: String,
        /// Tags for the successor.
        tags: Vec<String>,
    },
    /// Retire an active memory with no replacement.
    Retract {
        /// The id of the memory to retract; must be `active`.
        id: String,
    },
    /// List active memories in scope, newest first.
    Recall {
        /// Maximum number of memories to return.
        limit: u32,
        /// Restrict to memories recorded under this task.
        task: Option<String>,
        /// Recall across every scope instead of only the current one.
        all_scopes: bool,
    },
    /// Serve the six data verbs as MCP tools over stdio until EOF.
    Mcp,
    /// List every distinct scope value stored, with a count and the
    /// `scope_source` values seen.
    Scopes,
}

/// Converts a parsed [`Verb`] into a [`DomainVerb`] for domain dispatch.
///
/// Every domain variant converts one-to-one. A `Meta` command converts to
/// an error instead: `tftio_lib::run_cli_from` always routes `Verb::Meta`
/// through the shared metadata router before the domain runner is called,
/// so this arm is unreachable through the compiled binary. It is covered
/// directly by this module's own `ensure_domain_verb_rejects_meta` unit
/// test instead, per this module's documentation.
///
/// # Errors
///
/// Returns a [`FatalCliError`] labeled `meta` when `verb` is `Verb::Meta`.
pub fn ensure_domain_verb(verb: Verb, output: JsonOutput) -> Result<DomainVerb, FatalCliError> {
    match verb {
        Verb::Put { body, tags } => Ok(DomainVerb::Put { body, tags }),
        Verb::Get { id } => Ok(DomainVerb::Get { id }),
        Verb::Search {
            query,
            limit,
            include_superseded,
            all_scopes,
            tags,
        } => Ok(DomainVerb::Search {
            query,
            limit,
            include_superseded,
            all_scopes,
            tags,
        }),
        Verb::Overwrite { id, body, tags } => Ok(DomainVerb::Overwrite { id, body, tags }),
        Verb::Retract { id } => Ok(DomainVerb::Retract { id }),
        Verb::Recall {
            limit,
            task,
            all_scopes,
        } => Ok(DomainVerb::Recall {
            limit,
            task,
            all_scopes,
        }),
        Verb::Mcp => Ok(DomainVerb::Mcp),
        Verb::Scopes => Ok(DomainVerb::Scopes),
        Verb::Meta { .. } => Err(FatalCliError::new(
            "meta",
            output,
            "a meta command reached domain dispatch instead of the shared metadata router",
        )),
    }
}

/// The stable command label `src/main.rs` uses for [`FatalCliError`]s
/// raised while running `verb`.
#[must_use]
pub const fn domain_label(verb: &DomainVerb) -> &'static str {
    match verb {
        DomainVerb::Put { .. } => "put",
        DomainVerb::Get { .. } => "get",
        DomainVerb::Search { .. } => "search",
        DomainVerb::Overwrite { .. } => "overwrite",
        DomainVerb::Retract { .. } => "retract",
        DomainVerb::Recall { .. } => "recall",
        DomainVerb::Mcp => "mcp",
        DomainVerb::Scopes => "scopes",
    }
}

/// Builds a closure converting any displayable error into a
/// [`FatalCliError`] labeled `label` and rendered per `output`.
///
/// Returning the closure (rather than each `src/main.rs` call site writing
/// its own `.map_err(|err| ...)` literal) keeps the closure's body -- and
/// therefore the question of whether that body ever actually runs -- in
/// this library crate, where it is covered by
/// this module's own `wrap_fatal_renders_the_display_text` unit test
/// regardless of whether any given call site's error ever occurs in
/// practice.
pub fn wrap_fatal<E: std::fmt::Display>(
    label: &'static str,
    output: JsonOutput,
) -> impl FnOnce(E) -> FatalCliError {
    move |err| FatalCliError::new(label, output, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::{DomainVerb, Verb, domain_label, ensure_domain_verb, wrap_fatal};
    use tftio_lib::{JsonOutput, MetaCommand};

    #[test]
    fn ensure_domain_verb_converts_every_domain_variant() {
        let put = ensure_domain_verb(
            Verb::Put {
                body: "b".to_string(),
                tags: vec!["t".to_string()],
            },
            JsonOutput::Text,
        );
        assert!(matches!(put, Ok(DomainVerb::Put { .. })));

        let get = ensure_domain_verb(
            Verb::Get {
                id: "id".to_string(),
            },
            JsonOutput::Text,
        );
        assert!(matches!(get, Ok(DomainVerb::Get { .. })));

        let search = ensure_domain_verb(
            Verb::Search {
                query: "q".to_string(),
                limit: 5,
                include_superseded: true,
                all_scopes: true,
                tags: vec![],
            },
            JsonOutput::Text,
        );
        assert!(matches!(search, Ok(DomainVerb::Search { .. })));

        let overwrite = ensure_domain_verb(
            Verb::Overwrite {
                id: "id".to_string(),
                body: "b".to_string(),
                tags: vec![],
            },
            JsonOutput::Text,
        );
        assert!(matches!(overwrite, Ok(DomainVerb::Overwrite { .. })));

        let retract = ensure_domain_verb(
            Verb::Retract {
                id: "id".to_string(),
            },
            JsonOutput::Text,
        );
        assert!(matches!(retract, Ok(DomainVerb::Retract { .. })));

        let recall = ensure_domain_verb(
            Verb::Recall {
                limit: 5,
                task: Some("t".to_string()),
                all_scopes: false,
            },
            JsonOutput::Text,
        );
        assert!(matches!(recall, Ok(DomainVerb::Recall { .. })));

        let mcp = ensure_domain_verb(Verb::Mcp, JsonOutput::Text);
        assert!(matches!(mcp, Ok(DomainVerb::Mcp)));

        let scopes = ensure_domain_verb(Verb::Scopes, JsonOutput::Text);
        assert!(matches!(scopes, Ok(DomainVerb::Scopes)));
    }

    #[test]
    fn ensure_domain_verb_rejects_meta() {
        let result = ensure_domain_verb(
            Verb::Meta {
                command: MetaCommand::License,
            },
            JsonOutput::Json,
        );

        assert_eq!(
            Err(super::FatalCliError::new(
                "meta",
                JsonOutput::Json,
                "a meta command reached domain dispatch instead of the shared metadata router",
            )),
            result
        );
    }

    #[test]
    fn domain_label_names_every_domain_verb() {
        assert_eq!(
            "put",
            domain_label(&DomainVerb::Put {
                body: String::new(),
                tags: vec![]
            })
        );
        assert_eq!("get", domain_label(&DomainVerb::Get { id: String::new() }));
        assert_eq!(
            "search",
            domain_label(&DomainVerb::Search {
                query: String::new(),
                limit: 0,
                include_superseded: false,
                all_scopes: false,
                tags: vec![]
            })
        );
        assert_eq!(
            "overwrite",
            domain_label(&DomainVerb::Overwrite {
                id: String::new(),
                body: String::new(),
                tags: vec![]
            })
        );
        assert_eq!(
            "retract",
            domain_label(&DomainVerb::Retract { id: String::new() })
        );
        assert_eq!(
            "recall",
            domain_label(&DomainVerb::Recall {
                limit: 0,
                task: None,
                all_scopes: false
            })
        );
        assert_eq!("mcp", domain_label(&DomainVerb::Mcp));
        assert_eq!("scopes", domain_label(&DomainVerb::Scopes));
    }

    #[test]
    fn wrap_fatal_renders_the_display_text() {
        let mapped =
            wrap_fatal::<std::io::Error>("put", JsonOutput::Text)(std::io::Error::other("boom"));

        assert_eq!("put", mapped.command());
        assert_eq!(JsonOutput::Text, mapped.output());
        assert_eq!("boom", mapped.message());
    }
}
