#!/bin/sh
set -euo pipefail

# RPM post-uninstall: remove the qubox system user and group
if getent passwd qubox >/dev/null 2>&1; then
    userdel qubox || true
fi
if getent group qubox >/dev/null 2>&1; then
    groupdel qubox || true
fi
