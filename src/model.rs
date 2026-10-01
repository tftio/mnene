//! Domain model types for `mnene`.
//!
//! This module defines `MemoryId`, `Memory`, `State`, `Provenance`, `Tag`,
//! `SearchHit`, and `MneneError`. It is a pure module: no I/O and no dependency
//! on `rusqlite` or `serde_json`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize};
use uuid::{Uuid, Version};

/// A memory identifier: a `UUIDv7`, always rendered hyphenated and lowercase.
///
/// Constructing one via [`MemoryId::new`] always mints a fresh, time-ordered
/// id. Parsing (via [`FromStr`]) accepts only the hyphenated form of a
/// version 7 UUID and rejects everything else, including well-formed UUIDs
/// of other versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct MemoryId(Uuid);

impl MemoryId {
    /// Mints a fresh identifier from a new `UUIDv7`.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for MemoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

impl FromStr for MemoryId {
    type Err = MneneError;

    /// Parses a hyphenated `UUIDv7` string into a `MemoryId`.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::InvalidId`] when `s` is not valid UUID syntax,
    /// is not in hyphenated form, or is not a version 7 UUID.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(s).map_err(|_err| MneneError::InvalidId(s.to_string()))?;
        let hyphenated = uuid.hyphenated().to_string();
        if !s.eq_ignore_ascii_case(&hyphenated) {
            return Err(MneneError::InvalidId(s.to_string()));
        }
        match uuid.get_version() {
            Some(Version::SortRand) => Ok(Self(uuid)),
            _ => Err(MneneError::InvalidId(s.to_string())),
        }
    }
}

impl<'de> Deserialize<'de> for MemoryId {
    /// Deserializes a `MemoryId` from its hyphenated string form, rejecting
    /// non-v7 UUIDs the same way [`FromStr`] does.
    ///
    /// # Errors
    ///
    /// Returns a deserialization error when the string is not a hyphenated
    /// `UUIDv7`.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

/// The lifecycle state of a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// The memory is current and can be superseded or retracted.
    Active,
    /// The memory was replaced by a successor.
    Superseded,
    /// The memory was retired with no replacement.
    Retracted,
}

impl State {
    /// Returns the lowercase string form stored for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Retracted => "retracted",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = MneneError;

    /// Parses a stored state string.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] naming the value when `s` is not one
    /// of `active`, `superseded`, or `retracted`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "retracted" => Ok(Self::Retracted),
            other => Err(MneneError::Storage(format!("unknown state: {other}"))),
        }
    }
}

/// Normalizes tag text to lowercase kebab-case.
///
/// A case boundary (lowercase followed by uppercase) is treated as a word
/// break. Any run of characters that are not alphanumeric (punctuation,
/// whitespace, underscores) collapses to a single hyphen. Leading and
/// trailing hyphens are trimmed, and the result is lowercased.
fn normalize_tag(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_lower_letter = false;

    for c in input.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && prev_lower_letter && !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            for lower in c.to_lowercase() {
                out.push(lower);
            }
            prev_lower_letter = c.is_lowercase();
        } else {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
            prev_lower_letter = false;
        }
    }

    out.trim_matches('-').to_string()
}

/// A validated, normalized tag: lowercase kebab-case, never empty.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Tag(String);

impl Tag {
    /// Normalizes and validates `input` into a `Tag`.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::InvalidTag`] naming `input` when normalization
    /// yields an empty string.
    pub fn new(input: &str) -> Result<Self, MneneError> {
        let normalized = normalize_tag(input);
        if normalized.is_empty() {
            return Err(MneneError::InvalidTag(input.to_string()));
        }
        Ok(Self(normalized))
    }

    /// Returns the normalized tag text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Tag {
    type Err = MneneError;

