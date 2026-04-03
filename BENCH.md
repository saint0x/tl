# TL Live Benchmark Baseline

This document records the live GitHub-backed benchmark run performed on 2026-04-03 against a fresh private repository under the `saint0x` account.

## Scope

- Real GitHub remote, not a local bare repo
- `tl` binary under test:
  - `/Users/deepsaint/Desktop/timelapse/target/aarch64-apple-darwin/release/tl`
- Fozzy-first validation completed before live runs:
  - `fozzy doctor --deep --scenario tests/test_git_parity.run.fozzy.json --runs 5 --seed 7 --json`
  - `fozzy test --det --strict tests/test_git_parity.run.fozzy.json tests/cli.host.fozzy.json --json`
  - `fozzy run tests/test_git_parity.run.fozzy.json --det --record /tmp/timelapse-git-parity-trace.fozzy --json`
  - `fozzy trace verify /tmp/timelapse-git-parity-trace.fozzy --strict --json`
  - `fozzy replay /tmp/timelapse-git-parity-trace.fozzy --json`
  - `fozzy ci /tmp/timelapse-git-parity-trace.fozzy --json`
  - `fozzy run tests/host.pass.fozzy.json --det --proc-backend host --fs-backend host --http-backend host --json`

## Summary

- The strict Git-parity fetch paths are very close to raw Git.
- `tl pull` works end-to-end, but has noticeably worse tail latency than `git pull`.
- `tl push` failed repeatedly in the live environment with `refs/heads/main: stale info`.
- Local parity commands (`remote`, `tag`, `branch`) are generally within a modest constant overhead of Git.
- `tl init` is much slower than `git init`, which is expected because it performs substantially more work.

## Headline Latency Results

All values are wall-clock timings from repeated fresh-clone runs.

| Operation | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| `fetch --no-sync` vs `git fetch --quiet origin` | 929.9ms | 924.5ms | +0.6% |
| `pull --fetch-only` vs `git fetch --quiet origin` | 944.3ms | 933.6ms | +1.1% |
| `pull` vs `git pull --no-rebase origin main` | 1120.4ms | 1044.7ms | +7.2% |
| `init` vs `git init` | 5321.2ms | 101.1ms | +5163.0% |

## TL-Only Baseline

`tl publish HEAD` has no direct raw-Git equivalent because it is a checkpoint-to-JJ materialization step.

| Operation | p50 | p90 | p99 | Throughput |
|---|---:|---:|---:|---:|
| `tl publish HEAD` | 148.0ms | 206.3ms | 219.4ms | 5.844 ops/s |

## Detailed Parity Results

### Fetch / Pull-Fetch-Only

| Case | TL p50 | Git p50 | Notes |
|---|---:|---:|---|
| `fetch --no-sync` | 929.9ms | 924.5ms | Effectively Git-parity |
| `pull --fetch-only` | 944.3ms | 933.6ms | Effectively Git-parity |

Observed behavior:

- `tl fetch --no-sync` and `tl pull --fetch-only` are both close enough to Git that network variance dominates the difference.
- These commands appear to be using the intended low-overhead fast path.

### Full Pull

`tl pull` successful live run behavior:

- fetched remote branch
- auto-stashed local changes
- imported remote commit as checkpoint
- synced working directory
- re-applied stash

Latency:

| Case | TL | Git |
|---|---:|---:|
| p50 | 1120.4ms | 1044.7ms |
| p90 | 2161.1ms | 1047.2ms |
| p99 | 2395.3ms | 1047.8ms |

Interpretation:

- Median overhead is modest.
- Tail latency is the problem.
- The likely expensive region is the checkpoint-import / stash / sync path rather than fetch itself.

### Init

| Case | TL | Git |
|---|---:|---:|
| p50 | 5321.2ms | 101.1ms |

Interpretation:

- `tl init` is doing much more than `git init`:
  - Git setup
  - JJ workspace creation
  - Timelapse store creation
  - initial checkpoint
  - config sync
  - daemon startup attempt
- This is expected to be much slower, but the daemon startup path is contributing user-visible wait.

### Branch Commands

