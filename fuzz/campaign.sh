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
    for pass in ac1 asan; do
        if [ "$pass" = ac1 ]; then
            # #623 AC1 pass: no sanitizer, so RSS is the heap plus a small runtime baseline and the
            # 256 MiB limit measures an input's memory rather than ASAN's quarantine growth. This is
            # the pass AC1's zero-OOM and 2 s-per-input criteria are read from.
            sanitizer="--sanitizer none"
            rss=256
        else
            # Memory-safety pass: ASAN. Its quarantine and shadow keep RSS far above the heap, so
            # 2048 is this pass's process cap, not AC1's per-input bound.
            sanitizer=""
            rss=2048
        fi
        before="$(ls -1 "$artifacts" | wc -l | tr -d ' ')"
        log="$(mktemp)"
        code=0
        (
            cd "$here"
            # shellcheck disable=SC2086 # $sanitizer is empty for the ASAN pass by design
            cargo +nightly fuzz run "$target" $sanitizer -- \
                -max_total_time="$seconds" -timeout=2 -rss_limit_mb="$rss" -malloc_limit_mb=256
        ) >"$log" 2>&1 || code=$?
        after="$(ls -1 "$artifacts" | wc -l | tr -d ' ')"
        execs="$(sed -n 's/^Done \([0-9][0-9]*\) runs.*/\1/p' "$log" | tail -1)"
        echo "target=$target pass=$pass seconds=$seconds exit=$code execs=${execs:-none} new_artifacts=$((after - before))"
        if [ "$code" -ne 0 ]; then
            echo "--- $target ($pass): last 20 log lines ---"
            tail -20 "$log"
            status=1
        fi
        rm -f "$log"
    done
done

exit "$status"
