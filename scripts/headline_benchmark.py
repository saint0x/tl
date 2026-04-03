#!/usr/bin/env python3

import json
import math
import shutil
import statistics
import subprocess
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
TL_BIN = ROOT / "target" / "aarch64-apple-darwin" / "release" / "tl"
RESULTS_DIR = ROOT / "artifacts"

INIT_REPS = 3
NETWORK_REPS = 3
GIT_NAME = "TL Live Bench"
GIT_EMAIL = "tl-live-bench@example.com"


class BenchError(RuntimeError):
    pass


@dataclass
class BenchContext:
    owner: str
    repo_name: str
    remote_url: str
    work: Path


def run(cmd: list[str], cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess:
    proc = subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        text=True,
        capture_output=True,
    )
    if check and proc.returncode != 0:
        raise BenchError(
            f"Command failed ({proc.returncode}): {' '.join(cmd)}\n"
            f"cwd={cwd}\nstdout={proc.stdout}\nstderr={proc.stderr}"
        )
    return proc


def percentile(values: list[float], pct: float) -> float:
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    pos = (len(ordered) - 1) * pct
    lo = math.floor(pos)
    hi = math.ceil(pos)
    if lo == hi:
        return ordered[lo]
    weight = pos - lo
    return ordered[lo] * (1.0 - weight) + ordered[hi] * weight


def summarize(samples: list[float]) -> dict:
    mean_s = statistics.mean(samples)
    return {
        "n": len(samples),
        "min_ms": round(min(samples) * 1000, 1),
        "mean_ms": round(mean_s * 1000, 1),
        "p50_ms": round(percentile(samples, 0.5) * 1000, 1),
        "p90_ms": round(percentile(samples, 0.9) * 1000, 1),
        "p99_ms": round(percentile(samples, 0.99) * 1000, 1),
        "max_ms": round(max(samples) * 1000, 1),
        "ops_per_sec": round(1.0 / mean_s, 3),
        "samples_ms": [round(v * 1000, 1) for v in sorted(samples)],
    }


def gh(*args: str, check: bool = True) -> subprocess.CompletedProcess:
    return run(["gh", *args], check=check)


