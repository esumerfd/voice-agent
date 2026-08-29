#!/usr/bin/env bash
# Deliberately trivial (D-12): proves the ProcessService spawn/collect flow,
# not calendar fidelity. Emits today's date as a single JSON object.
set -euo pipefail

printf '{"date": "%s"}\n' "$(date +%Y-%m-%d)"
