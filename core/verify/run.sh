#!/usr/bin/env bash
# Verify the proofs in core/verify with Verus.
#
# usage: core/verify/run.sh [file.rs ...]   every *.rs here unless told
#
# Verus is a prebuilt release, tested with 0.2026.10.04.426d8b0
# (https://github.com/verus-lang/verus/releases). Unzip it and point VERUS at the `verus`
# binary inside, or put it on PATH. Verus runs its pinned toolchain through rustup. Without
# rustup on PATH the script takes it from nixpkgs, and RUSTUP_HOME decides where the
# toolchain goes, ~/.rustup unless told.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
verus=${VERUS:-$(command -v verus || true)}
if [ -z "$verus" ] || [ ! -x "$verus" ]; then
    echo "verus not found, set VERUS=/path/to/verus-x86-linux/verus" >&2
    exit 2
fi

files=("$@")
if [ ${#files[@]} -eq 0 ]; then
    files=("$here"/*.rs)
fi

run() {
    local status=0
    for f in "${files[@]}"; do
        echo "== $f"
        "$verus" "$f" || status=1
    done
    return $status
}

if command -v rustup >/dev/null; then
    run
elif command -v nix >/dev/null; then
    export -f run
    export verus
    files_list=$(printf '%q ' "${files[@]}")
    nix shell nixpkgs#rustup --command bash -c "files=($files_list); run"
else
    echo "rustup not found, Verus needs it for its pinned toolchain" >&2
    exit 2
fi
