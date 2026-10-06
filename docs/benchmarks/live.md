# Local live telemetry measurements

These diagnostics require Windows and an active iRacing session. They observe
real shared-memory acquisition and watch-backed subscriber delivery. Results
depend on simulator pacing, Windows scheduling, session activity, and other
machine load; they are not Criterion or CodSpeed timings. No hosted CI job runs
an active simulator.

This is forward-looking benchmark infrastructure. The original #160 sequencing
required a pre-#111 baseline before changing the live acquisition API, but that
change landed first. Historical comparison remains possible by checking out an
older revision explicitly; it is no longer a prerequisite for live work.

## Measurement boundaries

`live_acquisition/owned_frame` waits for the next event outside the timer and
times the current `WindowsConnection::get_new_data()` operation through its
accepted `LiveFrameSnapshot`. The timed operation therefore includes the
bounded shared-memory copy and coherent tick/session metadata acceptance, but
not schema validation or post-sample stability checks. Only accepted operations
contribute to p50/p95/p99. No-frame attempts, event signals/timeouts, skipped
ticks, copied bytes, and effective accepted rate are reported separately.

The acquisition case uses experiment version 2 because its timed boundary is
not comparable to the abandoned pre-#111 implementation in PR #163, which
measured `get_new_data() + to_vec() + provider metadata rereads`.

`live_consumer/dynamic_1` preserves the existing native-rate `DynamicFrame`
subscription. `live_consumer/dynamic_4` creates four simultaneous subscriptions
on the same `LiveConnection`. Connection and subscription setup precede the
observation window. At least 120 source ticks are discarded, followed by at
least 600 source tick opportunities. Each subscriber reports received frames,
inter-arrival percentiles, effective rate, and skipped/coalesced ticks. Delivery
is latest-wins: a replaced intermediate frame is not an acquisition failure.
Inter-arrival time is cadence, not one-way end-to-end latency.

Both targets write format-versioned JSON with exact Git SHA, dirty state,
non-sensitive machine profile, Windows/CPU/Rust/build/power identity, source
rate, frame size, and a stable schema fingerprint. A timeout, disconnection,
geometry/schema/session change, or insufficient samples makes a case incomplete.
Incomplete runs are retained for diagnosis but cannot qualify as baselines.
Neither target stores telemetry payloads or session YAML.

## Capture and compare revisions

Build base and candidate in separate worktrees using the same Rust toolchain,
target, benchmark profile, and installed iRacing setup. Build before timed runs:

```powershell
git worktree add ../iracing-base <base-revision>
git worktree add ../iracing-candidate <candidate-revision>
cd ../iracing-base
cargo bench -p iracing-sdk --features benchmark --bench live-acquisition-diagnostic --bench live-telemetry-diagnostic --no-run
cd ../iracing-candidate
cargo bench -p iracing-sdk --features benchmark --bench live-acquisition-diagnostic --bench live-telemetry-diagnostic --no-run
```

Keep the same active session, source tick rate, power profile, and minimal
background load. Run each target at least three times per revision and alternate
base/head runs where practical to expose thermal or session drift.

```powershell
cargo bench -p iracing-sdk --features benchmark --bench live-acquisition-diagnostic -- --profile win11-desktop-a
cargo bench -p iracing-sdk --features benchmark --bench live-telemetry-diagnostic -- --profile win11-desktop-a
```

Default full summaries go to `target/live-benchmarks/runs/<run-id>.json`.
Use `--output <path>` for an explicit location. `--warmup-frames 120`,
`--target-frames 600` (minimum 600), and `--timeout-seconds 60` can be tuned
for a steady session.

The local record command copies a run out of `target` into ignored
`.live-benchmarks/runs/`, so `cargo clean` and worktree rebuilds do not remove
it. Set `IRACING_LIVE_BENCH_STORE` to a durable directory outside worktrees if
base and candidate should share one store.

```powershell
python scripts/live_benchmarks.py record target/live-benchmarks/runs/<run-id>.json --label current-main
python scripts/live_benchmarks.py compare --baseline current-main --head .live-benchmarks/runs/<candidate-id>.json --json-output delta.json --markdown-output delta.md
```

Comparison pairs only equal case IDs, experiment versions, machine hardware,
target/build/power profile, source rate, frame geometry/fingerprint, workload
version, and case parameters. Missing/new cases and changed experiments are
unpaired; other mismatches are incomparable. Never compare these wall-time
observations numerically with CodSpeed simulation results.

For a maintainer-reviewed PR, explicitly promote each complete, clean,
sanitized summary. Promotion removes raw per-frame/per-interval samples and
refuses to overwrite an existing result:

```powershell
python scripts/live_benchmarks.py promote .live-benchmarks/runs/<run-id>.json
```

This writes a small JSON file under
`docs/benchmarks/results/live/<profile>/<run-id>.json`. Review it before
committing and include the corresponding Markdown comparison in the PR or issue.
