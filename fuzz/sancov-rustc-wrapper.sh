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

# Instrument both the Python binding and the underlying YAML parser. Also turn
# on overflow checks and debug assertions in them: release builds silently wrap
# on integer overflow and skip debug_assert!, so make those panic instead (a
# Rust panic surfaces as pyo3's PanicException, which the harness doesn't catch).
if [ "$CRATE_NAME" = "ryaml" ] || [ "$CRATE_NAME" = "libyaml_safer" ]; then
    exec "$RUSTC" "$@" \
        -Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=4 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Cllvm-args=-sanitizer-coverage-pc-table \
        -Cllvm-args=-sanitizer-coverage-trace-compares \
        -Z sanitizer=address \
        -C overflow-checks=on \
        -C debug-assertions=on
else
    exec "$RUSTC" "$@"
fi