| Case | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| list | 78.8ms | 89.4ms | -11.9% |
| create | 112.6ms | 91.4ms | +23.2% |
| delete | 94.1ms | 91.1ms | +3.3% |

Interpretation:

- Branch list is already competitive.
- Branch create has some extra JJ/checkpoint resolution overhead.

### Remote Commands

| Case | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| add | 117.6ms | 91.7ms | +28.2% |
| get-url | 97.3ms | 89.4ms | +8.9% |
| set-url | 98.6ms | 89.6ms | +10.0% |
| rename | 95.0ms | 90.3ms | +5.3% |
| remove | 95.0ms | 88.8ms | +7.0% |

Interpretation:

- These are all reasonably close to Git.
- `tl remote add` has the largest fixed overhead in this group.

### Tag Commands

| Case | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| create | 116.7ms | 94.1ms | +23.9% |
| show | 97.1ms | 92.8ms | +4.6% |
| delete | 96.8ms | 87.7ms | +10.4% |

Interpretation:

- Tag commands are close, though not as tight as remote operations.

## Failures Found

### 1. `tl push` failed repeatedly

Live outcome:

- `tl push` failed 3/3 repeated runs
- `git push origin main` succeeded in the same benchmark environment

Error:

```text
Push rejected:
refs/heads/main: stale info
```

Additional note:

- The failure reproduced even after an explicit `tl pull --fetch-only` before `tl publish HEAD` and `tl push`.
- This suggests the issue is not just "repo started stale" but likely stale bookmark / remote-head / push-target state in the JJ bridge.

### 2. Daemon startup instability in some cloned benchmark repos

Observed during automation:

```text
Daemon crashed: Failed to start IPC server
Daemon crashed 5 times in 60s - giving up
Error: Too many daemon crashes
```

Impact:

- This blocked broad repeated benchmarking for daemon-heavy commands such as:
  - `status`
  - `flush`
  - `restore`
  - larger end-to-end automation loops

Important nuance:

- The same daemon-dependent flows can succeed when run directly and carefully.
- The instability appears sensitive to repo lifecycle / temp clone setup / repeated fresh benchmark orchestration.
- Even if the benchmark harness amplified it, this still indicates fragility in production-like repeated clone environments.

### 3. `tl pull` tail latency instability

Observed:

- p50 is acceptable
- p90/p99 are much worse than Git

Likely latency sources:

- daemon startup / connection path
- auto-stash via flush retries
- checkpoint import path
- working tree sync
- pathmap invalidation or rebuild

## Throughput Snapshot

Approximate ops/sec at mean latency:

| Operation | TL ops/sec | Git ops/sec |
|---|---:|---:|
| `fetch --no-sync` | 1.065 | 1.072 |
| `pull --fetch-only` | 1.040 | 1.048 |
| `pull` | 0.645 | 0.958 |
| `init` | 0.188 | 8.930 |
| `remote add` | 8.537 | 11.030 |
| `tag create` | 8.516 | 10.707 |
| `branch create` | 8.999 | 10.972 |

## Root-Cause Hypotheses To Investigate

### `tl push`

Primary hypothesis:

- The JJ push path is using stale remote bookmark / remote ref info and is not refreshing state correctly before push negotiation.

Secondary hypotheses:

- publish-to-bookmark mapping is out of sync with remote-tracking state
- remote branch resolution is cached or read from stale view state
- push uses a branch/bookmark target that no longer matches the remote ref after previous live updates

### `tl pull`

Primary hypothesis:

- Tail latency is dominated by full-workflow orchestration after fetch rather than by network fetch itself.

Suspected components:

- daemon ensure/start
- retry-based flush in auto-stash
- remote import as checkpoint
- filesystem restore/sync
- pathmap invalidation/reconciliation

### Daemon IPC startup

Primary hypothesis:

- Unix socket lifecycle is fragile under repeated short-lived cloned repos.

Suspected causes:

- stale socket file handling
- binding race / cleanup race
- crash loop logic triggering before cleanup finishes
- interaction between `tl init` auto-start and explicit later daemon start

## Confidence / Caveats

