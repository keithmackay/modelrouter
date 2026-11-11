//! Expand-only migration policy (see `migrations/README.md`).
//!
//! A rolling upgrade runs an older binary against a newer schema, and a
//! rollback runs an older binary after the newer one has migrated. Both work
//! only if a migration adds and never takes away. This test reads every
//! migration file in both backend directories and fails on a statement that
//! drops, renames, tightens or retypes, unless the file carries a
//! `-- contract-step: <release> <reason>` line or shipped before the rule.

use std::fs;
use std::path::{Path, PathBuf};

/// Files that shipped before the policy existed and already carry a
/// destructive statement. sqlx checksums whole files, so they cannot be
/// edited to add a marker. Nothing may be added here: a new destructive
/// migration takes a `-- contract-step:` line instead.
const GRANDFATHERED: &[&str] = &[
    "migrations/012_keys_users_refactor.sql",
    "migrations/014_groups.sql",
    "migrations/017_cost_ledger_nullable_prompt.sql",
    "migrations/postgres/012_keys_users_refactor.sql",
    "migrations/postgres/014_groups.sql",
    // The 2026-11-10 PostgreSQL chain repair (037-040) retypes columns the
    // conformance harness found mismatched; it shipped on main before this rule.
    "migrations/postgres/037_prompts_experiments_fixes.sql",
    "migrations/postgres/038_identity_fixes.sql",
    "migrations/postgres/039_catalog_fixes.sql",
    "migrations/postgres/040_integrations_fixes.sql",
];

const MIGRATION_DIRS: &[&str] = &["migrations", "migrations/postgres"];

const MARKER: &str = "-- contract-step:";

/// `-- contract-step: <release> <reason>` on its own line, with both parts.
fn contract_marker(sql: &str) -> Option<&str> {
    sql.lines().map(str::trim).find(|line| {
        line.starts_with(MARKER) && line[MARKER.len()..].split_whitespace().count() >= 2
    })
}

