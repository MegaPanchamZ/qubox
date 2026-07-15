#!/bin/sh
set -euo pipefail

# RPM pre-uninstall: stop and disable the service
systemctl stop qubox.service || true
systemctl disable qubox.service || true
