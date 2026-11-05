#!/usr/bin/env python3
"""Fuzz target: ryaml's dumper must produce byte-identical output to PyYAML's.

Hypothesis generates dumper options and one to three documents of Python
values (including shared and recursive containers), which are dumped with
`yaml.dump_all` using PyYAML's CSafeDumper and ryaml's RSafeDumper, and with
`ryaml.dumps` against CSafeDumper's defaults for it. The outputs must be equal
exactly; when PyYAML raises, ryaml must raise the same exception class (or a
subclass of it).

Under libFuzzer (`fuzz/run.sh --dump`), each input is a Hypothesis choice
sequence decoded by `test_dump.hypothesis.fuzz_one_input`. Without Atheris it
is an ordinary Hypothesis test, which shrinks failures to small examples:
`python -m pytest fuzz/dump.py`.
"""
import collections
import contextlib
import datetime
import decimal
import enum
import math
import sys

try:
    import atheris
except ImportError:  # run as a plain Hypothesis test: pytest fuzz/dump.py
    atheris = None
from hypothesis import HealthCheck, given, settings, strategies as st

with atheris.instrument_imports() if atheris else contextlib.nullcontext():
    import ryaml
    from ryaml.compat import RSafeDumper

    import yaml
    try:
        from yaml import CSafeDumper as SafeDumper
    except ImportError:
        from yaml import SafeDumper


# Strings that the resolver, the scanner or the emitter treat specially.
SPECIAL_STRINGS = [
    "", " ", "  ", "a", "a b", " a", "a ", "\t", "a\tb",
    "~", "null", "Null", "NULL", "yes", "No", "on", "OFF", "y", "n", "true", "False",
    "0", "-0", "+1", "012", "0o17", "0x1F", "0b101", "1_000", "1:20", "190:20:30",
    "1.5", "1e3", "1.0e+3", "-.5", ".inf", "-.Inf", "+.INF", ".nan", ".NaN", "1.", "._",
    "2001-12-14", "2001-12-14t21:59:43.10-05:00", "2001-12-14 21:59:43.10 -5",
    "<<", "=", "!", "&", "*", "!!str", "!foo", "&a", "*a", "%TAG", "@", "`",
    "-", "- ", "- a", "--", "---", "--- a", "...", "?", "? a", ":", ": ", "a:", "a: b",
    "a:b", "#", "#a", "a #b", "a# b", "|", ">", "|-", ">+", "'", "''", '"', "a'b", 'a"b',
    ",", "[", "]", "{", "}", "[a]", "{a: b}", "\\", "a\\nb",
    "\n", "\n\n", "a\n", "\na", "a\nb", "a\n\nb", "a \nb", "a\n b", "\r", "a\r\nb",
    "\x85", "\u2028", "\u2029", "\ufeff", "\ufeffa", "\x00", "\x07", "\x1b", "\x7f",
    "\x9f", "\xa0", "\u00e9", "\u4e2d\u6587", "\U0001f600", "\U0010ffff", "\ufffe", "\ud800", "a\udfffb",
    # Around libyaml's printable ranges (it escapes everything past the BMP).
    "\u00bf\u00c0", "\ud7ff", "\ue000", "\uefff\uf000", "\ufffd", "\U00010000",
    "a\U0001f600 b", "\u0085", "\u00ad", "\u2028a", "\u3000",
]
SPECIAL_FLOATS = [
    0.0, -0.0, 1.0, -1.0, 0.5, 0.1, 1e-7, 1e16, 1e17, 1e22, 123456789012345678.0,
    # Around repr's switch to exponent notation.
    9999999999999998.0, 1e15, 1.5e16, 1e-4, 9.9e-5, 0.00012345,
    5e-324, 1.7976931348623157e308, math.inf, -math.inf, math.nan,
]
SPECIAL_INTS = [0, 1, -1, 7, 10, 2**31, 2**63 - 1, 2**63, -2**63 - 1, 10**30]


class StrSubclass(str):
    pass


class IntSubclass(int):
    pass


class ListSubclass(list):
    pass


class DictSubclass(dict):
    pass


class Placeholder:
    """A value made only when the test runs (see `realize`), for objects of
    types PyYAML refuses. Hypothesis reuses generated str and int values as
    choices in later examples, and fails to replay ones of a subclass, so such
    values must not appear in generated data."""

    def __init__(self, label, make):
        self.label, self.make = label, make

    def __repr__(self):
        return self.label


class SetSubclass(set):
    pass


class DatetimeSubclass(datetime.datetime):
    pass


class Color(enum.IntEnum):
    RED = 1


Point = collections.namedtuple("Point", "x y")

