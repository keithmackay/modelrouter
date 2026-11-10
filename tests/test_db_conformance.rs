//! Backend conformance: the same repository scenarios run against every
//! database backend this build supports, so the Postgres implementation is
//! held to the behaviour the SQLite one already has.
//!
//! SQLite always runs. Postgres runs too when the binary is built with
//! `--features postgres` and `MODELROUTER_TEST_POSTGRES_URL` points at a
//! database the tests may create schemas in:
//!
//! ```text
//! MODELROUTER_TEST_POSTGRES_URL=postgres://user:pass@localhost:5438/modelrouter_test \
//!     cargo test --features postgres --test test_db_conformance
//! ```
//!
//! Each test gets its own Postgres schema (dropped when the test ends), so
//! tests run in parallel without seeing each other's rows.

#[path = "db_conformance/harness.rs"]
mod harness;

#[path = "db_conformance/migrations.rs"]
mod migrations;
#[path = "db_conformance/costs.rs"]
mod costs;

#[path = "db_conformance/prompts_experiments.rs"]
mod prompts_experiments;

#[path = "db_conformance/identity.rs"]
mod identity;

#[path = "db_conformance/catalog.rs"]
mod catalog;

#[path = "db_conformance/integrations.rs"]
mod integrations;

#[path = "db_conformance/reports.rs"]
mod reports;