- High confidence:
  - fetch fast-path parity
  - remote/tag/branch command overhead levels

## Post-Fix Status (2026-04-03)

This section records the production fixes implemented after the baseline above.

### Core Production Fixes Landed

- `tl push`
  - refreshes remote-tracking state before push
  - resolves the actual configured remote instead of assuming `origin`
  - rejects non-fast-forward pushes unless `--force`
  - returns a failing exit code when JJ reports rejected/diverged refs
- `tl pull`
  - fetches against the resolved remote/branch
  - exits early without daemon startup when remote branch is missing
  - exits early without daemon startup when local bookmark already matches remote
  - delays daemon startup until the import/sync path is actually needed
- Daemon / IPC
  - daemon socket path is now resolved centrally via a short per-user hashed runtime path under `/tmp`
  - repo-local socket path length is no longer part of daemon startup correctness
  - legacy `.tl/state/daemon.sock` files are cleaned up opportunistically
  - background daemon start now pins the child process working directory to the repo root
  - daemon crash logs now print the full error context chain instead of only the outer message
- Benchmark harness
  - `scripts/live_benchmark.py` now strips ANSI color codes before parsing checkpoint IDs from `tl flush`

### Fresh-Clone Daemon Repro: Fixed

The previously failing fresh-clone `tl init` repro now succeeds end-to-end:

- `tl init` auto-starts the daemon successfully
- `tl status` confirms the daemon stays running after init
- daemon log now shows the new runtime socket path, for example:

```text
/tmp/tl-501/sockets/ad5263d322271aebdf3c8586.sock
```

This specifically resolves the earlier repeated failure mode:

```text
Daemon crashed: Failed to start IPC server
Daemon crashed 5 times in 60s - giving up
```

### Focused Verification Passed

- `cargo test -q -p cli ipc::tests::test_ipc_server_replaces_stale_socket -- --nocapture`
- `cargo test -q -p cli ipc::tests::test_ipc_server_keeps_live_socket -- --nocapture`
- `cargo test -q -p cli --test test_git_parity test_daemon_socket_path_is_short_for_deep_repos -- --nocapture`
- `cargo test -q -p cli --test test_git_parity test_pull_up_to_date_does_not_start_daemon -- --nocapture`
- `cargo test -q -p cli --test test_git_parity test_push_refreshes_remote_state_without_daemon -- --nocapture`
- `cargo test -q -p cli --test test_git_parity test_push_requires_force_for_non_fast_forward -- --nocapture`
- `cargo build --release -p cli`

### Fozzy Validation Passed

- `fozzy doctor --deep --scenario tests/daemon.run.fozzy.json --runs 5 --seed 7 --json`
- `fozzy test --det --strict tests/daemon.run.fozzy.json tests/init.run.fozzy.json tests/cli.host.fozzy.json --json`
- `fozzy run tests/daemon.run.fozzy.json --det --record /tmp/timelapse-daemon-trace-after.fozzy --json`
- `fozzy trace verify /tmp/timelapse-daemon-trace-after.fozzy --strict --json`
- `fozzy replay /tmp/timelapse-daemon-trace-after.fozzy --json`
- `fozzy ci /tmp/timelapse-daemon-trace-after.fozzy --json`
- `fozzy run tests/host.pass.fozzy.json --det --proc-backend host --fs-backend host --http-backend host --json`

### Current Rerun Status

- The original daemon-startup blocker from the live benchmark is fixed.
- The benchmark harness parser bug for colored checkpoint IDs is fixed.
- A full clean live GitHub-backed after-pass has not yet been recorded into this document.
- A broad live rerun was started after the fixes and progressed through repeated init/status/info/flush/log-style setup without reproducing the old daemon IPC crash loop, but it was too noisy and time-expensive to use as the final measurement artifact.

### Remaining Work For Final Benchmark Refresh

- Run a compact GitHub-backed after-pass for:
  - `tl init` vs `git init`
  - `tl fetch --no-sync` vs `git fetch --quiet origin`
  - `tl pull --fetch-only` vs `git fetch --quiet origin`
  - `tl pull` vs `git pull --no-rebase origin main`
  - `tl push` vs `git push origin main`
