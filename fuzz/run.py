#!/usr/bin/env python3
import sys
import math

import atheris

from hypothesis import given, strategies as st

with atheris.instrument_imports():
    from ryaml.compat import RSafeLoader
    try:
        from yaml import CSafeLoader as SafeLoader
    except ImportError:
        from yaml import SafeLoader

    import yaml

def approx_equal(a, b, rel_tol=1e-9, _seen=None):
    """Recursively compare structures, using math.isclose for floats."""
    if type(a) != type(b):
        return False
    if isinstance(a, float):
        # Handle NaN, inf
        if math.isnan(a) and math.isnan(b):
            return True
        return math.isclose(a, b, rel_tol=rel_tol)
    if isinstance(a, dict):
        if len(a) != len(b):
            return False
        if _seen is None:
            _seen = set()
        pair = (id(a), id(b))
        if pair in _seen:
            return True
        _seen.add(pair)
        # In order: PyYAML preserves mapping key order, so ryaml must too.
        return all(approx_equal(ak, bk, rel_tol, _seen) and approx_equal(av, bv, rel_tol, _seen)
                    for (ak, av), (bk, bv) in zip(a.items(), b.items()))
    if isinstance(a, (set, frozenset)):
        # Set equality checks identity first, and each library has its own NaN
        # object, so compare NaN members by count rather than by value.
        def is_nan(x):
            return isinstance(x, float) and math.isnan(x)
        return (sum(map(is_nan, a)) == sum(map(is_nan, b))
                and {x for x in a if not is_nan(x)} == {x for x in b if not is_nan(x)})
    if isinstance(a, (list, tuple)):
        if len(a) != len(b):
            return False
        if _seen is None:
            _seen = set()
        pair = (id(a), id(b))
        if pair in _seen:
            return True
        _seen.add(pair)
        return all(approx_equal(x, y, rel_tol, _seen) for x, y in zip(a, b))
    return a == b

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
        assert approx_equal(pyyaml_val, ryaml_val), (
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

@atheris.instrument_func
def fuzz_yaml(data):
    # Corpus entries are YAML files, not Hypothesis's encoded choice streams.
    compare(data.decode("utf-8", errors="replace"))
    # Also pass the raw bytes, so encoding detection (UTF-16 BOMs etc.) and
    # invalid UTF-8 are compared too.
    compare(data)


atheris.Setup(sys.argv, fuzz_yaml)
atheris.Fuzz()
