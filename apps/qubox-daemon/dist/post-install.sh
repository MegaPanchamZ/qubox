#!/bin/sh
set -euo pipefail

# RPM post-install: enable and start the systemd service
systemctl daemon-reload
systemctl enable qubox.service
systemctl start qubox.service || true
