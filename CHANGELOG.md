# Changelog

All notable changes to Frame are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, minor versions may contain breaking changes.

## [Unreleased]

### Added

- `.frame` parser with byte spans and comments, a comment-preserving formatter, and semantic checks with stable diagnostic codes.
- Deterministic typed IR and dependency graph with semantic and source hashes.
- Rust backend generation (Axum, SQLx, SQLite) and a browser UI built from native controls.
- `frame-runtime`: local development identity, CSRF, Origin and Host checks, idempotency receipts, revision-checked updates, rate and size limits, and a FIFO writer queue.
- Bounded context bundles, behavior diff, and stale-patch rejection for agent workflows.
- A supervisor with its own control database, persisted restart budgets, reconciliation, and redacted incident records.
- An isolated repair pipeline in the macOS Seatbelt sandbox with a deterministic fixture proposer, explicit local apply, and version-compatible precedents.
- `frame bench` with a hand-written baseline backend and a C++ kernel comparison.
- CLI commands: `init`, `fmt`, `check`, `build`, `dev`, `test`, `context`, `diff`, `status`, `incidents`, `repair`, `apply-repair` and `bench`. `plan` and `release` fail as unsupported.