- Append the before/after deltas here once captured.

## Clean After-Benchmark (2026-04-03)

This after-pass was run live against a fresh private GitHub repository using:

- `tl` release binary at `/Users/deepsaint/Desktop/timelapse/target/aarch64-apple-darwin/release/tl`
- 3 reps for `init`
- 3 reps for network-backed fetch / pull / push flows

Raw artifact:

- [headline-benchmark-after.json](/Users/deepsaint/Desktop/timelapse/artifacts/headline-benchmark-after.json)

### Headline Results After Fixes

| Operation | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| `init` vs `git init` | 392.3ms | 60.4ms | +549.5% |
| `fetch --no-sync` vs `git fetch --quiet origin` | 3208.2ms | 2373.8ms | +35.2% |
| `pull --fetch-only` vs `git fetch --quiet origin` | 2568.0ms | 2275.7ms | +12.8% |
| `pull` vs `git pull --no-rebase origin main` | 7043.1ms | 4982.0ms | +41.4% |
| `push` vs `git push origin main` | 4629.5ms | 2537.8ms | +82.4% |

### Before vs After For TL

| Operation | Before TL p50 | After TL p50 | Change |
|---|---:|---:|---:|
| `init` | 5321.2ms | 392.3ms | -92.6% |
| `fetch --no-sync` | 929.9ms | 3208.2ms | +245.0% |
| `pull --fetch-only` | 944.3ms | 2568.0ms | +171.9% |
| `pull` | 1120.4ms | 7043.1ms | +528.6% |
| `push` | failed 3/3 | 4629.5ms | correctness fixed |

### Interpretation

- `tl push`
  - correctness is materially improved: the previous live `stale info` failure no longer reproduces in the clean after-pass
  - latency is still much worse than raw Git, so push has moved from correctness blocker to performance work
- `tl init`
  - dramatically better than the original live baseline
  - the daemon-start path is now reliable and much cheaper in this measurement setup
- `tl pull`
  - the architectural fast-path and daemon fixes landed correctly
  - but the full live flow is still substantially slower than Git and remains the largest latency hotspot
- `tl fetch --no-sync` / `tl pull --fetch-only`
  - no correctness issues
  - but this after-pass is notably slower than the original baseline, so we should treat that as a regression signal and investigate the current network / initialization overhead on the fetch path

## Hotspot After-Pass (2026-04-03, second optimization run)

After adding:

- direct Git fast path for the common `tl push` case when Git `HEAD` already matches the tracked bookmark
- direct branch-scoped Git fetch/ref resolution in `tl pull`
- direct in-process import path for clean cold `tl pull` runs when the daemon is not already running

we reran the clean headline benchmark again.

Raw artifact:

- [headline-benchmark-after.json](/Users/deepsaint/Desktop/timelapse/artifacts/headline-benchmark-after.json)

### Updated Headline Results

| Operation | TL p50 | Git p50 | Delta |
|---|---:|---:|---:|
| `init` vs `git init` | 384.8ms | 56.6ms | +579.9% |
| `fetch --no-sync` vs `git fetch --quiet origin` | 2508.0ms | 2095.0ms | +19.7% |
| `pull --fetch-only` vs `git fetch --quiet origin` | 3391.5ms | 2436.8ms | +39.2% |
| `pull` vs `git pull --no-rebase origin main` | 6885.1ms | 5240.3ms | +31.4% |
| `push` vs `git push origin main` | 3539.4ms | 2400.8ms | +47.4% |

### Delta vs Previous After-Pass

| Operation | Previous TL p50 | New TL p50 | Change |
|---|---:|---:|---:|
| `init` | 392.3ms | 384.8ms | -1.9% |
| `fetch --no-sync` | 3208.2ms | 2508.0ms | -21.8% |
| `pull --fetch-only` | 2568.0ms | 3391.5ms | +32.1% |
| `pull` | 7043.1ms | 6885.1ms | -2.2% |
| `push` | 4629.5ms | 3539.4ms | -23.5% |

### Current Takeaway

- Best hotspot win:
  - `tl push` improved materially and remains correct in live runs