def git(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return run(["git", *args], cwd=repo, check=check)


def tl(repo: Path, *args: str, check: bool = True) -> subprocess.CompletedProcess:
    return run([str(TL_BIN), *args], cwd=repo, check=check)


def configure_git_identity(repo: Path) -> None:
    git(repo, "config", "user.name", GIT_NAME)
    git(repo, "config", "user.email", GIT_EMAIL)


def wait_for_github_repo(remote_url: str) -> None:
    for attempt in range(10):
        proc = run(["git", "ls-remote", remote_url], check=False)
        if proc.returncode == 0:
            return
        time.sleep(1.0 + attempt * 0.25)
    raise BenchError(f"GitHub repo did not become reachable: {remote_url}")


def create_remote() -> BenchContext:
    owner = gh("api", "user", "-q", ".login").stdout.strip()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ").lower()
    repo_name = f"tl-headline-bench-{stamp}"
    remote_url = f"https://github.com/{owner}/{repo_name}.git"
    work = Path(tempfile.mkdtemp(prefix="tl-headline-bench-"))

    seed = work / "seed"
    seed.mkdir()
    git(seed, "init")
    configure_git_identity(seed)
    (seed / "README.md").write_text("# TL headline bench\n", encoding="utf-8")
    (seed / "src").mkdir()
    (seed / "src" / "app.txt").write_text("alpha\n", encoding="utf-8")
    git(seed, "add", ".")
    git(seed, "commit", "-m", "seed")
    git(seed, "branch", "-M", "main")

    gh("repo", "create", f"{owner}/{repo_name}", "--private")
    wait_for_github_repo(remote_url)

    git(seed, "remote", "add", "origin", remote_url)
    git(seed, "push", "-u", "origin", "main")
    return BenchContext(owner=owner, repo_name=repo_name, remote_url=remote_url, work=work)


def cleanup(ctx: BenchContext) -> None:
    gh("repo", "delete", f"{ctx.owner}/{ctx.repo_name}", "--yes", check=False)
    shutil.rmtree(ctx.work, ignore_errors=True)


def ensure_release_binary() -> None:
    if not TL_BIN.exists():
        raise BenchError(f"Release tl binary not found at {TL_BIN}")


def fresh_git_clone(ctx: BenchContext, name: str) -> Path:
    repo = ctx.work / name
    git(ctx.work, "clone", ctx.remote_url, str(repo))
    configure_git_identity(repo)
    return repo


def fresh_tl_clone(ctx: BenchContext, name: str) -> Path:
    repo = fresh_git_clone(ctx, name)
    tl(repo, "init")
    tl(repo, "stop", check=False)
    shutil.rmtree(repo / ".tl" / "state", ignore_errors=True)
    (repo / ".tl" / "state").mkdir(parents=True, exist_ok=True)
    return repo


def measure(fn, reps: int) -> dict:
    samples = []
    for rep in range(reps):
        start = time.perf_counter()
        fn(rep)
        samples.append(time.perf_counter() - start)
    return summarize(samples)


def create_remote_update(ctx: BenchContext, label: str) -> None:
    incoming = fresh_git_clone(ctx, f"incoming-{label}-{time.time_ns()}")
    target = incoming / "src" / f"remote-{time.time_ns()}.txt"
    target.write_text(f"remote update {time.time_ns()}\n", encoding="utf-8")
    git(incoming, "add", ".")
    git(incoming, "commit", "-m", f"remote update {label}")
    git(incoming, "push", "origin", "main")


def prepare_tl_push_repo(ctx: BenchContext, label: str) -> Path:
    repo = fresh_tl_clone(ctx, f"tl-push-{label}-{time.time_ns()}")
    target = repo / "src" / "push.txt"
    target.write_text(f"tl push {time.time_ns()}\n", encoding="utf-8")
    git(repo, "add", ".")
    git(repo, "commit", "-m", "tl push bench")
    run(["jj", "git", "import"], cwd=repo)
    run(["jj", "bookmark", "set", "main", "-r", "@-"], cwd=repo)
    return repo


def prepare_git_push_repo(ctx: BenchContext, label: str) -> Path:
    repo = fresh_git_clone(ctx, f"git-push-{label}-{time.time_ns()}")
    target = repo / "src" / "push.txt"
    target.write_text(f"git push {time.time_ns()}\n", encoding="utf-8")
    git(repo, "add", ".")
    git(repo, "commit", "-m", "git push bench")
    return repo


def benchmark(ctx: BenchContext) -> dict:
    results: dict[str, dict] = {}

    results["tl_init"] = measure(
        lambda rep: tl(ctx.work / f"tl-init-{rep}-{time.time_ns()}", "init"),
        reps=0,
    )
    return results


def benchmark_all(ctx: BenchContext) -> dict:
    results: dict[str, dict] = {}

    def bench_tl_init(rep: int) -> None:
        repo = ctx.work / f"tl-init-{rep}-{time.time_ns()}"
        repo.mkdir()
        tl(repo, "init")
        tl(repo, "stop", check=False)

    def bench_git_init(rep: int) -> None:
        repo = ctx.work / f"git-init-{rep}-{time.time_ns()}"
        repo.mkdir()
        git(repo, "init")

    results["tl_init"] = measure(bench_tl_init, INIT_REPS)
    results["git_init"] = measure(bench_git_init, INIT_REPS)

    results["tl_fetch_no_sync"] = measure(
        lambda rep: tl(fresh_tl_clone(ctx, f"tl-fetch-{rep}-{time.time_ns()}"), "fetch", "--no-sync"),
        NETWORK_REPS,
    )
    results["git_fetch_origin"] = measure(
        lambda rep: git(fresh_git_clone(ctx, f"git-fetch-{rep}-{time.time_ns()}"), "fetch", "--quiet", "origin"),
        NETWORK_REPS,
    )

    results["tl_pull_fetch_only"] = measure(
        lambda rep: tl(fresh_tl_clone(ctx, f"tl-pfo-{rep}-{time.time_ns()}"), "pull", "--fetch-only"),
        NETWORK_REPS,
    )
    results["git_fetch_origin_2"] = measure(
        lambda rep: git(fresh_git_clone(ctx, f"git-pfo-{rep}-{time.time_ns()}"), "fetch", "--quiet", "origin"),
        NETWORK_REPS,
    )

    def bench_tl_pull(rep: int) -> None:
        repo = fresh_tl_clone(ctx, f"tl-pull-{rep}-{time.time_ns()}")
        create_remote_update(ctx, f"tl-pull-{rep}")
        tl(repo, "pull")

    def bench_git_pull(rep: int) -> None:
        repo = fresh_git_clone(ctx, f"git-pull-{rep}-{time.time_ns()}")
        create_remote_update(ctx, f"git-pull-{rep}")
        git(repo, "pull", "--no-rebase", "origin", "main")

    results["tl_pull"] = measure(bench_tl_pull, NETWORK_REPS)
    results["git_pull"] = measure(bench_git_pull, NETWORK_REPS)

    def bench_tl_push(rep: int) -> None:
        repo = prepare_tl_push_repo(ctx, f"{rep}")
        tl(repo, "push")

    def bench_git_push(rep: int) -> None:
        repo = prepare_git_push_repo(ctx, f"{rep}")
        git(repo, "push", "origin", "main")

    results["tl_push"] = measure(bench_tl_push, NETWORK_REPS)
    results["git_push"] = measure(bench_git_push, NETWORK_REPS)
    return results


def main() -> None:
    ensure_release_binary()
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ctx = create_remote()
    try:
        results = benchmark_all(ctx)
        payload = {
            "meta": {
                "timestamp": datetime.now(timezone.utc).isoformat(),
                "repo": f"{ctx.owner}/{ctx.repo_name}",
                "remote_url": ctx.remote_url,
                "tl_bin": str(TL_BIN),
                "init_reps": INIT_REPS,
                "network_reps": NETWORK_REPS,
            },
            "results": results,
        }
        out_path = RESULTS_DIR / "headline-benchmark-after.json"
        out_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        print(json.dumps(payload, indent=2))
    finally:
        cleanup(ctx)


if __name__ == "__main__":
    main()
