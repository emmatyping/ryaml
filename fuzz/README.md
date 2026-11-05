# Fuzzing ryaml

The local fuzz build uses a separate `.venv-fuzz` environment with CPython 3.12,
Atheris 3.1.0, and Rust nightly. Both ryaml and libyaml-safer are instrumented
with AddressSanitizer, coverage counters, PC tables, and comparison tracing.
The ordinary development environment is independent of this installed wheel.

From the repository root, start four fuzzing workers with:

```sh
./fuzz/run.sh
```

The default arguments are `fuzz/corpus fuzz/seed -fork=8 -max_len=2048
-dict=fuzz/yaml.dict -use_value_profile=1`; `fuzz/yaml.dict` lists YAML
indicators, directives, tags and resolver keywords for libFuzzer to splice in.
New corpus entries go into `fuzz/corpus`; crash artifacts go into
`target/fuzz/artifacts`. Stop with Ctrl+C. YAML files are passed directly to the
parser comparison, without Hypothesis decoding their bytes first.

Each input is loaded by PyYAML's CSafeLoader and by ryaml (through
`RSafeLoader` and the native `ryaml.loads_all`) as decoded text, as raw bytes,
and as raw bytes behind a comment that moves libyaml's 16 KB input chunk
boundary into the input. The raw bytes also go through the single-document API
(`yaml.load` with `RSafeLoader`, and `ryaml.loads`). Results must match exactly: types, float bits
(including `-0.0`), datetime UTC offsets, mapping key order, and which
collections are the same object (from anchors, aliases and merge keys). When
both fail, ryaml's exception must be (a subclass of) PyYAML's.

To pass your own libFuzzer arguments, supply the complete argument list:

```sh
./fuzz/run.sh fuzz/corpus fuzz/seed -fork=2 -max_len=4096
```

Replay only the compatibility seeds:

```sh
./fuzz/run.sh -runs=0 fuzz/seed/compat-*.yaml
```

ryaml and libyaml-safer are built with overflow checks and debug assertions
(see `fuzz/sancov-rustc-wrapper.sh`), so integer overflow and failed
`debug_assert!`s panic; the panic reaches Python as pyo3's `PanicException`,
which the harness does not catch, so libFuzzer records a crash.

The launcher finds Atheris's `asan_with_fuzzer.so`, preloads it, and disables leak
detection for the Python interpreter. See the
[Atheris native-extension guide](https://github.com/google/atheris/blob/master/native_extension_fuzzing.md)
for runtime details. Set `RYAML_FUZZ_PYTHON` to use another prepared fuzzing
environment, or `ASAN_OPTIONS` to override sanitizer options.

## Dumping

`fuzz/dump.py` checks that ryaml's dumper writes exactly the same bytes as
PyYAML's CSafeDumper. Hypothesis generates dumper options (flow style, scalar
style, canonical, indent, width, Unicode, line breaks, encodings, document
markers, version and tag directives, key sorting) and one to three documents of
Python values: tricky strings (resolver look-alikes, indicators, line breaks,
BOMs, lone surrogates, long text for folding), ints and floats of every size,
dates and datetimes with arbitrary UTC offsets, bytes, sets, tuples, and lists
and dicts that are shared or recursive (anchors and aliases). Some values are
ones PyYAML's SafeRepresenter refuses (subclasses such as `OrderedDict`,
frozensets). Each is dumped with `yaml.dump_all` through `CSafeDumper` and
`RSafeDumper`, and with `ryaml.dumps` against CSafeDumper with
`sort_keys=False`. Outputs must be identical; when PyYAML raises, ryaml must
raise the same exception class or a subclass of it.

Fuzz it with libFuzzer (its inputs are Hypothesis choice sequences, kept in
`fuzz/dump-corpus`):

```sh
./fuzz/run.sh --dump                                   # fuzz/dump-corpus -fork=8 -max_len=1024
./fuzz/run.sh --dump fuzz/dump-corpus -fork=2 -max_len=4096
```

Or run it as an ordinary Hypothesis test in the development environment, which
shrinks each distinct failure to a minimal example:

```sh
python -m pytest fuzz/dump.py
```

## Coverage

To see which Rust code the corpus reaches, and so where new seeds would help:

```sh
rustup component add llvm-tools-preview   # once
./fuzz/coverage.sh                        # or: ./fuzz/coverage.sh DIR_OR_FILE...
```

This builds a separate `-C instrument-coverage` extension (stable Rust, no
sanitizers) into `target/coverage`, replays the inputs through `fuzz/run.py`'s
comparisons, and writes `target/coverage/summary.txt`, annotated sources in
`target/coverage/html/`, and every never-executed line in
`target/coverage/uncovered.txt`. Seeds named `fuzz/seed/cov-*.yaml` were added
for branches it showed unreached. The emitter, dumper and libyaml-safer's
document API are not exercised by this harness (it only loads).

## Rebuild

With Rust nightly and system Python 3.12 available, run from the repository root:

```sh
uv venv --python /usr/bin/python3.12 .venv-fuzz
uv pip install --python .venv-fuzz/bin/python atheris hypothesis pyyaml maturin

RUSTUP_TOOLCHAIN=nightly \
RUSTC_WRAPPER="$PWD/fuzz/sancov-rustc-wrapper.sh" \
CARGO_TARGET_DIR=target/fuzz \
  .venv-fuzz/bin/maturin build --release --locked --offline \
  --interpreter .venv-fuzz/bin/python --out target/fuzz/wheels

uv pip install --python .venv-fuzz/bin/python --no-deps --force-reinstall \
  target/fuzz/wheels/ryaml-0.6.0-cp312-cp312-manylinux_2_34_x86_64.whl
```

The wheel filename above is the one produced on this Linux x86-64 machine;
select the corresponding filename if building on another platform. Omit
`--offline` on the first build if Cargo dependencies are not cached.

## Verified locally

- The installed extension exports ASan and sanitizer-coverage references.
- Atheris reports 23,310 native coverage counters and matching PC table entries.
- All 19 compatibility seeds replay successfully.
- A 1,000-run smoke test using the full seed directory completed successfully.
