"""Test cases to prevent regressions in compatibility with pyyaml"""

import math
import textwrap
import ryaml
from ryaml import RSafeLoader
import yaml
from hypothesis import given, settings, HealthCheck
from hypothesis import strategies as st

try:
    from yaml import CSafeLoader as SafeLoader
except ImportError:
    from yaml import SafeLoader


def test_nonspecific_tag():
    assert ryaml.loads("x: ! 4444") == {"x": 4444}


def test_long_int():
    assert ryaml.loads(
        "x: 99999999999999999999999999999999999999999999999999999999"
    ) == {"x": 99999999999999999999999999999999999999999999999999999999}


def test_set_type():
    assert (
        ryaml.loads(
            textwrap.dedent(
                """
                # sets are represented as a
                # mapping where each key is
                # associated with the empty string
                --- !!set
                ? Mark McGwire
                ? Sammy Sosa
                ? Ken Griff
                """
            )
        )
        == set(["Mark McGwire", "Ken Griff", "Sammy Sosa"])
    )

# --- Comparison helpers ---

def approx_equal(a, b, rel_tol=1e-9):
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
        # Compare sorted by repr since keys might be floats
        a_items = sorted(a.items(), key=repr)
        b_items = sorted(b.items(), key=repr)
        return all(approx_equal(ak, bk) and approx_equal(av, bv)
                    for (ak, av), (bk, bv) in zip(a_items, b_items))
    if isinstance(a, (list, tuple)):
        return len(a) == len(b) and all(approx_equal(x, y) for x, y in zip(a, b))
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

    if both_succeeded:
        assert approx_equal(pyyaml_val, ryaml_val), (
            f"Result mismatch:\n"
            f"  pyyaml: {pyyaml_val!r}\n"
            f"  ryaml:  {ryaml_val!r}\n"
            f"  input:  {text!r}"
        )

# --- Tests ---

@given(st.text())
@settings(max_examples=5000, suppress_health_check=[HealthCheck.too_slow])
def test_any_text(text):
    compare(text)


@given(st.binary())
@settings(max_examples=5000)
def test_any_bytes(data):
    text = data.decode("utf-8", errors="replace")
    compare(text)


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
@settings(max_examples=2000)
def test_structured_values(value):
    """Round-trip: serialize with pyyaml, then load with both."""
    try:
        text = yaml.safe_dump(value)
    except Exception:
        return  # pyyaml couldn't serialize it, skip
    compare(text)
