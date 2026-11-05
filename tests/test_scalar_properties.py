"""Property-based checks that ryaml constructs scalars exactly like PyYAML.

Strategies build text from the pieces each PyYAML constructor handles (signs,
underscores, prefixes, sexagesimal parts, exponents, .inf/.nan, timestamp
fields, base64, bool/null words) mixed with noise, so both the valid values
and every error path are compared against PyYAML's CSafeLoader: the result's
type and value, or the exception class.
"""

import base64
import datetime as dt
import math
import os

import pytest
import yaml
from hypothesis import given, settings, strategies as st

import ryaml

CSafeLoader = getattr(yaml, "CSafeLoader", None)
pytestmark = pytest.mark.skipif(
    CSafeLoader is None, reason="PyYAML was built without LibYAML"
)

LOADERS = {
    "loads": ryaml.loads,
    "RSafeLoader": lambda source: yaml.load(source, Loader=ryaml.RSafeLoader),
}


def outcome(load, source):
    try:
        return ("ok", normalize(load(source)))
    except Exception as e:  # noqa: BLE001 - the exception class is compared
        return ("error", type(e))


def normalize(value):
    """Make values comparable: exact types, NaN equal to NaN, tz-aware times."""
    if isinstance(value, float) and math.isnan(value):
        return (float, "nan")
    if isinstance(value, dt.datetime):
        return (dt.datetime, value.replace(tzinfo=None), value.utcoffset())
    if isinstance(value, dict):
        return (dict, [(normalize(k), normalize(v)) for k, v in value.items()])
    if isinstance(value, list):
        return (list, [normalize(v) for v in value])
    return (type(value), value)


def assert_matches_pyyaml(source):
    expected = outcome(lambda s: yaml.load(s, Loader=CSafeLoader), source)
    for name, load in LOADERS.items():
        actual = outcome(load, source)
        if expected[0] == actual[0] == "error":
            # ryaml's exceptions subclass PyYAML's (e.g. ConstructorError).
            assert issubclass(actual[1], expected[1]), (name, source, actual, expected)
        else:
            assert actual == expected, (name, source)


def single_quoted(text):
    return "'" + text.replace("'", "''") + "'"


def mixed_case(word):
    return st.lists(st.booleans(), min_size=len(word), max_size=len(word)).map(
        lambda upper: "".join(c.upper() if u else c for c, u in zip(word, upper))
    )


def joined(*parts):
    return st.tuples(*parts).map("".join)


def sprinkle_underscores(text_strategy):
    """Insert underscores at random positions, as YAML 1.1 numbers allow."""
    return st.tuples(text_strategy, st.lists(st.integers(0, 40), max_size=3)).map(
        lambda t: _insert(t[0], t[1])
    )


def _insert(text, positions):
    for p in sorted(positions, reverse=True):
        text = text[: p % (len(text) + 1)] + "_" + text[p % (len(text) + 1):]
    return text


sign = st.sampled_from(["", "", "-", "+", "--", "+-"])
digits = st.text("0123456789", min_size=1, max_size=12)
noise = st.text(" \t0123456789abcdefxXoObB._:+-eEiInNfFaA", max_size=12)

ints = st.one_of(
    joined(sign, st.sampled_from(["", "0", "0b", "0x", "0o"]), sprinkle_underscores(
        st.text("0123456789abcdefABCDEF", min_size=1, max_size=20))),
    joined(sign, sprinkle_underscores(digits),
           st.lists(joined(st.just(":"), st.text("0123456789", max_size=3)),
                    max_size=4).map("".join)),
    st.integers().map(str),
    noise,
)

mantissa = joined(
    st.text("0123456789", max_size=8), st.sampled_from([".", "", "."]),
    st.text("0123456789", max_size=8),
)
exponent = st.one_of(st.just(""), joined(st.sampled_from(["e", "E"]),
                                         st.sampled_from(["", "+", "-"]), digits))
floats = st.one_of(
    joined(sign, sprinkle_underscores(mantissa), exponent),
    joined(sign, sprinkle_underscores(
        st.sampled_from([".inf", ".nan", "inf", "nan", "infinity"]).flatmap(mixed_case))),
    joined(sign, mantissa,
           st.lists(joined(st.just(":"), mantissa), min_size=1, max_size=5)
           .map("".join)),
    st.floats(allow_nan=True, allow_infinity=True).map(repr),
    noise,
)