- Moderate win:
  - `tl fetch --no-sync` moved back closer to Git
- Small win:
  - `tl pull` improved slightly, but is still a major DX hotspot
- Regression still present:
  - `tl pull --fetch-only` was worse in this rerun and needs separate investigation

## Post-Fix Status

Production fixes implemented on 2026-04-03:

- `tl push` now refreshes remote-tracking state immediately before constructing JJ push targets.
- `tl push` now resolves the remote/branch from the same Git sync target logic as `tl pull`.
- `tl push` no longer starts the daemon on the hot path.
- non-fast-forward pushes now require `--force` explicitly in the TL wrapper path.
- `tl pull` now has a daemon-free no-op fast path when the local bookmark already matches the remote bookmark.
- `tl branch` no longer starts the daemon for list/create/delete routing.
- daemon IPC startup now refuses to replace a live socket and recovers stale socket files more safely.

### Post-Fix Verification

Focused regression coverage added:

- `test_push_refreshes_remote_state_without_daemon`
- `test_push_requires_force_for_non_fast_forward`
- `test_pull_up_to_date_does_not_start_daemon`
- `ipc::tests::test_ipc_server_replaces_stale_socket`
- `ipc::tests::test_ipc_server_keeps_live_socket`

Verification outcomes after the fixes:

- `cargo test -q -p jj`
  - passed (`33/33`)
- focused `cli` tests for push/pull/ipc
  - passed
- Fozzy deterministic validation
  - `fozzy doctor --deep --scenario tests/test_git_parity.run.fozzy.json --runs 5 --seed 7 --json`
  - `fozzy test --det --strict tests/test_git_parity.run.fozzy.json tests/cli.host.fozzy.json --json`
  - `fozzy run tests/test_git_parity.run.fozzy.json --det --record /tmp/timelapse-git-parity-trace-after.fozzy --json`
  - `fozzy trace verify /tmp/timelapse-git-parity-trace-after.fozzy --strict --json`
  - `fozzy replay /tmp/timelapse-git-parity-trace-after.fozzy --json`
  - `fozzy ci /tmp/timelapse-git-parity-trace-after.fozzy --json`
  - `fozzy run tests/host.pass.fozzy.json --det --proc-backend host --fs-backend host --http-backend host --json`
  - all passed

### Remaining Benchmark Blocker

I attempted a fresh live GitHub rerun after the fixes, but the same daemon-startup fragility still shows up during `tl init` in repeated fresh-clone benchmark repos:

```text
Warning: Could not auto-start daemon: Daemon failed to start within 5s
```

That means:

- the push correctness bug is fixed in targeted regression tests
- the daemon-free no-op `pull` path is fixed in targeted regression tests
- the full fresh-clone live benchmark harness is still not trustworthy for daemon-touched workflows until daemon startup is hardened further

### Broader Test Suite Note

The full `cargo test -q -p cli` suite still has a large pre-existing set of failing workflow tests in restore / large-file / checkpoint integrity areas, for example:

- restore / rewind correctness
- binary and large-file restore fidelity
- pin / GC preservation
- some flush semantics

Those failures are separate from the push/pull/daemon-latency changes in this pass, but they remain production risks and should be treated as the next correctness tranche after the Git parity fixes above.
  - `tl push` stale-info failure is real and reproducible
  - `tl pull` tail latency is materially worse than Git
- Medium confidence:
  - exact magnitude of daemon-heavy command latency outside carefully controlled runs
- Low confidence:
  - broad repeated benchmark results for commands that require repeated daemon lifecycle churn in temp clones

## Files Produced During Benchmarking

- Root summary: `BENCH.md`
- Helper scripts:
  - `scripts/live_benchmark.py`
  - `scripts/bench_core.zsh`

## Recommended Next Work

1. Fix `tl push` stale-info behavior first because it is correctness-blocking.
2. Investigate daemon IPC startup fragility next because it affects both reliability and latency.
3. Optimize `tl pull` tail latency after correctness/reliability issues are stabilized.
4. Re-run the same live GitHub benchmark suite and compare against this baseline.
