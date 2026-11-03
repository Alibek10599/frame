# Frame

Frame is a small, human-readable system language. A `.frame` file declares data, owner-scoped actions, screens, runtime limits, recovery policy, and repair authority. The compiler turns it into a native Rust backend (Axum + SQLx/SQLite), a browser UI built from native controls, a typed IR, and a dependency graph. Agents work from bounded, versioned context bundles instead of reconstructing the project from chat history.

Frame 0.1 is a developer preview. [docs/spec.md](docs/spec.md) is the normative contract and [examples/fieldwork](examples/fieldwork) the reference app. [docs/progress.md](docs/progress.md) maps every acceptance item to its evidence, and [docs/decisions.md](docs/decisions.md) records spec clarifications.

This is a local vertical slice. It is not a production platform, and it makes no universal security or C++-parity claims (see [Non-claims](#limits-and-non-claims)).

## Requirements

- Rust via rustup. `rust-toolchain.toml` pins 1.97.1, and rustup installs it on first use.
- macOS for isolated repair, which uses Seatbelt (`/usr/bin/sandbox-exec`). On other platforms repair runs proposal and static validation only and reports `SANDBOX_UNAVAILABLE`.
- Network access for the first `frame build`, which downloads crates once. Later builds work offline.
- Optional: Node 22+ and a Chromium (Playwright's headless shell or Chrome) for the browser journey test, and `c++` for the C++ kernel benchmark.

## Install from source

There are no prebuilt binaries yet. From a checkout:

```sh
cargo build --release                 # builds the CLI at target/release/frame
export PATH="$PWD/target/release:$PATH"
```

Generated apps depend on `runtime/` in this checkout through a path dependency, so keep the checkout in place after building. For the same reason the crates are not published to crates.io yet ([D4](docs/decisions.md)).

## Run the reference app

```sh
cd examples/fieldwork
frame check                           # parse, type, reference and policy checks (offline)
frame fmt --check                     # deterministic formatting; comments are preserved
frame build                           # writes .frame/{ir.json,graph.json,generated/,build/,manifest.json}
frame dev                             # builds, starts the supervisor and app, prints a one-time bootstrap URL
```

Open the printed `open: http://127.0.0.1:7331/#bootstrap=…` URL. The page exchanges the token, removes it from the URL, and sets an HttpOnly, SameSite=Strict session. The dev bar switches between the two seeded users (User A and User B). Stop with Ctrl-C.

The first `frame build` compiles Axum, SQLx and Tokio in release mode, which takes about a minute. To share that cache between projects, set `FRAME_TARGET_DIR`.

## Commands

| Command | What it does |
| --- | --- |
| `frame init NAME` | Create `NAME/` with an example app and protected tests; never overwrites |
| `frame fmt [--check]` | Format deterministically, preserving comments; refuses output that changes semantics |
| `frame check` | Validate offline, with diagnostics that carry file, line, column, byte span and node id |
| `frame build` | Write IR, graph, Rust backend, web files and the artifact manifest, then compile |
| `frame dev [--acknowledge]` | Run the supervisor and app; `--acknowledge` clears `StoppedNeedsAttention` and keeps the restart history |
| `frame test` | Run generated contract checks plus the protected tests in `acceptance/*.json`; failures become incidents |
| `frame context ID [--max-bytes N]` | Write `.frame/context/<id>/context.md` and `manifest.json` |
| `frame diff --base PATH` | Diff behavior and dependencies against another snapshot |
| `frame status` / `frame incidents [--id ID]` | Show supervisor state and redacted incident records |
| `frame repair ID --proposer fixture\|file:PATH` | Run the isolated repair pipeline; never releases |
| `frame apply-repair ID --candidate HASH` | Explicit local apply after rechecking hashes and protected tests |
| `frame bench [--quick]` | Benchmark report in `.frame/bench/report.json` |
| `frame plan` / `frame release` | Fail with `E_UNSUPPORTED_FEATURE` in 0.1 |

Every command accepts `--project PATH` and `--json`.

Exit codes:

- 0: success.
- 1: validation or check failure.
- 2: usage error or unsupported feature.
- 3: execution or environment failure.
- 4: stale precondition or policy rejection.

### Seeded-bug repair walkthrough

From the repository root:

```sh
mkdir /tmp/fw && cp -R examples/fieldwork/. /tmp/fw/
sed -i '' 's/set = { completed: true };/set = { completed: false };/' /tmp/fw/fieldwork.frame
frame --project /tmp/fw test                                  # protected test fails; prints the incident id
frame --project /tmp/fw repair <incident> --proposer fixture  # reproduce, propose, validate in Seatbelt; source untouched
frame --project /tmp/fw apply-repair <incident> --candidate <hash>
```

## Protected tests

`acceptance/*.json` files are declarative HTTP scenarios. They are data, not code, and the repair pipeline cannot modify them:

```json
{ "name": "…", "covers": ["fieldwork/action/complete_task"],
  "steps": [ { "as": "a", "action": "create_task", "input": { "title": "x" }, "expect": { "status": 200 }, "save": "task" },
             { "as": "a", "action": "complete_task", "input": { "id": "$task.id", "revision": "$task.revision" },
               "expect": { "status": 200, "data": { "completed": true } } } ] }
```

`expect.data` is a subset match. `$name.field` substitutes a saved response, `as` is `a`, `b` or omitted (unauthenticated), and `key` pins an idempotency key.

## Environment

| Variable | Purpose |
| --- | --- |
| `FRAME_TARGET_DIR` | Cargo target dir for generated apps (default `.frame/build/target`) |
| `FRAME_PORT`, `FRAME_DATABASE` | Trusted operator override of the declared port (0 picks a free one) and the database path |
| `FRAME_FAULT` | Dev and test fault injection: `crash_on_start`, `crash_on_request:<action>`, `crash_before_commit:<action>`, `crash_after_commit:<action>` |
| `FRAME_MUTATIONS_PER_MINUTE` | Operator override of the per-actor mutation limit (default 60); the check still runs |
| `FRAME_SANDBOX=none` | Force the no-isolation path (repair reports `SANDBOX_UNAVAILABLE`) |
| `FRAME_CHROMIUM`, `FRAME_REQUIRE_BROWSER` | Browser for the journey test; with `FRAME_REQUIRE_BROWSER` set, a missing browser fails the test instead of skipping it |

## Tests

```sh
cargo test --workspace     # language, runtime, browser, agent workflow, recovery and repair suites
frame bench                # full benchmark (release CLI recommended: target/release/frame)
```

The suites build generated apps into `target/frame-apps` and run real HTTP against real SQLite, real worker processes, and real Seatbelt sandboxes. Nothing in them is mocked.

## Layout

| Path | Contents |
| --- | --- |
| `src/syntax.rs`, `src/fmt.rs` | Parser with byte spans and comments, and the formatter |
| `src/semantic.rs`, `src/ir.rs` | Schema, type and reference checks, typed IR, graph, canonical JSON and SHA-256 hashes |
| `src/codegen.rs`, `src/web/` | Rust handler generation (parameterized SQL, structurally escaped literals) and web assets |
| `src/pipeline.rs` | Artifacts, cargo invocation as an argv array, and mapping rustc errors back to Frame spans |
| `runtime/` | `frame-runtime`: sessions, CSRF/Origin/Host checks, limits, idempotent transactions, readiness |
| `src/testrun.rs`, `src/launch.rs`, `src/http.rs` | Contract checks, the protected test runner, app launcher, and a minimal HTTP client |
| `src/context.rs`, `src/diff.rs`, `src/patch.rs`, `src/progress.rs` | Context bundles, behavior diff, patch preconditions, and resumable task records |
| `src/supervisor.rs` | Supervisor, control DB, restart budget, reconciliation, and incidents |
| `src/repair.rs`, `src/sandbox.rs` | Repair state machine, fixture proposer, Seatbelt sandbox, and precedents |
| `src/bench.rs`, `bench/baseline/` | Benchmark harness and the hand-written baseline backend |
| `examples/fieldwork/` | Reference app and its protected acceptance tests |
| `docs/` | Specification, decision records and the progress report |
| `tests/` | Integration suites; `scripts/collect-evidence.sh` exports acceptance evidence |

## Implemented and unsupported

Implemented:

- The 0.1 grammar and schema, typed IR and graph, formatter, and diagnostics.
- Rust and web generation with SQLite persistence.
- Local development identity with CSRF, Origin and Host checks, idempotency receipts, and revision checks.
- Supervisor with persisted budgets and reconciliation, and incident records.
- Context bundles, behavior diff, and stale-patch rejection.
- The isolated repair pipeline with explicit apply, and the benchmark harness.

Unsupported in 0.1, each rejected with a diagnostic:

- Mobile targets, production identity, PostgreSQL and migrations.
- Workflows, external effects and adapters, native extensions, and policy expressions.
- Public or anonymous access, list filters, and floats or other types beyond Text, Bool, UUID and Int.
- `automatic_release = true`, and `plan` and `release`.
- Live LLM proposers.

## Limits and non-claims

- `local_dev` identity is a convenience for one developer on one machine. It does not protect against a hostile local user or malware.
- First-party crates and generated code use `#![forbid(unsafe_code)]`. Dependencies, SQLite and the OS still contain unsafe code and remain trusted.
- On macOS, the Seatbelt sandbox enforces filesystem confinement, network denial, a wall-clock limit, and CPU, file-size and open-file rlimits. Memory and process-count rlimits are not enforced by macOS for this use; the run phase instead denies `fork`. `sandbox-exec` is deprecated by Apple but still functional.
- Benchmark results are measurements on one machine with stated baselines. Missed budgets are reported, not hidden.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues privately as described in [SECURITY.md](SECURITY.md). Changes are listed in [CHANGELOG.md](CHANGELOG.md).

## License

Frame is dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual-licensed as above, without any additional terms or conditions.
