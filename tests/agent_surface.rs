//! Agent-mode surface coverage for `mnene`.
//!
//! Unsupervised inspection (`meta agent list`/`describe`/`emit-skills`) and
//! supervised filtering (`TFTIO_AGENT_TOKEN` matching
//! `TFTIO_AGENT_TOKEN_EXPECTED`) are tested separately: `emit-skills`
//! deliberately refuses to run while agent supervision is active
//! (`tftio_lib::agent_skill::run_agent_subcommand`), so a single test could never
//! exercise both at once.
//!
//! Every test isolates the process environment exactly as `tests/cli.rs` does:
//! every variable `mnene` itself reads is removed unless a test sets it, `HOME`
//! is always a temporary directory, and no test passes `--install` or writes
//! outside a `tempfile` directory.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Every environment variable `mnene` itself reads (mirrors `tests/cli.rs`'s
/// `CONTROLLED_VARS`), plus the two shared agent-token variables this file
/// drives directly.
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

/// A `Command` for the `mnene` binary with every variable in
/// [`CONTROLLED_VARS`] removed. Callers set whichever of those variables the
/// test needs, and always set `HOME` to a temporary directory even when a
/// command does not need one, so no default-resolution path can reach the
/// real home.
fn base_command() -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("mnene")?;
    for var in CONTROLLED_VARS {
        command.env_remove(var);
    }
    Ok(command)
}

/// A [`base_command`] with matching `TFTIO_AGENT_TOKEN` /
/// `TFTIO_AGENT_TOKEN_EXPECTED` values, activating agent-mode supervision.
fn supervised_command(home: &Path) -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = base_command()?;
    command
        .env("TFTIO_AGENT_TOKEN", "agent-surface-test-token")
        .env("TFTIO_AGENT_TOKEN_EXPECTED", "agent-surface-test-token")
        .env("HOME", home);
    Ok(command)
}

/// A [`base_command`] with no agent tokens set (unsupervised) and `HOME`
/// pinned to a temporary directory so no default resolution reaches the real
/// home.
fn unsupervised_command(home: &Path) -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = base_command()?;
    command.env("HOME", home);
    Ok(command)
}

/// The seven declared capability names, in the order `src/agent_surface.rs`
/// declares them.
const CAPABILITY_NAMES: [&str; 7] = [
    "put",
    "get",
    "search",
    "recall",
    "overwrite",
    "retract",
    "scopes",
];

// --- Unsupervised inspection: `meta agent list` / `describe` / `emit-skills` ---