# Values whose exact type SafeRepresenter does not register.
UNREPRESENTABLE = [
    Placeholder("StrSubclass('s')", lambda: StrSubclass("s")),
    Placeholder("IntSubclass(3)", lambda: IntSubclass(3)),
    Placeholder("Color.RED", lambda: Color.RED),
    Placeholder("Point(1, 2)", lambda: Point(1, 2)),
    Placeholder("SetSubclass({1})", lambda: SetSubclass({1})),
    Placeholder("DatetimeSubclass(2001, 1, 2)", lambda: DatetimeSubclass(2001, 1, 2)),
    Placeholder("bytearray(b'a')", lambda: bytearray(b"a")),
    Placeholder("1j", lambda: 1j),
    Placeholder("Decimal('1.5')", lambda: decimal.Decimal("1.5")),
    Placeholder("object()", object),
    Placeholder("range(2)", lambda: range(2)),
    Placeholder("frozenset({1})", lambda: frozenset({1})),
]


def realize(value, seen=None):
    """`value` with placeholders replaced, in place where possible so that
    shared and recursive containers stay shared."""
    if seen is None:
        seen = set()
    if isinstance(value, Placeholder):
        return value.make()
    if isinstance(value, tuple):
        return tuple(realize(item, seen) for item in value)
    if id(value) in seen:
        return value
    if isinstance(value, list):
        seen.add(id(value))
        value[:] = [realize(item, seen) for item in value]
    elif isinstance(value, dict):
        seen.add(id(value))
        for key in value:
            value[key] = realize(value[key], seen)
    return value


def pick(items):
    """One of `items`, chosen by index. Hypothesis can record a sampled_from
    choice as the value itself, which breaks replaying (and shrinking) when the
    value is not a builtin scalar, so always choose with an integer."""
    items = list(items)
    return st.integers(0, len(items) - 1).map(items.__getitem__)


def joined(words, separators):
    return st.tuples(st.lists(words, min_size=2, max_size=40), separators).map(
        lambda parts: parts[1].join(parts[0]))


special_strings = pick(SPECIAL_STRINGS)
strings = st.one_of(
    special_strings,
    st.text(max_size=24),
    # Lone surrogates, which neither library can encode.
    st.text(st.characters(categories=["Cs"]), min_size=1, max_size=3),
    # Indicators in the middle of text.
    joined(special_strings, st.just("")).map(lambda s: s[:200]),
    # Short strings dense in blanks, breaks and indicators, which decide
    # scalar styles, folding and quoting.
    st.text(alphabet=" \t\n\r#:-?'\"\\,[]{}a\x85\u2028\xe9", max_size=12),
    # Long enough to be folded or split at the line width.
    joined(special_strings, pick([" ", "  ", "\n", " \n", "\t"])),
)
integers = st.one_of(pick(SPECIAL_INTS), st.integers())
floats = st.one_of(pick(SPECIAL_FLOATS), st.floats())
# Any offset datetime allows (strictly within a day), including seconds and
# microseconds, which YAML timestamps cannot express.
timezones = st.one_of(
    st.none(),
    st.just(datetime.timezone.utc),
    st.timedeltas(min_value=datetime.timedelta(hours=-23, minutes=-59),
                  max_value=datetime.timedelta(hours=23, minutes=59)).map(datetime.timezone),
)
dates = st.one_of(st.dates(), st.datetimes(timezones=timezones))
scalars = st.one_of(
    strings, integers, floats, st.booleans(), st.none(), st.binary(max_size=200), dates,
    # Values PyYAML's SafeRepresenter does not represent.
    pick(UNREPRESENTABLE),
)
keys = st.recursive(
    st.one_of(strings, integers, floats, st.booleans(), st.none(), dates),
    lambda children: st.tuples(children, children),
    max_leaves=4,
)


@st.composite
def documents(draw):
    """One to three documents. Containers are remembered as they are made, so
    later values can be the same object again (an alias), including inside
    the container itself (a cycle)."""
    shared = []
    budget = [64]  # values per example, so examples stay fast

    def value(depth):
        budget[0] -= 1
        if budget[0] <= 0 or depth >= 6:
            return draw(scalars)
        kind = draw(st.integers(0, 12))
        if kind <= 4:
            return draw(scalars)
        if kind == 5 and shared:
            return draw(pick(shared))
        if kind <= 7:
            items = draw(pick([list, list, list, ListSubclass]))()
            shared.append(items)
            items.extend(value(depth + 1) for _ in range(draw(st.integers(0, 5))))
            if draw(st.integers(0, 7)) == 0:
                # Non-empty tuples are anchored when shared too.
                items = tuple(items)
                shared.append(items)
            return items
        if kind <= 10:
            mapping = draw(pick(
                [dict, dict, dict, dict, collections.OrderedDict, DictSubclass]))()
            shared.append(mapping)
            for _ in range(draw(st.integers(0, 5))):
                mapping[draw(keys)] = value(depth + 1)
            return mapping
        if kind == 11:
            members = draw(st.sets(keys, max_size=4))
            if draw(st.integers(0, 5)) == 0:
                return frozenset(members)
            shared.append(members)
            return members
        date = draw(dates)
        shared.append(date)  # dates are anchored when shared, unlike str
        return date

    return [value(0) for _ in range(draw(st.integers(1, 3)))]


