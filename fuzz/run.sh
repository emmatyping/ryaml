#!/usr/bin/env bash
set -euo pipefail

fuzz_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
fuzz_python="${RYAML_FUZZ_PYTHON:-$fuzz_root/.venv-fuzz/bin/python}"
fuzz_runtime="$("$fuzz_python" -c 'import atheris; print(atheris.path())')/asan_with_fuzzer.so"

if [[ ! -f "$fuzz_runtime" ]]; then
    echo "Atheris ASan runtime not found: $fuzz_runtime" >&2
    exit 1
fi

cd -- "$fuzz_root"
mkdir -p target/fuzz/artifacts
export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=0:allocator_may_return_null=1}"
# ASan only looks for an unversioned `llvm-symbolizer` on PATH; distro packages
# often install it as `llvm-symbolizer-NN`. Without one, stack traces are raw
# addresses.
if [[ -z "${ASAN_SYMBOLIZER_PATH:-}" ]]; then
    for candidate in llvm-symbolizer $(compgen -c llvm-symbolizer- | sort -t- -k3 -rn); do
        if candidate="$(command -v "$candidate")"; then
            export ASAN_SYMBOLIZER_PATH="$candidate"
            break
        fi
    done
fi
fuzz_preload="$fuzz_runtime${LD_PRELOAD:+:$LD_PRELOAD}"
# libFuzzer's -fork/-merge modes re-run fuzz/run.py via its shebang
# (/usr/bin/env python3), so put the fuzzing interpreter first on PATH.
export PATH="$(dirname -- "$fuzz_python"):$PATH"

if [[ $# -eq 0 ]]; then
    set -- fuzz/corpus fuzz/seed -fork=8 -max_len=2048
fi

# Atheris's bundled ASan runtime predates LLVM's fix for kernels with
# vm.mmap_rnd_bits=32 (e.g. WSL2 6.x); with full ASLR its shadow memory can
# collide with mappings and it loops forever printing DEADLYSIGNAL. Disable
# address randomization for the fuzzer process to avoid that. LD_PRELOAD is
# applied via env *inside* setarch: setarch itself still runs with ASLR, so
# preloading the runtime into it would hit the same crash.
exec setarch "$(uname -m)" -R env LD_PRELOAD="$fuzz_preload" "$fuzz_python" fuzz/run.py -artifact_prefix=target/fuzz/artifacts/ "$@"
