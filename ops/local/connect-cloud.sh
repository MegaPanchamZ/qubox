#!/usr/bin/env bash
# Same as connect.sh but pre-configured to use the cloud signaling server
# on the AWS EC2 instance instead of the local one.
#
# Usage:
#   ops/local/connect-cloud.sh my-host
#   ops/local/connect-cloud.sh my-host --codec h265
#
# Override with QUBOX_SERVER if needed:
#   QUBOX_SERVER=ws://some-other-server:7000/ws ops/local/connect-cloud.sh my-host
set -euo pipefail

export QUBOX_SERVER="${QUBOX_SERVER:-ws://13.239.73.205:7000/ws}"
exec "$(dirname "$0")/connect.sh" "$@"