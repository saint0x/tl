#!/usr/bin/env python3

import json
import math
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable


ROOT = Path(__file__).resolve().parents[1]
TL_BIN = ROOT / "target" / "aarch64-apple-darwin" / "release" / "tl"
RESULTS_DIR = ROOT / "artifacts"

GIT_NAME = os.environ.get("TL_BENCH_GIT_NAME", "TL Live Bench")
GIT_EMAIL = os.environ.get("TL_BENCH_GIT_EMAIL", "tl-live-bench@example.com")
LOCAL_REPS = int(os.environ.get("TL_BENCH_LOCAL_REPS", "7"))
NETWORK_REPS = int(os.environ.get("TL_BENCH_NETWORK_REPS", "5"))
KEEP_REMOTE = os.environ.get("TL_BENCH_KEEP_REMOTE", "").lower() in {"1", "true", "yes"}


class BenchError(RuntimeError):
    pass


@dataclass
class Case:
    name: str
    kind: str
    reps: int
    runner: str
    command: list[str]
    setup: Callable[[Path, dict], None]
    compare_key: str | None = None
    notes: str | None = None


def run(cmd, cwd=None, check=True, capture_output=True, input_text=None):
    proc = subprocess.run(
        cmd,
        cwd=str(cwd) if cwd else None,
        text=True,
        input=input_text,
        capture_output=capture_output,
    )
    if check and proc.returncode != 0:
        raise BenchError(
            f"Command failed ({proc.returncode}): {' '.join(cmd)}\n"
            f"cwd={cwd}\nstdout={proc.stdout}\nstderr={proc.stderr}"
        )
    return proc


def ensure_tools():
    missing = []
    for name in ["git", "gh", "python3"]:
        if shutil.which(name) is None:
            missing.append(name)
    if missing:
        raise BenchError(f"Missing required tools: {', '.join(missing)}")
    if not TL_BIN.exists():
        raise BenchError(f"TL binary not found at {TL_BIN}")


def git(repo: Path, *args: str, check=True, capture_output=True):
    return run(["git", *args], cwd=repo, check=check, capture_output=capture_output)


def tl(repo: Path, *args: str, check=True, capture_output=True):
    return run([str(TL_BIN), *args], cwd=repo, check=check, capture_output=capture_output)


def gh(*args: str, check=True, capture_output=True):
    return run(["gh", *args], check=check, capture_output=capture_output)


def configure_git_identity(repo: Path):
    git(repo, "config", "user.name", GIT_NAME)
    git(repo, "config", "user.email", GIT_EMAIL)


def write_file(path: Path, content: str):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def parse_checkpoint(stdout: str) -> str | None:
    ansi_stripped = re.sub(r"\x1b\[[0-9;]*m", "", stdout)
    match = re.search(r"\b[0-9A-HJKMNP-TV-Z]{26}\b", ansi_stripped)
    return match.group(0) if match else None


def percentile(sorted_values: list[float], pct: float) -> float:
    if not sorted_values:
        return 0.0
    if len(sorted_values) == 1:
        return sorted_values[0]
    pos = (len(sorted_values) - 1) * pct
    lower = math.floor(pos)
    upper = math.ceil(pos)
    if lower == upper:
        return sorted_values[lower]
    weight = pos - lower
    return sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight


def summarize(samples: list[float]) -> dict:
    ordered = sorted(samples)
    mean_s = statistics.mean(samples)
    return {
        "n": len(samples),
        "min_ms": round(min(ordered) * 1000, 3),
        "mean_ms": round(mean_s * 1000, 3),
        "p50_ms": round(percentile(ordered, 0.50) * 1000, 3),
        "p90_ms": round(percentile(ordered, 0.90) * 1000, 3),
        "p99_ms": round(percentile(ordered, 0.99) * 1000, 3),
        "max_ms": round(max(ordered) * 1000, 3),
        "ops_per_sec": round(1.0 / mean_s, 3) if mean_s > 0 else None,
        "samples_ms": [round(v * 1000, 3) for v in ordered],
    }


