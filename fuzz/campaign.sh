#!/usr/bin/env bash
# One bounded libFuzzer run per target, one summary line each.
#
#   fuzz/campaign.sh [seconds] [target ...]
#
# Defaults to 60 seconds per target for all four targets. Needs nightly Rust, and belongs on the
# remote host rather than a workstation (see README.md).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
seconds="${1:-60}"
if [ "$#" -gt 0 ]; then
    shift
fi
if [ "$#" -gt 0 ]; then
    targets=("$@")
else
    targets=(fuzz_hwp5_read fuzz_hwpx_read fuzz_wmf_dib fuzz_equation_markdown)
fi

export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

if ! command -v cargo-fuzz >/dev/null; then
    echo "cargo-fuzz is not installed; installing it"
    if ! cargo install --locked cargo-fuzz; then
        echo "cargo home is not writable; retrying into $here/.cargo-tools"
        cargo install --locked cargo-fuzz --root "$here/.cargo-tools"
        PATH="$here/.cargo-tools/bin:$PATH"
    fi
    if ! command -v cargo-fuzz >/dev/null; then
        echo "cargo-fuzz installation left no binary on PATH" >&2
        exit 1
    fi
fi

status=0
for target in "${targets[@]}"; do
    artifacts="$here/artifacts/$target"
    mkdir -p "$artifacts"
    before="$(ls -1 "$artifacts" | wc -l | tr -d ' ')"
    log="$(mktemp)"
    code=0
    (
        cd "$here"
        # #623 AC1: zero panics, OOMs (>256 MiB per input) or timeouts (>2 s per input). A malloc
        # limit caps one allocation, not what an input accumulates, so AC1's per-input ceiling is
        # enforced by the RSS limit at its own 256 MiB, not by a looser process cap.
        cargo +nightly fuzz run "$target" -- \
            -max_total_time="$seconds" -timeout=2 -rss_limit_mb=256 -malloc_limit_mb=256
    ) >"$log" 2>&1 || code=$?
    after="$(ls -1 "$artifacts" | wc -l | tr -d ' ')"
    execs="$(sed -n 's/^Done \([0-9][0-9]*\) runs.*/\1/p' "$log" | tail -1)"
    echo "target=$target seconds=$seconds exit=$code execs=${execs:-none} new_artifacts=$((after - before))"
    if [ "$code" -ne 0 ]; then
        echo "--- $target: last 20 log lines ---"
        tail -20 "$log"
        status=1
    fi
    rm -f "$log"
done

exit "$status"