/// Statements with comments stripped, one entry per `;`, whitespace
/// collapsed and upper-cased so the checks below can match on plain text.
fn statements(sql: &str) -> Vec<String> {
    let without_comments: String = sql
        .lines()
        .map(|line| match line.find("--") {
            Some(i) => &line[..i],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    without_comments
        .split(';')
        .map(|s| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_uppercase()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Why a statement breaks an older binary, or `None` when it only expands.
fn contraction(stmt: &str) -> Option<&'static str> {
    if stmt.contains("DROP TABLE") {
        return Some("drops a table");
    }
    if stmt.contains("DROP COLUMN") {
        return Some("drops a column");
    }
    if stmt.contains("TRUNCATE") {
        return Some("truncates a table");
    }
    if stmt.contains(" RENAME ") {
        return Some("renames a table or column");
    }
    if stmt.contains("ALTER COLUMN")
        && !stmt.contains("DROP NOT NULL")
        && !stmt.contains("DROP DEFAULT")
        && !stmt.contains("SET DEFAULT")
    {
        return Some("retypes or tightens a column");
    }
    if stmt.contains("ADD COLUMN") && stmt.contains("NOT NULL") && !stmt.contains("DEFAULT") {
        return Some("adds a NOT NULL column without a default");
    }
    if stmt.contains("ADD CONSTRAINT") && (stmt.contains("CHECK") || stmt.contains("NOT NULL")) {
        return Some("adds a constraint older rows or binaries may violate");
    }
    None
}

fn contractions(sql: &str) -> Vec<String> {
    statements(sql)
        .into_iter()
        .filter_map(|s| contraction(&s).map(|why| format!("{why}: {s}")))
        .collect()
}

fn migration_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in MIGRATION_DIRS {
        for entry in fs::read_dir(root.join(dir)).unwrap_or_else(|e| panic!("{dir}: {e}")) {
            let path = entry.expect("dir entry").path();
            if path.extension().is_some_and(|e| e == "sql") {
                files.push(path);
            }
        }
    }
    files.sort();
    assert!(
        files.len() > 30,
        "found only {} migration files",
        files.len()
    );
    files
}

fn repo_relative(path: &Path) -> String {
    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
        .expect("under the repo")
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn every_migration_is_expand_only_or_marked() {
    let mut violations = Vec::new();
    for path in migration_files() {
        let rel = repo_relative(&path);
        let sql = fs::read_to_string(&path).expect("read migration");
        let found = contractions(&sql);
        if found.is_empty() || GRANDFATHERED.contains(&rel.as_str()) {
            continue;
        }
        if contract_marker(&sql).is_some() {
            continue;
        }
        for f in found {
            violations.push(format!("{rel}: {f}"));
        }
    }
    assert!(
        violations.is_empty(),
        "expand-only policy violated (see migrations/README.md); add the destructive step to a \
         later release with a `-- contract-step: <release> <reason>` line, or make it additive:\n  {}",
        violations.join("\n  ")
    );
}

/// The allowlist may only shrink. Each entry must exist and still be
/// destructive, so a rewritten or deleted file is noticed here.
#[test]
fn grandfathered_files_exist_and_are_still_destructive() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for rel in GRANDFATHERED {
        let sql = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(
            !contractions(&sql).is_empty(),
            "{rel} is no longer destructive; remove it from GRANDFATHERED"
        );
        assert!(
            contract_marker(&sql).is_none(),
            "{rel} carries a marker and needs no grandfathering"
        );
    }
}

#[test]
fn additive_statements_pass() {
    let sql = "
        -- a comment mentioning DROP TABLE must not count
        CREATE TABLE widgets (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
        ALTER TABLE users ADD COLUMN nickname TEXT;
        ALTER TABLE users ADD COLUMN flags INTEGER NOT NULL DEFAULT 0;
        CREATE INDEX IF NOT EXISTS idx_widgets_name ON widgets(name);
        ALTER TABLE cost_ledger ALTER COLUMN prompt_id DROP NOT NULL;
        ALTER TABLE users ALTER COLUMN enabled SET DEFAULT TRUE;
        UPDATE users SET enabled = 1 WHERE enabled IS NULL;
        INSERT INTO app_settings (key, value) VALUES ('x', 'y');
    ";
    assert_eq!(contractions(sql), Vec::<String>::new());
}

#[test]
fn contracting_statements_are_each_named() {
    let cases = [
        ("DROP TABLE users;", "drops a table"),
        ("ALTER TABLE users DROP COLUMN email;", "drops a column"),
        (
            "ALTER TABLE users_new RENAME TO users;",
            "renames a table or column",
        ),
        (
            "ALTER TABLE users RENAME COLUMN name TO full_name;",
            "renames a table or column",
        ),
        (
            "ALTER TABLE users ALTER COLUMN id TYPE BIGINT;",
            "retypes or tightens a column",
        ),
        (
            "ALTER TABLE users ALTER COLUMN email SET NOT NULL;",
            "retypes or tightens a column",
        ),
        (
            "ALTER TABLE users ADD COLUMN tenant TEXT NOT NULL;",
            "adds a NOT NULL column without a default",
        ),
        ("TRUNCATE TABLE audit_log;", "truncates a table"),
        (
            "ALTER TABLE users ADD CONSTRAINT ck CHECK (enabled IN (0,1));",
            "adds a constraint older rows or binaries may violate",
        ),
    ];
    for (sql, why) in cases {
        let found = contractions(sql);
        assert_eq!(found.len(), 1, "{sql}: {found:?}");
        assert!(found[0].starts_with(why), "{sql}: {}", found[0]);
    }
}

#[test]
fn a_contract_marker_needs_a_release_and_a_reason() {
    assert!(contract_marker(
        "-- contract-step: 0.3.0 users.api_key unused since 0.2.0\nDROP TABLE x;"
    )
    .is_some());
    assert!(contract_marker("  -- contract-step: 0.3.0 reason\n").is_some());
    assert!(
        contract_marker("-- contract-step: 0.3.0\nDROP TABLE x;").is_none(),
        "a release alone is not a reason"
    );
    assert!(contract_marker("-- contract step: 0.3.0 reason\n").is_none());
    assert!(
        contract_marker("DROP TABLE x; -- contract-step: 0.3.0 reason").is_none(),
        "marker must be on its own line"
    );
}
