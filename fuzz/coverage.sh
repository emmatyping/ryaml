#!/usr/bin/env bash
# Rust source coverage of ryaml and libyaml-safer over the fuzz corpus.
#
# Builds the extension with `-C instrument-coverage` (stable Rust, no
# sanitizers) into target/coverage, installs it into its own venv with PyYAML,
# replays every input through fuzz/run.py's fuzz_yaml (all three comparisons),
# and writes:
#   target/coverage/summary.txt  per-file region/function/line coverage
#   target/coverage/html/        annotated sources (open index.html)
#   target/coverage/uncovered.txt  source lines never executed, per file
#
# Usage: fuzz/coverage.sh [INPUT_DIR_OR_FILE...]   (default: fuzz/corpus fuzz/seed)
# Needs: rustup component add llvm-tools-preview
set -euo pipefail

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd -- "$root"
out=target/coverage
tools="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
if [[ ! -x "$tools/llvm-cov" ]]; then
    echo "llvm-cov not found; run: rustup component add llvm-tools-preview" >&2
    exit 1
fi
python="${RYAML_COVERAGE_PYTHON:-python3}"
venv="$out/venv"
[[ $# -eq 0 ]] && set -- fuzz/corpus fuzz/seed

if [[ ! -x "$venv/bin/python" ]]; then
    uv venv -q --python "$python" "$venv"
    uv pip install -q --python "$venv/bin/python" pyyaml hypothesis maturin
fi

# Instrument everything (dependencies are filtered out of the report). Build
# scripts are instrumented too and write profiles when they run, so keep those
# out of the working directory.
LLVM_PROFILE_FILE="$root/$out/build-profraw/%p-%m.profraw" \
RUSTFLAGS="-C instrument-coverage" CARGO_TARGET_DIR="$out/build" \
    "$venv/bin/maturin" build --release --locked --interpreter "$venv/bin/python" \
    --out "$out/wheels"
wheel="$(ls -t "$out"/wheels/ryaml-*.whl | head -1)"
uv pip install -q --python "$venv/bin/python" --no-deps --force-reinstall "$wheel"
so="$("$venv/bin/python" -c 'import ryaml._ryaml as m; print(m.__file__)')"

rm -rf "$out/profraw"
mkdir -p "$out/profraw"
LLVM_PROFILE_FILE="$root/$out/profraw/%p-%m.profraw" \
    "$venv/bin/python" - "$@" <<'EOF'
"""Replay inputs through fuzz/run.py's fuzz_yaml without Atheris."""
import contextlib, importlib.util, pathlib, sys, types

atheris = types.ModuleType("atheris")
atheris.instrument_imports = contextlib.nullcontext
atheris.instrument_func = lambda f: f
sys.modules["atheris"] = atheris
spec = importlib.util.spec_from_file_location("fuzz_run", "fuzz/run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)

files = failures = 0
for arg in sys.argv[1:]:
    path = pathlib.Path(arg)
    for f in sorted(path.rglob("*") if path.is_dir() else [path]):
        if not f.is_file():
            continue
        files += 1
        try:
            run.fuzz_yaml(f.read_bytes())
        except (AssertionError, RecursionError) as e:
            failures += 1
            print(f"mismatch: {f}: {str(e).splitlines()[0] if str(e) else type(e).__name__}")
print(f"replayed {files} inputs, {failures} mismatches")
EOF

"$tools/llvm-profdata" merge -sparse "$out"/profraw/*.profraw -o "$out/coverage.profdata"
ignore='(/\.cargo/|/rustc/|/\.rustup/|/target/)'
common=(-instr-profile="$out/coverage.profdata" -object "$so" -ignore-filename-regex="$ignore")
"$tools/llvm-cov" report "${common[@]}" > "$out/summary.txt"
rm -rf "$out/html"
"$tools/llvm-cov" show "${common[@]}" -format=html -output-dir="$out/html" \
    -show-line-counts-or-regions -Xdemangler=rustfilt 2>/dev/null \
    || "$tools/llvm-cov" show "${common[@]}" -format=html -output-dir="$out/html" \
        -show-line-counts-or-regions
# Lines with a zero execution count, grouped by file.
"$tools/llvm-cov" show "${common[@]}" -format=text -show-line-counts \
    | awk '
        /^\/.*\.rs:$/ { file = $0; next }
        match($0, /^ *[0-9]+\| *0\|/) { print file " " $0 }
      ' > "$out/uncovered.txt"
cat "$out/summary.txt"
echo
echo "HTML: $out/html/index.html; never-executed lines: $out/uncovered.txt"
