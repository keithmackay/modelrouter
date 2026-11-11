# Vendored `sqlx-sqlite` 0.9.0 with a one-line patch

This directory is the `sqlx-sqlite` 0.9.0 crate exactly as published on
crates.io (`sqlx-sqlite-0.9.0.crate`, SHA-256
`488e99c397a62007e4229aec669a179816339afc6d2620ca6fa420dbee2e982c`, the
checksum `Cargo.lock` records for the registry copy), applied through
`[patch.crates-io]` in `../../Cargo.toml`, with ONE change:

- `Cargo.toml`: the `libsqlite3-sys` requirement is `>=0.30.1, <0.40.0`
  instead of `>=0.30.1, <0.38.0`. This is the change upstream made on its
  `main` branch in launchbadge/sqlx#4402 and launchbadge/sqlx#4502 but has
  not published in a release yet.

Why: `libsqlite3-sys` below 0.39.0 bundles a SQLite affected by
CVE-2026-119375 (RUSTSEC-2026-0459: uninitialized page-cache memory returned
when a database is opened beside a crafted `-wal` file). The published sqlx
0.9.0 caps the range below the fixed release, so `cargo update` cannot reach
it without this patch.

Removed from the published tarball because a path dependency does not read
them: `Cargo.lock`, `Cargo.toml.orig`. `LICENSE-MIT` and `LICENSE-APACHE` are
the sqlx workspace's licence texts (copied from the `sqlx` 0.9.0 crate): the
tarball holds only the 14- and 17-byte `../LICENSE-*` link targets that the
workspace layout left behind. Nothing under `src/` is changed; a reviewer can
verify with `diff -r` against the extracted tarball.

The directory is `patches/`, not `vendor/`, so that `cargo vendor` (which
owns `vendor/` and rewrites it for offline builds) never deletes it.

Remove this directory and the `[patch.crates-io]` entries (in this crate and
in every crate that builds it) as soon as a sqlx release of 0.9.1 or later
carries the widened range, then run `cargo update sqlx-sqlite libsqlite3-sys`.