def clean_tl_state(repo: Path):
    state_dir = repo / ".tl" / "state"
    if state_dir.exists():
        for child in state_dir.iterdir():
            if child.is_dir():
                shutil.rmtree(child, ignore_errors=True)
            else:
                child.unlink(missing_ok=True)


def kill_bench_daemons(work_root: Path):
    proc = run(["ps", "-ax", "-o", "pid=,command="], check=True)
    for line in proc.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        parts = line.split(None, 1)
        if len(parts) != 2:
            continue
        pid_str, command = parts
        if str(work_root) in command and str(TL_BIN) in command:
            try:
                os.kill(int(pid_str), 15)
            except ProcessLookupError:
                pass


def seed_remote(work: Path, owner: str, repo_name: str) -> str:
    seed = work / "seed"
    seed.mkdir(parents=True, exist_ok=True)
    git(seed, "init")
    configure_git_identity(seed)
    write_file(seed / "README.md", "# TL live benchmark\n")
    write_file(seed / "src" / "app.txt", "alpha\n")
    write_file(seed / "src" / "lib.txt", "beta\n")
    write_file(seed / "docs" / "notes.md", "seed docs\n")
    git(seed, "add", ".")
    git(seed, "commit", "-m", "seed")
    git(seed, "branch", "-M", "main")
    remote_url = f"https://github.com/{owner}/{repo_name}.git"
    gh("repo", "create", f"{owner}/{repo_name}", "--private")
    git(seed, "remote", "add", "origin", remote_url)
    git(seed, "push", "-u", "origin", "main")
    return remote_url


def make_templates(work: Path, remote_url: str) -> dict:
    local_origin = work / "local-origin.git"
    git(work, "clone", "--mirror", remote_url, str(local_origin))

    return {
        "local_origin": local_origin,
        "remote_url": remote_url,
        "dummy_remote_url": "https://github.com/example/example.git",
        "dummy_remote_url_2": "https://github.com/example/example-2.git",
        "work_root": work,
    }


def start_daemon(repo: Path):
    tl(repo, "start", capture_output=False)
    time.sleep(1.0)


def stop_daemon(repo: Path):
    tl(repo, "stop", check=False, capture_output=False)
    time.sleep(0.5)
    clean_tl_state(repo)


def make_checkpoint(repo: Path, rel_path: str, content: str):
    write_file(repo / rel_path, content)
    time.sleep(0.6)
    out = tl(repo, "flush").stdout
    checkpoint = parse_checkpoint(out)
    if checkpoint is None:
        raise BenchError(f"Failed to parse checkpoint from:\n{out}")
    return checkpoint


def publish_head(repo: Path, bookmark: str | None = None):
    args = ["publish", "HEAD"]
    if bookmark:
        args.extend(["--bookmark", bookmark])
    tl(repo, *args)


def git_commit(repo: Path, rel_path: str, content: str, message: str):
    write_file(repo / rel_path, content)
    git(repo, "add", ".")
    git(repo, "commit", "-m", message)


def prepare_empty_dir(repo: Path, _: dict):
    pass


def prepare_tl_started(repo: Path, _: dict):
    start_daemon(repo)


def prepare_tl_repo(repo: Path, _: dict):
    pass


def prepare_tl_flush(repo: Path, _: dict):
    start_daemon(repo)
    write_file(repo / "src" / "flush.txt", "flush me\n")
    time.sleep(0.6)


def prepare_tl_with_history(repo: Path, _: dict):
    start_daemon(repo)
    make_checkpoint(repo, "src/app.txt", "checkpoint-one\n")
    make_checkpoint(repo, "src/app.txt", "checkpoint-two\n")
    stop_daemon(repo)


def prepare_tl_pin(repo: Path, _: dict):
    start_daemon(repo)
    make_checkpoint(repo, "src/app.txt", "pin-me\n")
    stop_daemon(repo)


def prepare_tl_unpin(repo: Path, _: dict):
    prepare_tl_pin(repo, {})
    tl(repo, "pin", "HEAD", "bench-pin")


def prepare_tl_publish(repo: Path, _: dict):
    start_daemon(repo)
    make_checkpoint(repo, "src/app.txt", "publish-me\n")
    stop_daemon(repo)


