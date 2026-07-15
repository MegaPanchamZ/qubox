#!/bin/sh
set -euo pipefail

# RPM pre-install: create the qubox system user if absent
if ! getent passwd qubox >/dev/null 2>&1; then
    useradd --system --home-dir /var/lib/qubox \
        --no-create-home --shell /usr/sbin/nologin \
        --user-group qubox
fi
