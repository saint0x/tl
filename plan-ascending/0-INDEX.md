# Timelapse Implementation Roadmap - Index

## Overview

This directory contains the complete, detailed implementation checklist for **Timelapse**, a low-latency repository checkpoint system built in Rust.

**Key Objectives:**
- Content-addressed storage (tree + blob model)
- Extreme low latency (< 10ms checkpoint creation)
- Minimal memory footprint (< 50MB active, key selling point)
- Cross-platform (macOS + Linux)
- JJ integration for Git interoperability

## Phase Files

Each numbered file contains a complete checklist for one major phase of development. Phases build on each other sequentially.

### [1.md](./1.md) - Foundation & Core Storage ✅ COMPLETE
**Dependencies:** None (starting point)
**Estimated Scope:** ~40% of total work
**Status:** 72 tests passing (67 unit + 5 integration)

**Key Deliverables:**
- Cargo workspace structure
- BLAKE3 hashing with SIMD
- Blob storage with memory pooling
- Tree representation with incremental updates
- On-disk store layout (`.tl/` directory)
- Atomic write operations

**Critical Files Created:**
- `Cargo.toml` (workspace root)
- `crates/core/src/hash.rs`
- `crates/core/src/blob.rs`
- `crates/core/src/tree.rs`
- `crates/core/src/store.rs`

**Performance Targets:**
- Blob operations: < 5ms for typical files
- Tree serialization: < 5ms for 10k-entry tree
- Memory: < 10MB idle

---

### [2.md](./2.md) - File System Watcher
**Dependencies:** Phase 1 (core)
**Estimated Scope:** ~25% of total work

**Key Deliverables:**
- Platform abstraction (macOS FSEvents + Linux inotify)
- Per-path debouncing (200-500ms configurable)
- Event coalescing (merge duplicate events)
- Overflow recovery (targeted rescan)
- Path interning (memory optimization)

**Critical Files Created:**
- `crates/watcher/src/platform/mod.rs`
- `crates/watcher/src/platform/macos.rs`
- `crates/watcher/src/platform/linux.rs`
- `crates/watcher/src/debounce.rs`
- `crates/watcher/src/coalesce.rs`

**Performance Targets:**
- Event processing: > 10k events/sec
- Debounce latency: < 500ms (300ms default)
- Memory: < 20MB under load (8192-event ring buffer)

---

### [3.md](./3.md) - Checkpoint Journal & Incremental Updates
**Dependencies:** Phase 1 (core)
**Estimated Scope:** ~20% of total work

**Key Deliverables:**
- Checkpoint data structures (ULID-based IDs)
- Append-only journal (sled embedded DB)
- PathMap state cache
- **Incremental tree update algorithm** (performance linchpin)
- Retention policies & GC (mark & sweep)

**Critical Files Created:**
- `crates/journal/src/checkpoint.rs`
- `crates/journal/src/journal.rs`
- `crates/journal/src/pathmap.rs`
- `crates/journal/src/incremental.rs`
- `crates/journal/src/retention.rs`

**Performance Targets:**
- Checkpoint creation: < 10ms for small changes (1-5 files)
- Journal lookup: < 1ms
- GC: < 5 seconds for 10k checkpoints

---

### [4.md](./4.md) - CLI & Daemon
**Dependencies:** Phases 1-3 (all crates)
**Estimated Scope:** ~10% of total work

**Key Deliverables:**
- Complete CLI (`tlinit`, `status`, `log`, `diff`, `restore`, `pin`, `gc`)
- Background daemon with IPC
- Daemon lifecycle management (start/stop)
- Lock file handling (prevent concurrent daemons)
- Structured logging & metrics

**Critical Files Created:**
- `crates/cli/src/main.rs`
- `crates/cli/src/daemon.rs`
- `crates/cli/src/ipc.rs`
- `crates/cli/src/cmd/init.rs`
- `crates/cli/src/cmd/restore.rs`

**Performance Targets:**
- CLI startup: < 50ms
- Daemon memory: < 40MB resident

---

### [5.md](./5.md) - JJ Integration
**Dependencies:** Phases 1-4
**Estimated Scope:** ~5% of total work

**Key Deliverables:**
- Checkpoint → JJ commit materialization
- `tlpublish` (create JJ commit from checkpoint)
- `tlpush` / `tlpull` (Git interop via JJ)
- Checkpoint ↔ JJ commit mapping

**Critical Files Created:**
- `crates/jj/src/lib.rs`
- `crates/jj/src/materialize.rs`
- `crates/jj/src/mapping.rs`
- `crates/cli/src/cmd/publish.rs`
- `crates/cli/src/cmd/push.rs`

**Performance Targets:**
- Publish (materialize): < 100ms for typical checkpoint
- JJ operations add < 5MB to daemon memory

---

## Implementation Strategy

### Sequential Development
Phases must be implemented in order (1 → 2 → 3 → 4 → 5):
- Each phase builds on previous phases
- No parallel phase implementation (dependencies are strict)
- Complete each phase's testing before moving to next

### Memory Optimization Throughout
**Critical Constraint:** Minimize memory footprint (major selling point)