def prepare_tl_push(repo: Path, _: dict):
    prepare_tl_publish(repo, {})
    publish_head(repo)


def prepare_tl_pull_fetch_only(repo: Path, _: dict):
    pass


def prepare_git_fetch_only(repo: Path, _: dict):
    pass


def prepare_remote_update_for_pull(repo: Path, ctx: dict):
    incoming = repo.parent / "incoming"
    git(repo.parent, "clone", ctx["remote_url"], str(incoming))
    configure_git_identity(incoming)
    git_commit(incoming, "src/remote.txt", f"remote-update-{time.time_ns()}\n", "remote update")
    git(incoming, "push", "origin", "main")
    shutil.rmtree(incoming, ignore_errors=True)


def prepare_tl_pull(repo: Path, ctx: dict):
    start_daemon(repo)
    make_checkpoint(repo, "src/local.txt", "local changes before pull\n")
    stop_daemon(repo)
    prepare_remote_update_for_pull(repo, ctx)


def prepare_git_pull(repo: Path, ctx: dict):
    write_file(repo / "src" / "local.txt", "local changes before pull\n")
    prepare_remote_update_for_pull(repo, ctx)


def prepare_branch_create(repo: Path, _: dict):
    prepare_tl_publish(repo, {})
    publish_head(repo)


def prepare_branch_delete(repo: Path, _: dict):
    prepare_branch_create(repo, {})
    tl(repo, "branch", "--create", "bench-branch", "--at", "HEAD")


def prepare_git_branch_create(repo: Path, _: dict):
    pass


def prepare_git_branch_delete(repo: Path, _: dict):
    git(repo, "branch", "bench-branch", "HEAD")


def prepare_tag_create(repo: Path, _: dict):
    pass


def prepare_tag_show_delete_push(repo: Path, _: dict):
    git(repo, "tag", "-a", "bench-v1", "-m", "bench tag")


def prepare_tl_tag_push(repo: Path, _: dict):
    tl(repo, "tag", "create", "bench-v1", "-m", "bench tag")


def prepare_stash_push(repo: Path, _: dict):
    write_file(repo / "src" / "stash.txt", "stash me\n")


def prepare_tl_stash_apply(repo: Path, _: dict):
    write_file(repo / "src" / "stash.txt", "stash me\n")
    tl(repo, "stash", "push", "-m", "bench stash")


def prepare_git_stash_apply(repo: Path, _: dict):
    write_file(repo / "src" / "stash.txt", "stash me\n")
    git(repo, "stash", "push", "-m", "bench stash")


def prepare_tl_stash_drop(repo: Path, _: dict):
    prepare_tl_stash_apply(repo, {})


def prepare_git_stash_drop(repo: Path, _: dict):
    prepare_git_stash_apply(repo, {})


def prepare_tl_stash_clear(repo: Path, _: dict):
    write_file(repo / "src" / "stash.txt", "stash one\n")
    tl(repo, "stash", "push", "-m", "bench stash 1")
    write_file(repo / "src" / "stash.txt", "stash two\n")
    tl(repo, "stash", "push", "-m", "bench stash 2")


def prepare_git_stash_clear(repo: Path, _: dict):
    write_file(repo / "src" / "stash.txt", "stash one\n")
    git(repo, "stash", "push", "-m", "bench stash 1")
    write_file(repo / "src" / "stash.txt", "stash two\n")
    git(repo, "stash", "push", "-m", "bench stash 2")


def prepare_remote_add(repo: Path, ctx: dict):
    git(repo, "remote", "remove", "bench", check=False)
    ctx["bench_remote_url"] = ctx["dummy_remote_url"]


def prepare_remote_list_get_url(repo: Path, ctx: dict):
    git(repo, "remote", "remove", "bench", check=False)
    git(repo, "remote", "add", "bench", ctx["dummy_remote_url"])


def prepare_remote_set_url(repo: Path, ctx: dict):
    prepare_remote_list_get_url(repo, ctx)


def prepare_remote_rename(repo: Path, ctx: dict):
    prepare_remote_list_get_url(repo, ctx)


def prepare_remote_remove(repo: Path, ctx: dict):
    prepare_remote_list_get_url(repo, ctx)