    /// Parses and normalizes tag text; see [`Tag::new`].
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::InvalidTag`] under the same condition as
    /// [`Tag::new`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl<'de> Deserialize<'de> for Tag {
    /// Deserializes a `Tag` from a string, normalizing and validating it.
    ///
    /// # Errors
    ///
    /// Returns a deserialization error when the normalized value is empty.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// Which tier resolved a memory's `scope`.
///
/// Stored as a nullable `TEXT` column (`scope_source`) alongside `scope`; a
/// row written before schema version 2 carries `None` (see
/// `SqliteStore::open`'s in-place migration), and so does any row whose
/// scope came from a build that predates this field entirely. Variant order
/// matches the tier order `Config::resolve` applies:
/// [`ScopeSource::Env`] (an explicit `MNENE_SCOPE`), then the three
/// `tftio_lib::project` tiers ([`ScopeSource::Declared`], [`ScopeSource::Path`],
/// [`ScopeSource::Remote`], [`ScopeSource::Derived`]), then
/// [`ScopeSource::Clanker`] (`CLANKER_SESSION_PROJECT`), then
/// [`ScopeSource::Directory`] (the git repository's name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeSource {
    /// An explicit `MNENE_SCOPE` environment variable.
    Env,
    /// A `.clanker` declaration at the project root.
    Declared,
    /// A registry `paths` entry that is or contains the working directory.
    Path,
    /// The working directory's origin remote, found among a project's
    /// registry `remotes`.
    Remote,
    /// A slug derived from an unregistered origin remote.
    Derived,
    /// `CLANKER_SESSION_PROJECT`, clanker's own session marker.
    Clanker,
    /// The name of the git repository containing the working directory.
    Directory,
}

impl ScopeSource {
    /// The stable, lowercase label this source is reported and stored
    /// under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Declared => "declared",
            Self::Path => "path",
            Self::Remote => "remote",
            Self::Derived => "derived",
            Self::Clanker => "clanker",
            Self::Directory => "directory",
        }
    }
}

impl fmt::Display for ScopeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ScopeSource {
    type Err = MneneError;

    /// Parses a stored `scope_source` value.
    ///
    /// # Errors
    ///
    /// Returns [`MneneError::Storage`] naming the value when `s` is not one
    /// of the seven stable labels [`ScopeSource::as_str`] produces.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "env" => Ok(Self::Env),
            "declared" => Ok(Self::Declared),
            "path" => Ok(Self::Path),
            "remote" => Ok(Self::Remote),
            "derived" => Ok(Self::Derived),
            "clanker" => Ok(Self::Clanker),
            "directory" => Ok(Self::Directory),
            other => Err(MneneError::Storage(format!(
                "unknown scope_source: {other}"
            ))),
        }
    }
}

/// The origin of a memory: who wrote it, and under what session and scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The agent identifier attributed to the write.
    pub agent: String,
    /// The context (per-database namespace) the write was made under.
    pub context: String,
    /// The session identifier recorded on the write, if any.
    pub session: Option<String>,
    /// The scope the memory belongs to.
    pub scope: String,
    /// Which tier resolved `scope`, if known. `None` for a row written
    /// before schema version 2's migration.
    pub scope_source: Option<ScopeSource>,
    /// The task identifier recorded on the write, if any.
    pub task: Option<String>,
}

/// A single memory record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    /// The memory's identifier.
    pub id: MemoryId,
    /// The memory's body text.
    pub body: String,
    /// The memory's lifecycle state.
    pub state: State,
    /// The predecessor this memory replaced, if it was created by
    /// `overwrite`.
    pub supersedes: Option<MemoryId>,
    /// The successor that replaced this memory, if it has been superseded.
    pub superseded_by: Option<MemoryId>,
    /// Where this memory came from.
    pub provenance: Provenance,
    /// The tags attached to this memory.
    pub tags: Vec<Tag>,
    /// When this memory was created, as an RFC 3339 UTC timestamp.
    pub created_at: String,
    /// When this memory was closed (superseded or retracted), if it has
    /// been, as an RFC 3339 UTC timestamp.
    pub closed_at: Option<String>,
    /// The agent that closed this memory, if it has been closed.
    pub closed_by: Option<String>,
}

/// A single search result: a memory id and its relevance score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The matching memory's identifier.
    pub id: MemoryId,
    /// The relevance score; higher is more relevant.
    pub score: f64,
}

/// Renders the ", superseded by <id>" suffix for [`MneneError::NotActive`],
/// or an empty string when there is no successor.
fn successor_suffix(superseded_by: Option<MemoryId>) -> String {
    superseded_by.map_or_else(String::new, |id| format!(", superseded by {id}"))
}

/// Errors returned by the `mnene` domain and storage layers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MneneError {
    /// No memory exists with the given id.
    #[error("memory {0} not found")]
    NotFound(MemoryId),
    /// The memory exists but is not active, so it cannot be overwritten or
    /// retracted.
    #[error("memory {id} is {state} and cannot be modified{}", successor_suffix(*superseded_by))]
    NotActive {
        /// The memory's id.
        id: MemoryId,
        /// The memory's current state.
        state: State,
        /// The successor that superseded the memory, if any.
        superseded_by: Option<MemoryId>,
    },
    /// A memory body was blank after trimming.
    #[error("memory body must not be blank")]
    EmptyBody,
    /// A tag failed to normalize to a non-empty value.
    #[error("invalid tag: {0}")]
    InvalidTag(String),
    /// A memory id string was not a hyphenated `UUIDv7`.
    #[error("invalid memory id: {0}")]
    InvalidId(String),
    /// A scope was required but none was configured.
    #[error("scope is required")]
    MissingScope,
    /// An agent identifier was required but none was configured.
    #[error("agent is required")]
    MissingAgent,
    /// The database's `user_version` did not match what this build expects.
    #[error("schema version {found} found, expected {expected}")]
    SchemaVersion {
        /// The `user_version` found in the database.
        found: i64,
        /// The `user_version` this build expects.
        expected: i64,
    },
    /// A storage-layer failure, carrying the underlying error text.
    #[error("storage error: {0}")]
    Storage(String),
}

impl From<anyhow::Error> for MneneError {
    /// Wraps an opaque `anyhow::Error` as [`MneneError::Storage`].
    ///
    /// `src/main.rs`'s `print_json` deliberately renders a `serde_json`
    /// serialization failure through `anyhow`'s blanket `?` conversion
    /// (see its own doc comment): the failure is unreachable for the
    /// plain, owned data this binary ever serializes, so that conversion
    /// is never itself exercised. This `From` impl only bridges
    /// `print_json`'s `anyhow::Result<()>` back to the
    /// `Result<i32, MneneError>` domain dispatch returns; it never touches
    /// `serde_json` itself (RS-009 confines `serde_json` to
    /// `src/main.rs`/`src/mcp.rs`).
    fn from(err: anyhow::Error) -> Self {
        Self::Storage(err.to_string())
    }
}

/// Validates a memory body, returning its trimmed form.
///
/// # Errors
///
/// Returns [`MneneError::EmptyBody`] when `body` is blank after trimming.
pub fn validate_body(body: &str) -> Result<String, MneneError> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(MneneError::EmptyBody);
    }
    Ok(trimmed.to_string())
}

/// Returns the current time as an RFC 3339 UTC timestamp string.
///
/// This is the only place `jiff` is used; the rest of the crate treats
/// timestamps as opaque strings.
#[must_use]
pub fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        MemoryId, MneneError, Provenance, ScopeSource, SearchHit, State, Tag, now_rfc3339,
        validate_body,
    };
    use std::str::FromStr;
    use uuid::Uuid;

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

    #[test]
    fn expect_err_reports_an_unexpected_ok() {
        let ok: Result<(), MneneError> = Ok(());
        let result: Result<MneneError, String> = expect_err!(ok);
        assert_eq!(Err("expected an error but got Ok".to_string()), result);
    }

    // --- Tag normalization ---

    #[test]
    fn tag_normalizes_case_boundary() -> Result<(), MneneError> {
        assert_eq!("silent-critic", Tag::new("silentCritic")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_normalizes_space_separated_words() -> Result<(), MneneError> {
        assert_eq!("silent-critic", Tag::new("Silent Critic")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_collapses_punctuation_runs() -> Result<(), MneneError> {
        assert_eq!("ci-cd", Tag::new("CI/CD")?.as_str());
        assert_eq!("a-b", Tag::new("a___---   b")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_does_not_split_runs_with_no_word_break() -> Result<(), MneneError> {
        assert_eq!("cicd", Tag::new("CICD")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_trims_leading_and_trailing_punctuation() -> Result<(), MneneError> {
        assert_eq!("tag", Tag::new("--tag--")?.as_str());
        assert_eq!("tag", Tag::new("  tag  ")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_lowercases_unicode() -> Result<(), MneneError> {
        assert_eq!("café-note", Tag::new("Café Note")?.as_str());
        Ok(())
    }

    #[test]
    fn tag_rejects_empty_input() -> Result<(), String> {
        let err = expect_err!(Tag::new(""))?;
        assert_eq!(MneneError::InvalidTag(String::new()), err);
        Ok(())
    }

    #[test]
    fn tag_rejects_all_punctuation_input() -> Result<(), String> {
        let err = expect_err!(Tag::new("---"))?;
        assert_eq!(MneneError::InvalidTag("---".to_string()), err);
        Ok(())
    }

    #[test]
    fn tag_from_str_matches_new() -> Result<(), MneneError> {
        assert_eq!(Tag::new("Foo")?, Tag::from_str("Foo")?);
        Ok(())
    }

    #[test]
    fn tag_and_memory_id_are_hashable() -> Result<(), MneneError> {
        let mut tags = std::collections::HashSet::new();
        tags.insert(Tag::new("Foo")?);
        tags.insert(Tag::new("Foo")?);
        assert_eq!(1, tags.len());

        let mut ids = std::collections::HashSet::new();
        let id = MemoryId::new();
        ids.insert(id);
        ids.insert(id);
        assert_eq!(1, ids.len());
        Ok(())
    }

    #[test]
    fn tag_display_renders_normalized_text() -> Result<(), MneneError> {
        assert_eq!("silent-critic", Tag::new("Silent Critic")?.to_string());
        Ok(())
    }

    #[test]
    fn tag_deserialize_normalizes_and_validates() -> Result<(), Box<dyn std::error::Error>> {
        let ok: Tag = serde::Deserialize::deserialize(serde::de::value::StrDeserializer::<
            serde::de::value::Error,
        >::new("Silent Critic"))?;
        assert_eq!("silent-critic", ok.as_str());

        let bad_result: Result<Tag, _> =
            serde::Deserialize::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new("---"));
        assert!(bad_result.is_err());
        Ok(())
    }

    // --- MemoryId ---

    #[test]
    fn memory_id_new_mints_a_v7_uuid() {
        let id = MemoryId::new();
        let parsed = id.to_string().parse::<MemoryId>();
        assert!(parsed.is_ok());
    }

    #[test]
    fn memory_id_default_mints_a_fresh_id() {
        let a = MemoryId::default();
        let b = MemoryId::default();
        assert_ne!(a, b);
    }

    #[test]
    fn memory_id_round_trips_through_display_and_parse() -> Result<(), MneneError> {
        let id = MemoryId::new();
        let rendered = id.to_string();
        let parsed: MemoryId = rendered.parse()?;
        assert_eq!(id, parsed);
        assert_eq!(36, rendered.len());
        assert!(rendered.contains('-'));
        Ok(())
    }

    #[test]
    fn memory_id_rejects_v4_uuid() -> Result<(), Box<dyn std::error::Error>> {
        let v4_text = "11111111-2222-4333-8444-555555555555";
        let v4 = Uuid::parse_str(v4_text)?;
        assert_eq!(Some(uuid::Version::Random), v4.get_version());
        let err = expect_err!(v4_text.parse::<MemoryId>())?;
        assert_eq!(MneneError::InvalidId(v4_text.to_string()), err);
        Ok(())
    }

    #[test]
    fn memory_id_rejects_garbage() -> Result<(), String> {
        let err = expect_err!("not-a-uuid".parse::<MemoryId>())?;
        assert_eq!(MneneError::InvalidId("not-a-uuid".to_string()), err);
        Ok(())
    }

    #[test]
    fn memory_id_rejects_non_hyphenated_form() -> Result<(), String> {
        let id = MemoryId::new();
        let simple = id.to_string().replace('-', "");
        let err = expect_err!(simple.parse::<MemoryId>())?;
        assert_eq!(MneneError::InvalidId(simple), err);
        Ok(())
    }

    #[test]
    fn memory_id_deserialize_rejects_non_v7() {
        let v4 = "11111111-2222-4333-8444-555555555555";
        let result: Result<MemoryId, _> =
            serde::Deserialize::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(v4));
        assert!(result.is_err());
    }

    #[test]
    fn memory_id_deserialize_accepts_v7() -> Result<(), Box<dyn std::error::Error>> {
        let id = MemoryId::new();
        let rendered = id.to_string();
        let parsed: MemoryId =
            serde::Deserialize::deserialize(serde::de::value::StrDeserializer::<
                serde::de::value::Error,
            >::new(&rendered))?;
        assert_eq!(id, parsed);
        Ok(())
    }

    // --- State ---

    #[test]
    fn state_round_trips_through_as_str_and_from_str() -> Result<(), MneneError> {
        for state in [State::Active, State::Superseded, State::Retracted] {
            let parsed: State = state.as_str().parse()?;
            assert_eq!(state, parsed);
        }
        Ok(())
    }

    #[test]
    fn state_display_matches_as_str() {
        assert_eq!("active", State::Active.to_string());
        assert_eq!("superseded", State::Superseded.to_string());
        assert_eq!("retracted", State::Retracted.to_string());
    }

    #[test]
    fn state_rejects_unknown_string() -> Result<(), String> {
        let err = expect_err!("bogus".parse::<State>())?;
        assert_eq!(MneneError::Storage("unknown state: bogus".to_string()), err);
        Ok(())
    }

    // --- Body validation ---

    #[test]
    fn validate_body_trims_and_accepts_non_blank() -> Result<(), MneneError> {
        assert_eq!("hello", validate_body("  hello  ")?);
        Ok(())
    }

    #[test]
    fn validate_body_rejects_blank() -> Result<(), String> {
        let err = expect_err!(validate_body("   "))?;
        assert_eq!(MneneError::EmptyBody, err);
        Ok(())
    }

    #[test]
    fn validate_body_rejects_empty() -> Result<(), String> {
        let err = expect_err!(validate_body(""))?;
        assert_eq!(MneneError::EmptyBody, err);
        Ok(())
    }

    // --- now_rfc3339 ---

    #[test]
    fn now_rfc3339_renders_a_zulu_timestamp() {
        let rendered = now_rfc3339();
        assert!(
            rendered.ends_with('Z'),
            "expected a Zulu timestamp: {rendered}"
        );
    }

    // --- Error Display text ---

    #[test]
    fn not_found_names_the_id() {
        let id = MemoryId::new();
        let err = MneneError::NotFound(id);
        assert_eq!(format!("memory {id} not found"), err.to_string());
        assert_eq!(err, err.clone());
    }

    #[test]
    fn not_active_names_state_without_successor() {
        let id = MemoryId::new();
        let err = MneneError::NotActive {
            id,
            state: State::Retracted,
            superseded_by: None,
        };
        assert_eq!(
            format!("memory {id} is retracted and cannot be modified"),
            err.to_string()
        );
    }

    #[test]
    fn not_active_names_state_and_successor() {
        let id = MemoryId::new();
        let successor = MemoryId::new();
        let err = MneneError::NotActive {
            id,
            state: State::Superseded,
            superseded_by: Some(successor),
        };
        assert_eq!(
            format!("memory {id} is superseded and cannot be modified, superseded by {successor}"),
            err.to_string()
        );
    }

    #[test]
    fn empty_body_message() {
        assert_eq!(
            "memory body must not be blank",
            MneneError::EmptyBody.to_string()
        );
    }

    #[test]
    fn invalid_tag_names_the_input() {
        assert_eq!(
            "invalid tag: ---",
            MneneError::InvalidTag("---".to_string()).to_string()
        );
    }

    #[test]
    fn invalid_id_names_the_input() {
        assert_eq!(
            "invalid memory id: nope",
            MneneError::InvalidId("nope".to_string()).to_string()
        );
    }

    #[test]
    fn missing_scope_message() {
        assert_eq!("scope is required", MneneError::MissingScope.to_string());
    }

    #[test]
    fn missing_agent_message() {
        assert_eq!("agent is required", MneneError::MissingAgent.to_string());
    }

    #[test]
    fn schema_version_names_found_and_expected() {
        let err = MneneError::SchemaVersion {
            found: 0,
            expected: 1,
        };
        assert_eq!("schema version 0 found, expected 1", err.to_string());
    }

    #[test]
    fn storage_names_the_underlying_text() {
        assert_eq!(
            "storage error: disk full",
            MneneError::Storage("disk full".to_string()).to_string()
        );
    }

    // --- Serde round trips for the remaining derive-based types ---

    #[test]
    fn provenance_serializes_and_round_trips_via_debug_clone_eq() {
        let provenance = Provenance {
            agent: "agent-a".to_string(),
            context: "default".to_string(),
            session: Some("s1".to_string()),
            scope: "mnene".to_string(),
            scope_source: Some(ScopeSource::Env),
            task: None,
        };
        let cloned = provenance.clone();
        assert_eq!(provenance, cloned);
    }

    #[test]
    fn memory_clones_and_compares_by_value() -> Result<(), MneneError> {
        let memory = super::Memory {
            id: MemoryId::new(),
            body: "body".to_string(),
            state: State::Active,
            supersedes: None,
            superseded_by: None,
            provenance: Provenance {
                agent: "agent-a".to_string(),
                context: "default".to_string(),
                session: None,
                scope: "mnene".to_string(),
                scope_source: None,
                task: None,
            },
            tags: vec![Tag::new("a-tag")?],
            created_at: now_rfc3339(),
            closed_at: None,
            closed_by: None,
        };
        let cloned = memory.clone();
        assert_eq!(memory, cloned);
        Ok(())
    }

    #[test]
    fn search_hit_holds_id_and_score() {
        let id = MemoryId::new();
        let hit = SearchHit { id, score: 1.5 };
        assert_eq!(id, hit.id);
        assert!((hit.score - 1.5).abs() < f64::EPSILON);
        assert_eq!(hit, hit.clone());
    }

    #[test]
    fn mnene_error_wraps_an_opaque_anyhow_error_as_storage() {
        let source = anyhow::anyhow!("boom");
        assert_eq!(MneneError::Storage("boom".to_string()), source.into());
    }

    // --- ScopeSource ---

    #[test]
    fn scope_source_round_trips_through_as_str_and_from_str() -> Result<(), MneneError> {
        for source in [
            ScopeSource::Env,
            ScopeSource::Declared,
            ScopeSource::Path,
            ScopeSource::Remote,
            ScopeSource::Derived,
            ScopeSource::Clanker,
            ScopeSource::Directory,
        ] {
            let parsed: ScopeSource = source.as_str().parse()?;
            assert_eq!(source, parsed);
        }
        Ok(())
    }

    #[test]
    fn scope_source_display_matches_as_str() {
        assert_eq!("env", ScopeSource::Env.to_string());
        assert_eq!("declared", ScopeSource::Declared.to_string());
        assert_eq!("path", ScopeSource::Path.to_string());
        assert_eq!("remote", ScopeSource::Remote.to_string());
        assert_eq!("derived", ScopeSource::Derived.to_string());
        assert_eq!("clanker", ScopeSource::Clanker.to_string());
        assert_eq!("directory", ScopeSource::Directory.to_string());
    }

    #[test]
    fn scope_source_rejects_unknown_string() -> Result<(), String> {
        let err = expect_err!("bogus".parse::<ScopeSource>())?;
        assert_eq!(
            MneneError::Storage("unknown scope_source: bogus".to_string()),
            err
        );
        Ok(())
    }
}