#[test]
fn meta_agent_list_reports_exactly_the_seven_capabilities() -> TestResult {
    let home = tempfile::tempdir()?;
    let output = unsupervised_command(home.path())?
        .args(["meta", "agent", "list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&output)?;
    let names: Vec<String> = value
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .ok_or("expected a capabilities array")?
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(names, CAPABILITY_NAMES);
    Ok(())
}

#[test]
fn meta_agent_describe_renders_skill_md_for_every_capability() -> TestResult {
    let home = tempfile::tempdir()?;
    for name in CAPABILITY_NAMES {
        let output = unsupervised_command(home.path())?
            .args(["meta", "agent", "describe", name, "--format", "skill-md"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let rendered = String::from_utf8(output)?;
        assert!(
            rendered.starts_with(&format!("---\nname: mnene-{name}\n")),
            "{name}: {rendered}"
        );
        assert!(rendered.contains(&format!("- `mnene {name}`")), "{name}");
        assert!(rendered.contains("## When to use"), "{name}");
        assert!(rendered.contains("## When not to use"), "{name}");
        assert!(rendered.contains("## Output"), "{name}");
        assert!(rendered.contains("## Constraints"), "{name}");

        // The YAML `description:` field is `synthesize_description`'s
        // `[summary, "Use when ...", "Do not use when ..."].join(". ")`: a
        // summary ending in its own period doubles up into "}.. Use when"
        // once joined. Pin that none of the seven declared summaries end
        // with a period by asserting the rendered description line never
        // contains a doubled period.
        let description_line = rendered
            .lines()
            .find(|line| line.starts_with("description:"))
            .ok_or_else(|| format!("{name}: no description: line in {rendered}"))?;
        assert!(
            !description_line.contains(".."),
            "{name}: doubled period in {description_line}"
        );

        // `search` is the one capability whose constraints must be
        // precise about --include-superseded's scope: it is never allowed
        // to imply retracted memories become visible (src/store/search.rs
        // filters them out unconditionally, --include-superseded or not).
        if name == "search" {
            assert!(
                rendered.contains("retracted memories are never returned"),
                "{rendered}"
            );
        }
    }
    Ok(())
}

#[test]
fn emit_skills_writes_seven_claude_skill_files_under_a_temp_dir() -> TestResult {
    let home = tempfile::tempdir()?;
    let out = tempfile::tempdir()?;
    unsupervised_command(home.path())?
        .args([
            "meta",
            "agent",
            "emit-skills",
            "--target",
            "claude",
            "--out",
        ])
        .arg(out.path())
        .assert()
        .success();
    assert_skill_files_written(out.path())
}

#[test]
fn emit_skills_writes_seven_codex_skill_files_under_a_temp_dir() -> TestResult {
    let home = tempfile::tempdir()?;
    let out = tempfile::tempdir()?;
    unsupervised_command(home.path())?
        .args(["meta", "agent", "emit-skills", "--target", "codex", "--out"])
        .arg(out.path())
        .assert()
        .success();
    assert_skill_files_written(out.path())
}

/// Every declared capability wrote `<out>/mnene-<name>/SKILL.md` under `out`.
fn assert_skill_files_written(out: &Path) -> TestResult {
    for name in CAPABILITY_NAMES {
        let path: PathBuf = out.join(format!("mnene-{name}")).join("SKILL.md");
        assert!(path.is_file(), "expected {} to exist", path.display());
        let rendered = std::fs::read_to_string(&path)?;
        assert!(rendered.starts_with(&format!("---\nname: mnene-{name}\n")));
    }
    Ok(())
}

#[test]
fn emit_skills_never_uses_install_or_writes_outside_out() -> TestResult {
    // Documents the invariant the other emit-skills tests rely on: every
    // invocation in this file passes an explicit `--out` and never
    // `--install`, so nothing is written under the temporary `HOME` this
    // test pins, let alone a real skill directory. Runs both targets so
    // neither runtime's default directory is touched.
    let home = tempfile::tempdir()?;
    for target in ["claude", "codex"] {
        let out = tempfile::tempdir()?;
        unsupervised_command(home.path())?
            .args(["meta", "agent", "emit-skills", "--target", target, "--out"])
            .arg(out.path())
            .assert()
            .success();
    }
    let claude_dir = home.path().join(".claude");
    let codex_dir = home.path().join(".codex");
    assert!(
        !claude_dir.exists(),
        "emit-skills must not touch HOME/.claude without --install"
    );
    assert!(
        !codex_dir.exists(),
        "emit-skills must not touch HOME/.codex without --install"
    );
    Ok(())
}

// --- Supervised filtering: matching TFTIO_AGENT_TOKEN values ---

#[test]
fn agent_help_lists_the_seven_capabilities_and_hides_meta_and_mcp() -> TestResult {
    let home = tempfile::tempdir()?;
    let output = supervised_command(home.path())?
        .arg("--agent-help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rendered = String::from_utf8(output)?;
    // `render_agent_help` lists each visible capability as its own
    // `- <name>: <summary>` line; match that shape rather than a bare
    // substring so a name like "get" cannot spuriously match inside
    // unrelated words.
    for name in CAPABILITY_NAMES {
        let prefix = format!("- {name}:");
        assert!(
            rendered.lines().any(|line| line.starts_with(&prefix)),
            "missing {prefix:?} line in:\n{rendered}"
        );
    }
    assert!(
        rendered.lines().all(|line| !line.starts_with("- meta:")),
        "{rendered}"
    );
    assert!(
        rendered.lines().all(|line| !line.starts_with("- mcp:")),
        "{rendered}"
    );
    Ok(())
}

#[test]
fn supervised_put_and_get_parse_with_their_allowed_flags() -> TestResult {
    let home = tempfile::tempdir()?;
    let db = home.path().join("mnene.db");

    let put_output = supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["put", "a memory body", "--tag", "t1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&put_output)?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("put --json did not return an id field")?;

    supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["get", id])
        .assert()
        .success();
    Ok(())
}

#[test]
fn supervised_search_and_recall_parse_with_their_allowed_flags() -> TestResult {
    let home = tempfile::tempdir()?;
    let db = home.path().join("mnene.db");

    supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["put", "a searchable memory", "--tag", "t1"])
        .assert()
        .success();

    supervised_command(home.path())?
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args([
            "search",
            "searchable",
            "--limit",
            "5",
            "--include-superseded",
            "--all-scopes",
            "--tag",
            "t1",
        ])
        .assert()
        .success();

    supervised_command(home.path())?
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["recall", "--limit", "5", "--task", "task-a", "--all-scopes"])
        .assert()
        .success();
    Ok(())
}

#[test]
fn supervised_overwrite_and_retract_parse_with_their_allowed_flags() -> TestResult {
    let home = tempfile::tempdir()?;
    let db = home.path().join("mnene.db");

    let put_output = supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["put", "an overwritable memory"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&put_output)?;
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("put --json did not return an id field")?;

    let overwrite_output = supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["overwrite", id, "an updated memory", "--tag", "t2"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: serde_json::Value = serde_json::from_slice(&overwrite_output)?;
    let new_id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or("overwrite --json did not return an id field")?;

    supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["retract", new_id])
        .assert()
        .success();
    Ok(())
}

#[test]
fn supervised_scopes_parses_with_its_allowed_flags() -> TestResult {
    let home = tempfile::tempdir()?;
    let db = home.path().join("mnene.db");

    supervised_command(home.path())?
        .env("MNENE_AGENT", "agent-a")
        .env("MNENE_SCOPE", "scope-a")
        .args(["--json", "--db"])
        .arg(&db)
        .args(["put", "a memory to be scoped"])
        .assert()
        .success();

    supervised_command(home.path())?
        .args(["--json", "--db"])
        .arg(&db)
        .arg("scopes")
        .assert()
        .success();
    Ok(())
}

#[test]
fn supervised_mode_rejects_meta_and_mcp() -> TestResult {
    let home = tempfile::tempdir()?;

    supervised_command(home.path())?
        .args(["meta", "version"])
        .assert()
        .failure();

    supervised_command(home.path())?
        .arg("mcp")
        .assert()
        .failure();
    Ok(())
}

#[test]
fn supervised_mode_rejects_an_undeclared_flag() -> TestResult {
    let home = tempfile::tempdir()?;
    let db = home.path().join("mnene.db");

    supervised_command(home.path())?
        .env("MNENE_SCOPE", "scope-a")
        .args(["--db"])
        .arg(&db)
        .args(["search", "q", "--nonexistent-flag"])
        .assert()
        .failure();
    Ok(())
}
