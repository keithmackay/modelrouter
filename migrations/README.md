# Migrations

Two directories, one per backend: `migrations/` for SQLite and
`migrations/postgres/` for PostgreSQL. Each is applied by `sqlx::migrate!`
and tracked in `_sqlx_migrations`. Numbers match across the two where the
same change exists in both.

## Never edit a shipped migration

sqlx checksums the whole file, comments included. A database that already
applied a migration refuses to start if the file changed. Add a new file
instead. (One narrow exception for comment-only edits lives in
`src/db/migrations.rs`; do not reach for it.)

## Expand only

A rolling upgrade runs the previous binary against the new schema until the
last old pod drains. A rollback runs the previous binary against the new
schema for as long as it takes to roll forward again. Both work only if a
migration adds and never takes away.

A migration in the same release that stops using a shape may:

- create a table or index
- add a nullable column, or a `NOT NULL` column with a `DEFAULT`
- loosen a column (`DROP NOT NULL`, `SET DEFAULT`, `DROP DEFAULT`)
- backfill or correct data with `UPDATE`

It may not:

- drop a table or column
- rename a table or column
- change a column's type, or add `NOT NULL` to an existing column
- add a `NOT NULL` column without a default
- add a `CHECK` or `NOT NULL` constraint, or truncate a table

The old shape stays, unused, until a later release. That later release
ships the removal as a **contract step**: a migration whose file carries,
on its own line,

```sql
-- contract-step: <release> <reason>
```

for example `-- contract-step: 0.4.0 users.api_key unread since 0.3.0`. The
release is the first version that may run this file; the reason names what
stopped using the shape and when. A contract step still rolls forward
safely because by then no supported binary reads the old shape.

SQLite cannot drop or retype a column in place on every version it ships
with. The rebuild pattern (`CREATE TABLE x_new`, copy, `DROP TABLE x`,
`RENAME TO x`) is therefore always a contract step, never part of the
release that stops using the column.

## The check

`tests/test_migration_policy.rs` reads every file in both directories and
fails on a contracting statement in a file without a marker. It runs with
`cargo test` and in CI on every pull request. Files that shipped before the
rule and already contract are listed there as grandfathered; that list only
shrinks. Data-only statements (`UPDATE`, `DELETE`, `INSERT`) are not
checked: they cannot break an older binary's queries, though they can lose
data, so review them as carefully.
