#!/usr/bin/env bash
set -eux
IFS=''

rtic_scope=$(realpath $1)
pushd $(dirname "$0")/expected >/dev/null

# For each --bin, `trace --resolve-only` it, and compare with expected
# output.
cp ./manifests/general.toml Cargo.toml
for bin in src/bin/*.rs; do
    bin=$(basename "$bin" .rs)
    cargo build --bin $bin # RTIC Scope does not forward compilation errors as expected
    out=$($rtic_scope trace --resolve-only --bin $bin 2>&1 || true)

    # for each (fixed) expected string, ensure it's in the output.
    while read line; do
        echo "$out" | grep -F "$line" >/dev/null || exit 1
    done < ./out/$bin.run
done

# Same as above, but for each manifest with general.rs
for manifest in manifests/*.toml; do
    cp $manifest Cargo.toml
    manifest=$(basename "$manifest" .toml)
    out=$($rtic_scope trace --resolve-only --bin general 2>&1 || true)

    # for each (fixed) expected string, ensure it's in the output.
    while read line; do
        echo "$out" | grep -F "$line" >/dev/null || exit 1
    done < ./out/$manifest.run
done

# Test expected trace output
for tracefile in traces/*.trace; do
    PATH=$PATH:$HOME/.cargo/bin
    out=$($rtic_scope replay --trace-file $tracefile 2>&1)
    name=$(basename "$tracefile" .trace)

    # for each (fixed) expected string, ensure it's in the output.
    while read line; do
        echo "$out" | grep -F "$line" >/dev/null || exit 1
    done < ./out/trace-$name.run
done

popd >/dev/null
exit 0