bools = st.one_of(
    st.sampled_from(["yes", "no", "true", "false", "on", "off", "y", "n"])
    .flatmap(mixed_case),
    noise,
)

nulls = st.one_of(st.sampled_from(["~", "null", "", "NULL", "Null", "nULL"]), noise)


def two(lo, hi):
    return st.integers(lo, hi).map(lambda n: str(n).zfill(st_width(n)))


def st_width(n):
    return 2 if n < 100 else len(str(n))


timestamps = st.one_of(
    joined(
        st.integers(0, 9999).map(lambda y: str(y).zfill(4)),
        st.just("-"), st.integers(0, 13).map(str), st.just("-"),
        st.integers(0, 32).map(str),
        st.one_of(
            st.just(""),
            joined(
                st.sampled_from(["T", "t", " ", "  ", "\t"]),
                st.integers(0, 25).map(str), st.just(":"), two(0, 61), st.just(":"),
                two(0, 61),
                st.one_of(st.just(""), joined(st.just("."), st.text("0123456789",
                                                                    max_size=9))),
                st.sampled_from(["", "Z", " Z", "+05", "-5", " -05:30", "+1:00",
                                 "+14:60"]),
            ),
        ),
    ),
    noise,
)

base64_text = st.one_of(
    st.binary(max_size=30).map(lambda b: base64.b64encode(b).decode()),
    st.text("ABCDEFabcdef0123456789+/= \n", max_size=30),
)

STRATEGIES = {
    "int": ints,
    "float": floats,
    "bool": bools,
    "null": nulls,
    "timestamp": timestamps,
    "binary": base64_text,
    "str": st.one_of(ints, floats, bools, nulls, timestamps),
}

# RYAML_PROPERTY_EXAMPLES=20000 for a longer search than the default run.
SETTINGS = settings(
    max_examples=int(os.environ.get("RYAML_PROPERTY_EXAMPLES", "300")), deadline=None
)


@pytest.mark.parametrize("kind", sorted(STRATEGIES))
@SETTINGS
@given(data=st.data())
def test_implicit_scalar(kind, data):
    """Plain scalars: implicit tag resolution, then construction."""
    text = data.draw(STRATEGIES[kind].filter(_plain_safe))
    assert_matches_pyyaml(f"k: {text}\n")


@pytest.mark.parametrize("kind", sorted(STRATEGIES))
@SETTINGS
@given(data=st.data())
def test_explicit_tag(kind, data):
    """Explicit `!!tag 'text'`: the constructor alone, on arbitrary text."""
    text = data.draw(STRATEGIES[kind])
    assert_matches_pyyaml(f"k: !!{kind} {single_quoted(text)}\n")


@pytest.mark.parametrize("kind", sorted(STRATEGIES))
@SETTINGS
@given(data=st.data())
def test_explicit_tag_on_value_mapping(kind, data):
    """`!!tag {=: 'text'}` constructs the scalar from the `=` key's value."""
    text = data.draw(STRATEGIES[kind])
    assert_matches_pyyaml(f"k: !!{kind} {{=: {single_quoted(text)}}}\n")


def double_quoted(text):
    return '"' + "".join(
        {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t"}.get(c, c) for c in text
    ) + '"'


@pytest.mark.parametrize("kind", sorted(STRATEGIES))
@SETTINGS
@given(data=st.data())
def test_non_specific_tag(kind, data):
    """`! "text"` resolves a quoted scalar like a plain one. PyYAML's resolver
    regexes end in `$`, which also matches before a final newline."""
    text = data.draw(STRATEGIES[kind]) + data.draw(st.sampled_from(["", "", "\n", "\n\n"]))
    assert_matches_pyyaml(f"k: ! {double_quoted(text)}\n")


def _plain_safe(text):
    """Text that stays a single plain scalar in `k: <text>`."""
    if text != text.strip() or "\n" in text or "#" in text or ": " in text:
        return False
    return not text or (text[0] not in "-?:,[]{}#&*!|>'\"%@`" and not text.endswith(":"))