def prepare_config_set(repo: Path, _: dict):
    pass


def prepare_config_get(repo: Path, _: dict):
    tl(repo, "config", "--set", "checkpoint.interval_secs=7")


def prepare_git_config_get(repo: Path, _: dict):
    git(repo, "config", "benchmark.interval", "7")


def prepare_tl_worktree_list(repo: Path, _: dict):
    pass


def prepare_tl_worktree_add(repo: Path, _: dict):
    pass


def prepare_tl_worktree_remove(repo: Path, _: dict):
    tl(repo, "worktree", "add", "bench-ws")


def benchmark_case(case: Case, _: Path | None, ctx: dict) -> dict:
    samples = []
    for i in range(case.reps):
        repo = ctx["local_origin"].parent / f"{case.name.replace('/', '_')}-{case.runner}-{i}"
        try:
            kill_bench_daemons(ctx["work_root"])
            git(repo.parent, "clone", str(ctx["local_origin"]), str(repo))
            configure_git_identity(repo)
            git(repo, "remote", "set-url", "origin", ctx["remote_url"])
            if case.runner == "tl":
                tl(repo, "init", capture_output=False)
                tl(repo, "stop", check=False, capture_output=False)
                clean_tl_state(repo)
            case.setup(repo, ctx)
            start = time.perf_counter()
            proc = run(case.command, cwd=repo, check=True)
            elapsed = time.perf_counter() - start
            samples.append(elapsed)
            if case.runner == "tl":
                stop_daemon(repo)
        except Exception as exc:
            daemon_log = repo / ".tl" / "logs" / "daemon.log"
            if daemon_log.exists():
                try:
                    print(f"--- daemon log for {case.name} ---", file=sys.stderr)
                    print(daemon_log.read_text(encoding='utf-8', errors='replace'), file=sys.stderr)
                    print(f"--- end daemon log for {case.name} ---", file=sys.stderr)
                except Exception:
                    pass
            raise
        finally:
            kill_bench_daemons(ctx["work_root"])
            shutil.rmtree(repo, ignore_errors=True)
    return summarize(samples)


def fresh_empty_repo_dirs(work: Path, count: int) -> list[Path]:
    dirs = []
    for i in range(count):
        path = work / f"init-empty-{i}"
        path.mkdir(parents=True, exist_ok=True)
        dirs.append(path)
    return dirs


def benchmark_init_pair(work: Path) -> dict:
    tl_samples = []
    git_samples = []
    for i in range(LOCAL_REPS):
        kill_bench_daemons(work)
        tl_repo = work / f"tl-init-{i}"
        git_repo = work / f"git-init-{i}"
        tl_repo.mkdir(parents=True, exist_ok=True)
        git_repo.mkdir(parents=True, exist_ok=True)

        start = time.perf_counter()
        tl(tl_repo, "init", capture_output=False)
        tl_samples.append(time.perf_counter() - start)
        stop_daemon(tl_repo)

        start = time.perf_counter()
        git(git_repo, "init")
        git_samples.append(time.perf_counter() - start)
        kill_bench_daemons(work)
    return {
        "tl_init": summarize(tl_samples),
        "git_init": summarize(git_samples),
    }