**Techniques Applied Across All Phases:**
1. Pre-allocation (ring buffers, object pools)
2. Zero-copy I/O (memory-mapped files)
3. Compact representations (`SmallVec`, fixed-size types)
4. Lazy loading (defer work until needed)
5. Path interning (`Arc<Path>` deduplication)

**Memory Targets:**
- **Idle daemon:** < 10MB
- **Active (1000 files/sec):** < 50MB
- **Peak (checkpoint creation):** < 100MB

### Performance Benchmarks
Create benchmarks alongside each phase:
- Use `criterion` crate for reproducible benchmarks
- Profile with `heaptrack` / `valgrind --tool=massif`
- Measure both latency and memory

### Testing Strategy
**Per Phase:**
- Unit tests (isolated functionality)
- Integration tests (cross-component)
- Platform-specific tests (macOS/Linux)

**End-to-End:**
- Full workflow: `init → start → edit → log → restore → publish → push`
- Stress tests: high file churn (1000s of changes/sec)
- Crash recovery: kill daemon mid-checkpoint

---

## On-Disk Layout (Final)

```
.tl/
├── config.toml              # User configuration
├── HEAD                     # Current checkpoint ID
├── locks/
│   ├── daemon.lock         # Daemon PID + lock
│   └── gc.lock             # GC exclusive lock
├── journal/
│   ├── checkpoints.db      # sled: append-only checkpoint log
│   └── ops.log.idx         # (optional) sparse index
├── objects/
│   ├── blobs/
│   │   └── <hh>/<rest>     # Content-addressed blobs
│   └── trees/
│       └── <hh>/<rest>     # Content-addressed trees
├── refs/
│   ├── pins/
│   │   ├── last-good       # Named checkpoint pins
│   │   └── pre-push
│   └── heads/
│       └── workspace
├── state/
│   ├── pathmap.bin         # Current tree cache
│   ├── watcher.state       # Watcher cursors
│   ├── jj-mapping.db       # Checkpoint ↔ JJ commit map
│   ├── daemon.sock         # Unix socket for IPC
│   └── metrics.json        # Runtime metrics
├── logs/
│   └── daemon.log          # Daemon logs (rotated)
└── tmp/
    ├── ingest/             # Temp files for atomic writes
    └── gc/                 # GC workspace
```

---

## Key Dependencies (Workspace-Wide)

```toml
[workspace.dependencies]
# Core
blake3 = { version = "1.5", features = ["rayon", "mmap"] }
memmap2 = "0.9"
sled = "0.34"

# Concurrency
tokio = { version = "1.35", features = ["rt-multi-thread", "sync", "time", "fs"] }
parking_lot = "0.12"
crossbeam-channel = "0.5"
dashmap = "5.5"

# Data Structures
smallvec = "1.13"
ahash = "0.8"
bytes = "1.5"

# Serialization
serde = { version = "1.0", features = ["derive"] }
bincode = "1.3"

# File Watching
notify = { version = "6.1", features = ["macos_fsevent"] }

# CLI
clap = { version = "4.4", features = ["derive"] }
tracing = "0.1"
tracing-subscriber = "0.3"
indicatif = "0.17"
owo-colors = "4.0"

# JJ Integration
jj-lib = "0.13"

# Utilities
chrono = "0.4"
ulid = "1.1"
thiserror = "1.0"
anyhow = "1.0"
```

---

## Success Criteria (MVP Complete)

**Functional:**
- [ ] All v1 CLI commands work (`init`, `status`, `log`, `restore`, `pin`, `gc`)
- [ ] File watcher detects changes within debounce window
- [ ] Checkpoint creation is automatic and lossless
- [ ] Restore produces byte-identical working trees
- [ ] JJ integration works (`publish`, `push`)

**Performance:**
- [ ] Checkpoint creation: < 10ms for small changes
- [ ] Restoration: < 100ms for typical tree (1k files)
- [ ] Memory (idle): < 10MB
- [ ] Memory (active): < 50MB
- [ ] Memory (peak): < 100MB

**Correctness:**
- [ ] Survives daemon crash (no data loss)
- [ ] Handles deletes, renames, symlinks, exec bit
- [ ] GC safely removes unreferenced objects
- [ ] Never watches `.tl/` or `.git/` (no recursion)

**Quality:**
- [ ] Comprehensive test coverage (unit + integration)
- [ ] Cross-platform (macOS + Linux tested)
- [ ] User-friendly error messages
- [ ] Documentation complete

---

## Next Steps

1. **Start with Phase 1**: Set up workspace, implement core storage
2. **Profile early**: Measure memory and latency from day one
3. **Test continuously**: Don't move to next phase until current is solid
4. **Keep it simple**: Avoid premature optimization (except memory)
5. **Ship MVP**: Get to working end-to-end before adding features

---

## Reference Documentation

- **PLAN.md**: Full architectural design (Parts 1 & 2)
- **1.md - 5.md**: Detailed implementation checklists
- **This file (0-INDEX.md)**: Overview and navigation

For any questions about design decisions, refer back to `PLAN.md` Part 2 (lines 187-632) which contains the low-level format specifications.
