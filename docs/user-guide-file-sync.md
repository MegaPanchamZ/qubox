# User guide: Context-aware file sync (ADR-022)

Qubox can sync project files between **paired** devices over the same native QUIC
session used for remote desktop. Sync is **process-locked**, **event-driven**,
and **never** stores file blobs on the signaling server.

## What never syncs by default

Global **never-track** patterns are seeded on first use and always include:

| Pattern | Why |
|---------|-----|
| `.git`, `.git/**` | Live Git indexes must not be binary-synced (use Git) |
| `.svn`, `.hg` | Same for other VCS |
| `node_modules/**`, `target/**` | Huge build artifacts |
| `*.tmp`, `*.part`, `*~`, `*.qubox-partial` | In-progress / temp files |
| `.DS_Store`, `Thumbs.db` | OS junk |

You can add more paths, globs, or folder names from the GUI or CLI. Removing
`.git` is allowed but **strongly discouraged**.

## CLI

Daemon must be running (`qubox-daemon run`).

```bash
# List never-track patterns
qubox-daemon sync list-ignores
# or via client CLI
qubox-client-cli sync list-ignores

# Add / remove
qubox-daemon sync add-ignore '*.rom'
qubox-daemon sync remove-ignore '*.rom'
qubox-daemon sync set-ignores --pattern .git --pattern '*.tmp' --pattern node_modules

# Named presets (merged into current list)
qubox-daemon sync apply-preset default
qubox-daemon sync apply-preset git
qubox-daemon sync apply-preset emulator-saves   # ROMs ignored, saves ok
qubox-daemon sync apply-preset dev              # node_modules, target, etc.

# Rules, jobs, conflicts
qubox-daemon sync add-rule --path ~/saves --process mgba --peer <peer-id>
qubox-daemon sync list-rules
qubox-daemon sync list-jobs
qubox-daemon sync list-conflicts
qubox-daemon sync resolve-conflict <id> --resolution keep-local   # keep-remote|keep-both

# Manual push (queues outbox until peer session is up)
qubox-daemon sync push /path/to/game.sav --peer <peer-id> --node-id my-laptop
```

## GUI

Open **File Sync** in the sidebar:

1. **Never track** — list, add, remove, apply presets (`.git` default).
2. **Conflicts** — Keep local / Keep remote / Keep both.
3. **Watch rules** — paths, process lock names, target peers.
4. **Manual push** + **Outbox jobs**.

## Folder conventions

| Use case | Suggested rule path | Process locks | Extra ignores |
|----------|---------------------|---------------|---------------|
| Emulator saves | `~/Games/*/saves` | `mgba`, `retroarch` | `*.gba`, `*.nes` (preset `emulator-saves`) |
| DAW project (not VST cache) | `~/Music/Projects` | `Ableton Live`, `REAPER` | `*.tmp`, `Cache/**` |
| Work tree **without** `.git` | Project root | editor name | Keep default `.git` ignore |

## Live transfer (session up)

When both peers have a native QUIC remote session:

1. Queue work with `sync push` or a watch rule.
2. Host and client each run a **FileSync drain** loop (automatic).
3. Files land in `QUBOX_FILESYNC_DIR` if set, else platform data dir
   `…/qubox/incoming`.
4. Outbox jobs move Queued → InFlight → Done (or Failed + retry).

Process-locked files stay in the outbox until unlock.

## Safety rules

- Only **allowlisted** roots from your rules are watched.
- Symlinks that escape the root are refused.
- Opaque binaries: concurrent edits → **conflict fork**
  `{name}.conflict.{peer}.{utc}.{ext}` — no silent merge.
- Do **not** rely on mtime alone (clock skew).
- Files sync when both peers are **online with a QUIC session** (or a future
  sync-only session). Offline changes queue in the local daemon outbox.

## Emulator version note

Save formats can differ between emulator versions. Prefer matching versions on
both machines, or resolve conflicts manually after testing the save.

## Related

- ADR: `research/decisions/ADR-022-context-aware-file-sync.md`
- Host sensors: build with `--features file-sync` on `qubox-host-agent`
