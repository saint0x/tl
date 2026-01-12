# Timelapse (tl)

**Automatic version control for AI-assisted development.** Every file change creates a checkpoint you can restore instantly.

Built on [Jujutsu](https://github.com/martinvonz/jj) for Git compatibility.

---

## Quick Start

```bash
# Install
./build.sh install

# Initialize in your project
cd /your/project
tl init

# That's it. Checkpoints are created automatically every 5 seconds.
```

---

## CLI Reference

### Initialization & Daemon

| Command | Description |
|---------|-------------|
| `tl init` | Initialize timelapse in current directory |
| `tl init --skip-git` | Skip git initialization |
| `tl init --skip-jj` | Skip JJ initialization |
| `tl start` | Start background daemon |
| `tl start --foreground` | Run daemon in foreground (debugging) |
| `tl stop` | Stop background daemon |
| `tl status` | Show daemon and checkpoint status |
| `tl info` | Show detailed repository info |

### Checkpoints

| Command | Description |
|---------|-------------|
| `tl log` | Show checkpoint history (default: 20) |
| `tl log --limit 50` | Show more checkpoints |
| `tl show <id>` | Show detailed checkpoint information |
| `tl show <id> -p` | Show checkpoint with diff |
| `tl flush` | Force immediate checkpoint |
| `tl flush --force` | Create checkpoint even with no changes |
| `tl restore <id>` | Restore to checkpoint (interactive) |
| `tl restore <id> -y` | Restore without confirmation |
| `tl diff <a> <b>` | File-level diff between checkpoints |
| `tl diff <a> <b> -p` | Line-level diff (unified format) |
| `tl diff <a> <b> -p -U 5` | Diff with 5 context lines |

### Pins

| Command | Description |
|---------|-------------|
| `tl pin <id> <name>` | Name a checkpoint |
| `tl unpin <name>` | Remove a pin |

### Git Integration (via JJ)

| Command | Description |
|---------|-------------|
| `tl publish <id>` | Publish checkpoint to JJ |
| `tl publish <id> -b <name>` | Publish with bookmark name |
| `tl publish <id> --compact` | Squash into single commit |
| `tl publish <id> --no-pin` | Don't auto-pin published checkpoint |
| `tl push` | Push to Git remote |
| `tl push -b <name>` | Push specific bookmark |
| `tl push --all` | Push all bookmarks |
| `tl push --force` | Force push |
| `tl pull` | Pull from Git remote |
| `tl pull --fetch-only` | Fetch without merging |
| `tl pull --no-pin` | Don't pin pulled commits |
| `tl fetch` | Fetch from Git remote |
| `tl fetch --no-sync` | Fetch without syncing working directory |
| `tl fetch --prune` | Remove deleted remote branches |

### Workspaces

| Command | Description |
|---------|-------------|
| `tl worktree list` | List all workspaces |
| `tl worktree add <name>` | Create new workspace |
| `tl worktree add <name> --path /custom/path` | Create at specific path |
| `tl worktree add <name> --from <checkpoint>` | Create from checkpoint |
| `tl worktree switch <name>` | Switch to workspace |
| `tl worktree remove <name>` | Remove workspace |
| `tl worktree remove <name> --delete-files` | Remove with files |

### Branches

| Command | Description |
|---------|-------------|
| `tl branch` | List local branches |
| `tl branch -r` | List remote branches |
| `tl branch -a` | List all branches |
| `tl branch --create <name>` | Create new branch |
| `tl branch --create <name> --at <id>` | Create branch at checkpoint |
| `tl branch --delete <name>` | Delete a branch |
| `tl merge <branch>` | Merge changes from branch |
| `tl merge --abort` | Abort in-progress merge |
| `tl merge --continue` | Continue after resolving conflicts |
| `tl resolve` | Check conflict resolution status |
| `tl resolve -l` | List files with conflict status |
| `tl resolve --continue` | Continue merge after resolving |
| `tl resolve --abort` | Abort merge |

### Tags

| Command | Description |
|---------|-------------|
| `tl tag list` | List all tags |
| `tl tag create <name>` | Create tag at HEAD |
| `tl tag create <name> --checkpoint <id>` | Create tag at checkpoint |
| `tl tag create <name> -m "message"` | Create annotated tag |
| `tl tag create <name> --force` | Overwrite existing tag |
| `tl tag delete <name>` | Delete a tag |
| `tl tag show <name>` | Show tag details |
| `tl tag push <name>` | Push tag to remote |
| `tl tag push --all` | Push all tags to remote |

### Stash

| Command | Description |
|---------|-------------|
| `tl stash list` | List all stashes |
| `tl stash push` | Save working changes to stash |
| `tl stash push -m "message"` | Stash with message |
| `tl stash apply` | Apply most recent stash |
| `tl stash apply stash@{1}` | Apply specific stash |
| `tl stash pop` | Apply and remove stash |
| `tl stash drop` | Delete most recent stash |
| `tl stash clear` | Delete all stashes |
| `tl stash clear -y` | Delete all stashes without confirmation |

### Remotes

| Command | Description |
|---------|-------------|
| `tl remote list` | List all remotes |
| `tl remote list -v` | List remotes with URLs |
| `tl remote add <name> <url>` | Add a new remote |
| `tl remote add <name> <url> --fetch` | Add and fetch immediately |
| `tl remote remove <name>` | Remove a remote |
| `tl remote rename <old> <new>` | Rename a remote |
| `tl remote set-url <name> <url>` | Change remote URL |
| `tl remote set-url <name> <url> --push` | Set push URL |
| `tl remote get-url <name>` | Get remote URL |

### Configuration

| Command | Description |
|---------|-------------|
| `tl config` | List all configuration values |
| `tl config --get <key>` | Get a config value |
| `tl config --set <key>=<value>` | Set a config value |
| `tl config --path` | Show config file path |
| `tl config --path --create` | Create config file if missing |
| `tl config --example` | Show example configuration |

### Maintenance

| Command | Description |
|---------|-------------|
| `tl gc` | Garbage collection |

### Checkpoint References

| Format | Example | Description |
|--------|---------|-------------|
| Full ULID | `01HN8XYZABC123...` | Complete identifier |
| Short prefix | `01HN8` | 4+ characters, must be unique |
| Pin name | `my-feature` | Named checkpoint |
| Workspace pin | `ws:feature-name` | Auto-created by workspace |
| HEAD | `HEAD` | Latest checkpoint |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              TIMELAPSE                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                              │
│   ┌──────────────┐         ┌──────────────────────────────────────────┐     │
│   │   CLI (tl)   │         │           Background Daemon              │     │
│   ├──────────────┤         ├──────────────────────────────────────────┤     │
│   │ init         │         │                                          │     │
│   │ log          │◄───────►│  ┌─────────────┐    ┌────────────────┐   │     │
│   │ restore      │   IPC   │  │   Watcher   │    │   Checkpoint   │   │     │
│   │ diff         │ (Unix   │  │  (FSEvents/ │───►│    Creator     │   │     │
│   │ publish      │ Socket) │  │   inotify)  │    │                │   │     │
│   │ push/pull    │         │  └─────────────┘    └───────┬────────┘   │     │
│   └──────────────┘         │         │                   │            │     │
│                            │         ▼                   │            │     │
│                            │  ┌─────────────┐            │            │     │
│                            │  │  Debouncer  │            │            │     │
│                            │  │  (300ms)    │            │            │     │
│                            │  └─────────────┘            │            │     │
│                            └─────────────────────────────┼────────────┘     │
│                                                          │                  │
│   ┌──────────────────────────────────────────────────────┼────────────────┐ │
│   │                         Storage Layer                │                │ │
│   ├──────────────────────────────────────────────────────┼────────────────┤ │
│   │                                                      ▼                │ │
│   │  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐       │ │
│   │  │     Blobs       │  │     Trees       │  │    Journal      │       │ │
│   │  │  (SHA-1, zstd)  │  │  (Git format)   │  │   (Sled DB)     │       │ │
│   │  │                 │  │                 │  │                 │       │ │
│   │  │ .tl/objects/    │  │ .tl/objects/    │  │ .tl/journal/    │       │ │
│   │  │    blobs/       │  │    trees/       │  │                 │       │ │
│   │  └─────────────────┘  └─────────────────┘  └─────────────────┘       │ │
│   └──────────────────────────────────────────────────────────────────────┘ │
│                                                                              │
│   ┌──────────────────────────────────────────────────────────────────────┐ │
│   │                      JJ Integration Layer                             │ │
│   ├──────────────────────────────────────────────────────────────────────┤ │
│   │                                                                       │ │
│   │  tl publish              tl push/pull                                │ │
│   │       │                       │                                      │ │
│   │       ▼                       ▼                                      │ │
│   │  ┌─────────┐            ┌─────────┐            ┌─────────────┐       │ │
│   │  │Checkpoint│───────────►│   JJ    │───────────►│  Git Remote │       │ │
│   │  │  → JJ   │            │ Commits │            │  (GitHub)   │       │ │
│   │  └─────────┘            └─────────┘            └─────────────┘       │ │
│   │                                                                       │ │
│   └──────────────────────────────────────────────────────────────────────┘ │
│                                                                              │
└─────────────────────────────────────────────────────────────────────────────┘
```

### Data Flow

1. **File Watcher** detects changes (FSEvents on macOS, inotify on Linux)
2. **Debouncer** coalesces rapid changes (300ms window per file)
3. **Checkpoint Creator** hashes only changed files (O(k) complexity)
4. **Storage Layer** deduplicates via content-addressing (SHA-1)
5. **JJ Integration** publishes checkpoints as Git-compatible commits

### Storage Model

Timelapse uses Git's proven content-addressing model:

| Object | Format | Storage |
|--------|--------|---------|
| **Blobs** | SHA-1 hash, zstd compressed | `.tl/objects/blobs/` |
| **Trees** | Git tree format, sorted entries | `.tl/objects/trees/` |
| **Checkpoints** | ULID + tree hash + parent ref | `.tl/journal/` (Sled DB) |

**Deduplication**: Identical file content stored once. Identical directory states share the same tree hash.

### Performance

| Operation | Target | Achieved |
|-----------|--------|----------|
| Checkpoint creation | <10ms | ~3ms |
| Restore | <100ms | ~60ms |
| Watcher throughput | >10k events/sec | 11k/sec |
| Memory (idle) | <10MB | ~8MB |
| Storage overhead | - | ~1.2x vs Git |

---

## Jujutsu Integration

Timelapse uses [Jujutsu (JJ)](https://github.com/martinvonz/jj) as the bridge to Git, providing:

- **Git compatibility** without implementing Git protocol
- **Atomic operations** via MVCC transactions
- **Conflict-free merging** inherited from JJ
- **Standard workflows** via `git push/pull`

### Checkpoint → Git Flow

```
Timelapse Checkpoints (100s/day)
         ↓
    tl publish
         ↓
   JJ Commits (10s/day)
         ↓
    tl push
         ↓
   Git Commits → GitHub/GitLab
```

### Typical Workflow

```bash
# Work normally - checkpoints created automatically
# ...make changes...

# Ready to push
tl publish HEAD              # Publish latest checkpoint to JJ
tl push                      # Push to Git remote

# Or publish multiple checkpoints as one commit
tl publish HEAD~10 --compact -b feature-name
tl push -b feature-name
```

---

## Configuration

### Ignore Patterns

Timelapse automatically ignores:
- Editor temp files (`.swp`, `~`, `#*#`)
- IDE directories (`.vscode/`, `.idea/`)
- Build directories (`node_modules/`, `target/`, `__pycache__/`)
- System files (`.DS_Store`, `Thumbs.db`)
- VCS directories (`.tl/`, `.git/`, `.jj/`)

Custom patterns via `.tlignore` (gitignore syntax):
```
/build/
/dist/
*.log
```

### Config File

`.tl/config` (TOML):
```toml
[watcher]
debounce_ms = 300

[retention]
default_keep_count = 1000
default_keep_duration = "30d"
pinned_keep_forever = true

[storage]
compression_level = 3
```

---

## Development

```bash
# Build
./build.sh check      # Fast compile check
./build.sh debug      # Debug build
./build.sh release    # Release build
./build.sh install    # Build + install to PATH
./build.sh info       # Show current binary info

# Test
./test.sh test-quick  # Fast tests (~10s)
./test.sh test-all    # Full suite (~2min)
./test.sh test-jj     # JJ integration tests
./test.sh ci          # Full CI pipeline
```

### Crate Structure

| Crate | Purpose |
|-------|---------|
| `core` | Content-addressed storage (blobs, trees) |
| `watcher` | File system monitoring |
| `journal` | Checkpoint management (Sled DB) |
| `jj` | Jujutsu integration |
| `cli` | Command-line interface |

---

## Requirements

- macOS (FSEvents) or Linux (inotify)
- Rust 1.75+
- Git (for JJ integration)

## License

MIT or Apache-2.0
