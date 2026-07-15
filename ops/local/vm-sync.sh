#!/usr/bin/env bash
# Sync the host-side project tree into the Windows VM so it can be built
# there with MSVC. Uses vagrant's rsync-over-ssh if available, else
# falls back to a tar over stdin via vagrant ssh-config + scp.
#
# Usage:
#   ops/local/vm-sync.sh                 # sync to default location
#   VM_DEST='C:\\src\\qubox' ops/local/vm-sync.sh
set -euo pipefail

cd "$(dirname "$0")/../.."
REPO_ROOT="$(pwd)"
cd "$REPO_ROOT/scratch"

VM_DEST="${VM_DEST:-C:\\Users\\vagrant\\qubox}"

if ! vagrant status >/dev/null 2>&1; then
    echo "error: no vagrant VM running. Run 'cd scratch && vagrant up' first." >&2
    exit 1
fi

# Build a tarball excluding build artifacts, Vagrant state, and the .git
# history (we only need the working tree, not git objects). We push it via
# 'vagrant upload' (which uses the vagrant ssh transport; WinRM doesn't
# carry tar streams cleanly) and extract on the guest with bsdtar.
TAR="$(mktemp -t qubox-sync.XXXXXX.tar.gz)"
trap 'rm -f "$TAR"' EXIT

echo "[sync] creating tarball..."

# Build a list of exclude patterns. Always-on heavy excludes:
ALWAYS_EXCLUDE=(
    '.git'             # entire git history
    'target'           # cargo build outputs (per .gitignore)
    '.vagrant'         # vagrant state
    '.local'           # local dev state (identities, etc.)
    'node_modules'     # JS deps
    '.DS_Store'        # macOS noise
    'Thumbs.db'        # Windows noise
    'dist'             # build outputs
)

# Plus everything git considers untracked in the working tree (this catches
# /local.bak, /apps/qubox-client-gui/node_modules, generated files, etc.).
GIT_EXCLUDE_FILE="$(mktemp -t qubox-gitexcl.XXXXXX)"
trap 'rm -f "$TAR" "$GIT_EXCLUDE_FILE"' EXIT
git -C "$REPO_ROOT" ls-files -o --directory > "$GIT_EXCLUDE_FILE" 2>/dev/null || true

tar \
    "${ALWAYS_EXCLUDE[@]/#/--exclude=}" \
    -czf "$TAR" \
    -C "$REPO_ROOT" \
    --transform 's,^\.,qubox,' \
    --exclude-from="$GIT_EXCLUDE_FILE" \
    .

SIZE=$(du -h "$TAR" | awk '{print $1}')
echo "[sync] tarball: $TAR ($SIZE)"

# vagrant upload (2.2.7+) is the cleanest way: puts file in guest, no scp needed.
if vagrant upload "$TAR" "$VM_DEST\\qubox-src.tar.gz" 2>/dev/null; then
    echo "[sync] uploaded via 'vagrant upload' to $VM_DEST\\qubox-src.tar.gz"
else
    # Fallback: scp through vagrant ssh-config
    echo "[sync] vagrant upload failed; using scp fallback"
    TMP_REMOTE=/tmp/qubox-src.tar.gz
    vagrant scp "$TAR" "default:$TMP_REMOTE"
    # Move from vagrant home to VM_DEST via WinRM
    vagrant winrm --command "Move-Item -Force C:\\Users\\vagrant\\$TMP_REMOTE $VM_DEST\\qubox-src.tar.gz" >/dev/null
    echo "[sync] scp+move complete"
fi

echo "[sync] extracting on guest..."
vagrant winrm --command "
    if (-not (Test-Path '$VM_DEST')) { New-Item -ItemType Directory -Force -Path '$VM_DEST' | Out-Null }
    Remove-Item -Recurse -Force '$VM_DEST\\qubox' -ErrorAction SilentlyContinue
    # use tar (Windows 11 24H2 ships bsdtar)
    tar -xzf '$VM_DEST\\qubox-src.tar.gz' -C '$VM_DEST'
    Get-ChildItem '$VM_DEST\\qubox' | Select-Object -First 10 Name
"

echo "[sync] OK. Workspace at: $VM_DEST\\qubox"