def build_cases(ctx: dict) -> list[Case]:
    remote_url = ctx["dummy_remote_url"]
    remote_url_2 = ctx["dummy_remote_url_2"]
    return [
        Case("tl_status", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "status"], prepare_tl_started),
        Case("tl_info", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "info"], prepare_tl_started),
        Case("tl_flush", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "flush"], prepare_tl_flush),
        Case("tl_log", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "log"], prepare_tl_with_history),
        Case("tl_show_head", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "show", "HEAD"], prepare_tl_with_history),
        Case("tl_diff_head", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "diff", "HEAD~1", "HEAD", "-p"], prepare_tl_with_history),
        Case("tl_restore", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "restore", "HEAD~1", "-y"], prepare_tl_with_history),
        Case("tl_pin", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "pin", "HEAD", "bench-pin"], prepare_tl_pin),
        Case("tl_unpin", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "unpin", "bench-pin"], prepare_tl_unpin),
        Case("tl_gc", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "gc"], prepare_tl_with_history),
        Case("tl_publish", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "publish", "HEAD"], prepare_tl_publish),
        Case("tl_worktree_list", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "worktree", "list"], prepare_tl_worktree_list),
        Case("tl_worktree_add", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "worktree", "add", "bench-ws"], prepare_tl_worktree_add),
        Case("tl_worktree_remove", "tl_only", LOCAL_REPS, "tl", [str(TL_BIN), "worktree", "remove", "bench-ws", "-y"], prepare_tl_worktree_remove),
        Case("tl_branch_list", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "branch"], prepare_tl_publish, compare_key="git_branch_list"),
        Case("git_branch_list", "parity", LOCAL_REPS, "git", ["git", "branch"], prepare_git_branch_create),
        Case("tl_branch_create", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "branch", "--create", "bench-branch", "--at", "HEAD"], prepare_branch_create, compare_key="git_branch_create"),
        Case("git_branch_create", "parity", LOCAL_REPS, "git", ["git", "branch", "bench-branch", "HEAD"], prepare_git_branch_create),
        Case("tl_branch_delete", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "branch", "--delete", "bench-branch"], prepare_branch_delete, compare_key="git_branch_delete"),
        Case("git_branch_delete", "parity", LOCAL_REPS, "git", ["git", "branch", "-D", "bench-branch"], prepare_git_branch_delete),
        Case("tl_tag_list", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "tag", "list"], prepare_tag_create, compare_key="git_tag_list"),
        Case("git_tag_list", "parity", LOCAL_REPS, "git", ["git", "tag"], prepare_tag_create),
        Case("tl_tag_create", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "tag", "create", "bench-v1", "-m", "bench tag"], prepare_tag_create, compare_key="git_tag_create"),
        Case("git_tag_create", "parity", LOCAL_REPS, "git", ["git", "tag", "-a", "bench-v1", "-m", "bench tag"], prepare_tag_create),
        Case("tl_tag_show", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "tag", "show", "bench-v1"], prepare_tag_show_delete_push, compare_key="git_tag_show"),
        Case("git_tag_show", "parity", LOCAL_REPS, "git", ["git", "show", "bench-v1", "--quiet"], prepare_tag_show_delete_push),
        Case("tl_tag_delete", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "tag", "delete", "bench-v1"], prepare_tag_show_delete_push, compare_key="git_tag_delete"),
        Case("git_tag_delete", "parity", LOCAL_REPS, "git", ["git", "tag", "-d", "bench-v1"], prepare_tag_show_delete_push),
        Case("tl_stash_list", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "stash", "list"], prepare_tag_create, compare_key="git_stash_list"),
        Case("git_stash_list", "parity", LOCAL_REPS, "git", ["git", "stash", "list"], prepare_tag_create),
        Case("tl_stash_push", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "stash", "push", "-m", "bench stash"], prepare_stash_push, compare_key="git_stash_push"),
        Case("git_stash_push", "parity", LOCAL_REPS, "git", ["git", "stash", "push", "-m", "bench stash"], prepare_stash_push),
        Case("tl_stash_apply", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "stash", "apply"], prepare_tl_stash_apply, compare_key="git_stash_apply"),
        Case("git_stash_apply", "parity", LOCAL_REPS, "git", ["git", "stash", "apply"], prepare_git_stash_apply),
        Case("tl_stash_drop", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "stash", "drop"], prepare_tl_stash_drop, compare_key="git_stash_drop"),
        Case("git_stash_drop", "parity", LOCAL_REPS, "git", ["git", "stash", "drop"], prepare_git_stash_drop),
        Case("tl_stash_clear", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "stash", "clear", "-y"], prepare_tl_stash_clear, compare_key="git_stash_clear"),
        Case("git_stash_clear", "parity", LOCAL_REPS, "git", ["git", "stash", "clear"], prepare_git_stash_clear),
        Case("tl_remote_list", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "list", "-v"], prepare_remote_list_get_url, compare_key="git_remote_list"),
        Case("git_remote_list", "parity", LOCAL_REPS, "git", ["git", "remote", "-v"], prepare_remote_list_get_url),
        Case("tl_remote_add", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "add", "bench", remote_url], prepare_remote_add, compare_key="git_remote_add"),
        Case("git_remote_add", "parity", LOCAL_REPS, "git", ["git", "remote", "add", "bench", remote_url], prepare_remote_add),
        Case("tl_remote_get_url", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "get-url", "bench"], prepare_remote_list_get_url, compare_key="git_remote_get_url"),
        Case("git_remote_get_url", "parity", LOCAL_REPS, "git", ["git", "remote", "get-url", "bench"], prepare_remote_list_get_url),
        Case("tl_remote_set_url", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "set-url", "bench", remote_url_2], prepare_remote_set_url, compare_key="git_remote_set_url"),
        Case("git_remote_set_url", "parity", LOCAL_REPS, "git", ["git", "remote", "set-url", "bench", remote_url_2], prepare_remote_set_url),
        Case("tl_remote_rename", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "rename", "bench", "bench2"], prepare_remote_rename, compare_key="git_remote_rename"),
        Case("git_remote_rename", "parity", LOCAL_REPS, "git", ["git", "remote", "rename", "bench", "bench2"], prepare_remote_rename),
        Case("tl_remote_remove", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "remote", "remove", "bench"], prepare_remote_remove, compare_key="git_remote_remove"),
        Case("git_remote_remove", "parity", LOCAL_REPS, "git", ["git", "remote", "remove", "bench"], prepare_remote_remove),
        Case("tl_config_set", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "config", "--set", "checkpoint.interval_secs=7"], prepare_config_set, compare_key="git_config_set"),
        Case("git_config_set", "parity", LOCAL_REPS, "git", ["git", "config", "benchmark.interval", "7"], prepare_config_set),
        Case("tl_config_get", "parity", LOCAL_REPS, "tl", [str(TL_BIN), "config", "--get", "checkpoint.interval_secs"], prepare_config_get, compare_key="git_config_get"),
        Case("git_config_get", "parity", LOCAL_REPS, "git", ["git", "config", "benchmark.interval"], prepare_git_config_get),
        Case("tl_fetch_no_sync", "parity", NETWORK_REPS, "tl", [str(TL_BIN), "fetch", "--no-sync"], prepare_tl_pull_fetch_only, compare_key="git_fetch_origin"),
        Case("git_fetch_origin", "parity", NETWORK_REPS, "git", ["git", "fetch", "--quiet", "origin"], prepare_git_fetch_only),
        Case("tl_pull_fetch_only", "parity", NETWORK_REPS, "tl", [str(TL_BIN), "pull", "--fetch-only"], prepare_tl_pull_fetch_only, compare_key="git_fetch_origin_2"),
        Case("git_fetch_origin_2", "parity", NETWORK_REPS, "git", ["git", "fetch", "--quiet", "origin"], prepare_git_fetch_only),
        Case("tl_pull", "parity", NETWORK_REPS, "tl", [str(TL_BIN), "pull"], prepare_tl_pull, compare_key="git_pull"),
        Case("git_pull", "parity", NETWORK_REPS, "git", ["git", "pull", "--no-rebase", "origin", "main"], prepare_git_pull),
        Case("tl_push", "parity", NETWORK_REPS, "tl", [str(TL_BIN), "push"], prepare_tl_push, compare_key="git_push"),
        Case("git_push", "parity", NETWORK_REPS, "git", ["git", "push", "origin", "main"], lambda repo, _: git_commit(repo, "src/push.txt", f"git-push-{time.time_ns()}\n", "git push bench")),
        Case("tl_tag_push", "parity", NETWORK_REPS, "tl", [str(TL_BIN), "tag", "push", "bench-v1"], prepare_tl_tag_push, compare_key="git_tag_push"),
        Case("git_tag_push", "parity", NETWORK_REPS, "git", ["git", "push", "origin", "bench-v1"], prepare_tag_show_delete_push),
    ]


