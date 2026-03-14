#!/bin/bash
# sancov-rustc-wrapper.sh

# $1 is the path to rustc (passed by cargo when using RUSTC_WRAPPER)
RUSTC="$1"
shift

# Check --crate-name in the arguments
CRATE_NAME=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--crate-name" ]; then
        CRATE_NAME="$arg"
        break
    fi
    prev="$arg"
done

# Only instrument your crate
if [ "$CRATE_NAME" = "ryaml" ]; then
    exec "$RUSTC" "$@" \
        -Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=4 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Cllvm-args=-sanitizer-coverage-trace-compares \
        -Z sanitizer=address
else
    exec "$RUSTC" "$@"
fi