# Keyword arguments for yaml.dump_all, each sometimes left out.
options = st.fixed_dictionaries({}, optional={
    "default_flow_style": pick([False, True, None]),
    "default_style": pick([None, "'", '"', "|", ">", ""]),
    "canonical": pick([None, True, False]),
    "indent": pick([None, 0, 1, 2, 3, 4, 9, 10]),
    "width": pick([None, -1, 0, 1, 2, 10, 20, 40, 80, 1000]),
    "allow_unicode": pick([None, True, False]),
    "line_break": pick([None, "\n", "\r", "\r\n"]),
    # CEmitter: exactly "utf-16-le"/"utf-16-be" select UTF-16, others UTF-8.
    "encoding": pick([None, "utf-8", "utf-16-le", "utf-16-be", "UTF-16-LE", "utf-16",
                      "latin-1", ""]),
    "explicit_start": pick([None, True, False]),
    "explicit_end": pick([None, True, False]),
    "version": pick([None, (1, 1), (1, 2), (1, 0), (2, 0)]),
    # Prefixes of the standard tags make the emitter shorten !!binary etc.
    "tags": pick([None, {"!e!": "tag:example.com,2000:"},
                  {"!y!": "tag:yaml.org,2002:", "!!": "tag:example.com,2000:"},
                  {"!": "tag:yaml.org,2002:"}, {"!e!": "tag:yaml.org,2002:b"}]),
    "sort_keys": st.booleans(),
})


def outcome(dump):
    """(output, None) or (None, exception class)."""
    try:
        return dump(), None
    except RecursionError:
        raise
    except Exception as e:
        return None, type(e)
    except BaseException as e:
        # A Rust panic reaches Python as pyo3's PanicException, a
        # BaseException. Report it as a failure Hypothesis can shrink (it
        # re-raises other BaseExceptions at once).
        if type(e).__name__ == "PanicException":
            raise AssertionError(f"ryaml panicked: {e}") from e
        raise


def check(name, expected, actual, docs, kwargs):
    pyyaml_out, pyyaml_exc = expected
    ryaml_out, ryaml_exc = actual
    context = f"  documents: {docs!r}\n  options:   {kwargs!r}"
    assert (pyyaml_exc is None) == (ryaml_exc is None), (
        f"One raised, one didn't ({name}):\n"
        f"  pyyaml: {pyyaml_exc or pyyaml_out!r}\n"
        f"  ryaml:  {ryaml_exc or ryaml_out!r}\n{context}"
    )
    if pyyaml_exc is not None:
        assert issubclass(ryaml_exc, pyyaml_exc), (
            f"Different exceptions ({name}):\n"
            f"  pyyaml: {pyyaml_exc}\n  ryaml:  {ryaml_exc}\n{context}"
        )
    else:
        assert type(pyyaml_out) is type(ryaml_out) and pyyaml_out == ryaml_out, (
            f"Different output ({name}):\n"
            f"  pyyaml: {pyyaml_out!r}\n  ryaml:  {ryaml_out!r}\n{context}"
        )


def compare(docs, kwargs):
    expected = outcome(lambda: yaml.dump_all(docs, Dumper=SafeDumper, **kwargs))
    actual = outcome(lambda: yaml.dump_all(docs, Dumper=RSafeDumper, **kwargs))
    check("RSafeDumper", expected, actual, docs, kwargs)
    # ryaml.dumps: one document, block style, keys in insertion order.
    expected = outcome(lambda: yaml.dump(docs[0], Dumper=SafeDumper, sort_keys=False))
    actual = outcome(lambda: ryaml.dumps(docs[0]))
    check("ryaml.dumps", expected, actual, docs[:1], {"sort_keys": False})


@settings(database=None, deadline=None, max_examples=500,
          suppress_health_check=list(HealthCheck))
@given(documents(), options)
def test_dump(docs, kwargs):
    compare(realize(docs), kwargs)


if __name__ == "__main__":
    sys.setrecursionlimit(10000)
    atheris.Setup(sys.argv, atheris.instrument_func(test_dump.hypothesis.fuzz_one_input))
    atheris.Fuzz()