def render_markdown(meta: dict, results: dict, comparisons: list[dict]) -> str:
    lines = []
    lines.append("# TL Live Benchmark")
    lines.append("")
    lines.append(f"- Timestamp: {meta['timestamp']}")
    lines.append(f"- GitHub repo: `{meta['owner']}/{meta['repo_name']}`")
    lines.append(f"- TL binary: `{meta['tl_bin']}`")
    lines.append(f"- Local reps: {meta['local_reps']}")
    lines.append(f"- Network reps: {meta['network_reps']}")
    lines.append("")
    lines.append("## Headline Comparisons")
    lines.append("")
    lines.append("| TL case | Git case | TL p50 ms | Git p50 ms | Delta | TL ops/s | Git ops/s |")
    lines.append("|---|---:|---:|---:|---:|---:|---:|")
    for item in comparisons:
        lines.append(
            f"| {item['tl_case']} | {item['git_case']} | "
            f"{item['tl_p50_ms']:.3f} | {item['git_p50_ms']:.3f} | "
            f"{item['delta_pct']:+.1f}% | {item['tl_ops_per_sec']:.3f} | {item['git_ops_per_sec']:.3f} |"
        )
    lines.append("")
    lines.append("## TL-only Baselines")
    lines.append("")
    lines.append("| Case | p50 ms | p90 ms | p99 ms | ops/s |")
    lines.append("|---|---:|---:|---:|---:|")
    for name, stats in sorted(results.items()):
        if not name.startswith("tl_") or name in {"tl_init"}:
            continue
        if any(pair["tl_case"] == name for pair in comparisons):
            continue
        lines.append(
            f"| {name} | {stats['p50_ms']:.3f} | {stats['p90_ms']:.3f} | {stats['p99_ms']:.3f} | {stats['ops_per_sec']:.3f} |"
        )
    return "\n".join(lines) + "\n"


