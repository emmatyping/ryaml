#!/usr/bin/env python3
import sys
import math
import codecs
import datetime

import atheris

from hypothesis import given, strategies as st

with atheris.instrument_imports():
    from ryaml.compat import RSafeLoader
    try:
        from yaml import CSafeLoader as SafeLoader
    except ImportError:
        from yaml import SafeLoader

    import yaml

def same_float(a, b):
    """Exactly the same float: NaN matches NaN, and 0.0 does not match -0.0."""
    if math.isnan(a) or math.isnan(b):
        return math.isnan(a) and math.isnan(b)
    return a.hex() == b.hex()


def same_datetime(a, b):
    """Same wall-clock time and the same UTC offset; datetime's own == compares
    instants, so 10:00+01:00 would equal 09:00Z."""
    return (a.replace(tzinfo=None) == b.replace(tzinfo=None)
            and a.utcoffset() == b.utcoffset()
            and type(a.tzinfo) is type(b.tzinfo))


def same_value(a, b, _pairs=None):
    """Recursively compare loaded values: exact types and values, mapping key
    order, and object sharing. Anchors and aliases (and merge keys) make PyYAML
    reuse one collection object, so the collections must pair up one to one:
    a collection seen again on one side must pair with the same collection
    seen again on the other."""
    if _pairs is None:
        _pairs = ({}, {})
    if type(a) != type(b):
        return False
    if isinstance(a, float):
        return same_float(a, b)
    if isinstance(a, datetime.datetime):
        return same_datetime(a, b)
    if isinstance(a, tuple):
        # !!pairs entries: fresh (key, value) tuples, so not tracked for
        # sharing, but their values can lead back into a cycle.
        return len(a) == len(b) and all(
            same_value(x, y, _pairs) for x, y in zip(a, b))
    if not isinstance(a, (dict, list, set)):
        return a == b
    a_to_b, b_to_a = _pairs
    if id(a) in a_to_b or id(b) in b_to_a:
        return a_to_b.get(id(a)) == id(b) and b_to_a.get(id(b)) == id(a)
    a_to_b[id(a)] = id(b)
    b_to_a[id(b)] = id(a)
    if len(a) != len(b):
        return False
    if isinstance(a, dict):
        # In order: PyYAML preserves mapping key order, so ryaml must too.
        return all(same_value(ak, bk, _pairs) and same_value(av, bv, _pairs)
                   for (ak, av), (bk, bv) in zip(a.items(), b.items()))
    if isinstance(a, set):
        # Members are hashable scalars. Set equality checks identity first, and
        # each library has its own NaN object, so match NaN members by count.
        def key(x):
            if isinstance(x, float):
                return ("float", "nan" if math.isnan(x) else x.hex())
            if isinstance(x, datetime.datetime):
                return ("datetime", x.replace(tzinfo=None), x.utcoffset())
            return (type(x), x)
        return sorted(map(repr, map(key, a))) == sorted(map(repr, map(key, b)))
    return all(same_value(x, y, _pairs) for x, y in zip(a, b))

def compare(text):
    pyyaml_val = pyyaml_exc = None
    ryaml_val = ryaml_exc = None

    try:
        pyyaml_val = list(yaml.load_all(text, Loader=SafeLoader))
    except Exception as e:
        pyyaml_exc = type(e)

    try:
        ryaml_val = list(yaml.load_all(text, Loader=RSafeLoader))
    except Exception as e:
        ryaml_exc = type(e)

    both_succeeded = (pyyaml_exc is None) and (ryaml_exc is None)
    both_failed    = (pyyaml_exc is not None) and (ryaml_exc is not None)

    assert both_succeeded or both_failed, (
        f"One raised, one didn't:\n"
        f"  pyyaml: {pyyaml_exc or pyyaml_val!r}\n"
        f"  ryaml:  {ryaml_exc  or ryaml_val!r}\n"
        f"  input:  {text!r}"
    )

    if both_failed:
        # ryaml's exceptions subclass PyYAML's (e.g. ScannerError), so callers
        # catching PyYAML's class still catch ryaml's.
        assert issubclass(ryaml_exc, pyyaml_exc), (
            f"Different exceptions:\n"
            f"  pyyaml: {pyyaml_exc}\n"
            f"  ryaml:  {ryaml_exc}\n"
            f"  input:  {text!r}"
        )

    if both_succeeded:
        assert same_value(pyyaml_val, ryaml_val), (
            f"Result mismatch:\n"
            f"  pyyaml: {pyyaml_val!r}\n"
            f"  ryaml:  {ryaml_val!r}\n"
            f"  input:  {text!r}"
        )

# Build structured-but-varied YAML strings from fragments
yaml_scalars = st.one_of(
    st.integers(),
    st.floats(allow_nan=True, allow_infinity=True),
    st.text(max_size=20),
    st.none(),
    st.booleans(),
    st.just("~"),
    st.just("null"),
    st.just(".inf"),
    st.just("-.inf"),
    st.just(".nan"),
)

@given(st.recursive(
    yaml_scalars,
    lambda children: st.one_of(
        st.lists(children, max_size=5),
        st.dictionaries(st.text(max_size=10), children, max_size=5),
    ),
    max_leaves=20,
))
def test_structured_values(value):
    """Round-trip: serialize with pyyaml, then load with both."""
    try:
        text = yaml.safe_dump(value)
    except Exception:
        return  # pyyaml couldn't serialize it, skip
    compare(text)

@given(st.binary())
def test_any_bytes(data):
    text = data.decode("utf-8", errors="replace")
    compare(text)

# libyaml decodes its input in 16 KB raw chunks (INPUT_RAW_BUFFER_SIZE), and
# ryaml mimics when it reports decoding errors in each chunk. Inputs are far
# shorter than that, so also run each one behind a comment that pushes a chunk
# boundary into it.
RAW_CHUNK = 16384
BOMS = [
    (codecs.BOM_UTF8, "utf-8"),
    (codecs.BOM_UTF16_LE, "utf-16-le"),
    (codecs.BOM_UTF16_BE, "utf-16-be"),
]


def pad_to_chunk_boundary(data):
    """`data` with a comment inserted at the start (after any BOM) so that the
    first byte of `data` past the BOM lands `k` bytes before a chunk boundary,
    with `k` taken from the last byte so most mutations keep it stable."""
    bom, encoding = b"", "utf-8"
    for candidate, candidate_encoding in BOMS:
        if data.startswith(candidate):
            bom, encoding = candidate, candidate_encoding
            break
    body = data[len(bom):]
    k = data[-1] % (len(body) + 1) if body else 0
    width = len("#".encode(encoding))
    pad_len = RAW_CHUNK - len(bom) - k
    # Round down to whole characters; "#" and "\n" are one unit each.
    units = max(pad_len // width, 2)
    padding = ("#" + "x" * (units - 2) + "\n").encode(encoding)
    return bom + padding + body


@atheris.instrument_func
def fuzz_yaml(data):
    # Corpus entries are YAML files, not Hypothesis's encoded choice streams.
    compare(data.decode("utf-8", errors="replace"))
    # Also pass the raw bytes, so encoding detection (UTF-16 BOMs etc.) and
    # invalid UTF-8 are compared too.
    compare(data)
    compare(pad_to_chunk_boundary(data))


if __name__ == "__main__":
    atheris.Setup(sys.argv, fuzz_yaml)
    atheris.Fuzz()
