# Contributing to modelrouter

## Reporting bugs

Open an issue using the **Bug Report** template. Include the modelrouter version (`modelrouter --version`), your OS, and the smallest config or request that reproduces the problem. If the bug involves a specific provider, redact your API key but include the provider name and model string.

## Suggesting features

Open an issue using the **Feature Request** template. Describe the problem you're trying to solve, not just the solution — this makes it easier to find the right approach.

## Development setup

```bash
git clone https://github.com/keithmackay/tokenomics.git
cd tokenomics

# Build and run tests (SQLite only, no external dependencies)
cargo build
cargo test

# Verify optional features compile
cargo build --features postgres
cargo build --features otel
cargo test --features otel
```

For Postgres tests, set `DATABASE_URL` to a running Postgres instance before running `cargo test --features postgres`.

## Making changes

1. Fork the repo and create a branch from `main`
2. Write tests first — new behavior should have a failing test before the implementation
3. Keep commits focused; one logical change per commit
4. Run the full test suite before opening a PR:
   ```bash
   cargo test
   cargo build --features postgres,otel
   ```
5. Open a pull request against `main`

## Code style

- `cargo fmt` before committing
- `cargo clippy -- -D warnings` must be clean
- No `unwrap()` in production paths — use `?` or explicit error handling
- Instrument new async handlers and significant code paths with `tracing`

## PR process

PRs are reviewed by the maintainer. One approval is required to merge. For larger changes (new features, breaking config changes), open an issue first to align on the approach before writing code.