def main():
    ensure_tools()
    RESULTS_DIR.mkdir(parents=True, exist_ok=True)

    owner = gh("api", "user", "-q", ".login").stdout.strip()
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    repo_name = f"tl-live-bench-{stamp.lower()}"
    work = Path(tempfile.mkdtemp(prefix="tl-live-bench-"))
    remote_created = False

    try:
        remote_url = seed_remote(work, owner, repo_name)
        remote_created = True
        ctx = make_templates(work, remote_url)
        cases = build_cases(ctx)

        results = {}
        init_results = benchmark_init_pair(work)
        results.update(init_results)

        for case in cases:
            results[case.name] = benchmark_case(case, None, ctx)

        comparisons = []
        for case in cases:
            if case.runner != "tl" or case.compare_key is None:
                continue
            tl_stats = results[case.name]
            git_stats = results[case.compare_key]
            comparisons.append(
                {
                    "tl_case": case.name,
                    "git_case": case.compare_key,
                    "tl_p50_ms": tl_stats["p50_ms"],
                    "git_p50_ms": git_stats["p50_ms"],
                    "delta_pct": ((tl_stats["p50_ms"] - git_stats["p50_ms"]) / git_stats["p50_ms"] * 100.0) if git_stats["p50_ms"] else 0.0,
                    "tl_ops_per_sec": tl_stats["ops_per_sec"],
                    "git_ops_per_sec": git_stats["ops_per_sec"],
                }
            )
        comparisons.sort(key=lambda item: item["delta_pct"], reverse=True)

        payload = {
            "meta": {
                "timestamp": stamp,
                "owner": owner,
                "repo_name": repo_name,
                "remote_url": remote_url,
                "tl_bin": str(TL_BIN),
                "local_reps": LOCAL_REPS,
                "network_reps": NETWORK_REPS,
                "keep_remote": KEEP_REMOTE,
            },
            "results": results,
            "comparisons": comparisons,
        }

        json_path = RESULTS_DIR / f"live-benchmark-{stamp}.json"
        md_path = RESULTS_DIR / f"live-benchmark-{stamp}.md"
        json_path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        md_path.write_text(render_markdown(payload["meta"], results, comparisons), encoding="utf-8")

        print(json.dumps({"json": str(json_path), "markdown": str(md_path), "repo": f"{owner}/{repo_name}"}, indent=2))
    finally:
        if remote_created and not KEEP_REMOTE:
            gh("repo", "delete", f"{owner}/{repo_name}", "--yes", check=False)
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as exc:
        print(str(exc), file=sys.stderr)
        sys.exit(1)
