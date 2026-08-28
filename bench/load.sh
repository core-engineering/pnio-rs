#!/usr/bin/env bash
# CPU + memory load on the housekeeping cores only (never on the isolated RT core).
set -euo pipefail
SECS="${1:-600}"
HK_CPUS="${HK_CPUS:-0-2}"
exec taskset -c "$HK_CPUS" stress-ng --cpu 3 --vm 1 --vm-bytes 512M --timeout "${SECS}s" --metrics-brief
