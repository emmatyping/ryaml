import json
from pathlib import Path

import pytest

import ryaml
import yaml

try:
    from yaml import CSafeLoader as SafeLoader
except ImportError:
    from yaml import SafeLoader

from helpers import VALID_YAMLS, INVALID_YAMLS, YamlTestSuite, _is_nan


def test_loads_empty():
    assert ryaml.loads('') is None

def test_loads_key():
    assert ryaml.loads('''
    key:

    ''') == { 'key': None }

def test_loads_key_value():
    assert ryaml.loads('''
    key:
        4

    ''') == { 'key': 4 }

def test_loads_key_sequence():
    assert ryaml.loads('''
    key:
        - 4
        - 5

    ''') == { 'key': [4, 5] }

@pytest.mark.parametrize("ts", VALID_YAMLS, ids=lambda ts: ts.id)
def test_valid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    actual = ryaml.loads_all(
        ts.in_yaml.read_text("utf-8"),
    )
    if isinstance(actual, list) and len(actual) == 1:
        actual = actual[0]


    text = ts.in_json.read_text("utf-8")

    if text == "":  # noqa: PLC1901
        expected = None
    else:
        try:
            expected = json.loads(text)
        except json.JSONDecodeError:
            decoder = json.JSONDecoder()
            expected = []
            pos = 0
            n = len(text)

            while pos < n:
                obj, pos = decoder.raw_decode(text, pos)
                expected.append(obj)
                while pos < n and text[pos] in " \t\r\n":
                    pos += 1

    if isinstance(expected, list) and not isinstance(actual, list):
        actual = [actual]

    # JSON does not have a native "set" type, while Python does.
    # In YAML, the tag `!!set` represents a set, and Python YAML parsers
    # (including ours) map it to a Python `set`.
    # ```python
    # import yaml as py_yaml
    #
    # y = """\
    # --- !!set
    # ? Mark McGwire
    # ? Sammy Sosa
    # ? Ken Griffey
    # """
    # print(py_yaml.safe_load(y))  # {'Mark McGwire', 'Ken Griffey', 'Sammy Sosa'}
    # print(type(py_yaml.safe_load(y)))  # <class 'set'>
    # ```
    if (
            isinstance(actual, set)
            and isinstance(expected, dict)
            and all(v is None for v in expected.values())
    ):
        actual = dict.fromkeys(actual)

    assert (
        f"\nTest case: {ts.id}\n"
        f"\nYAML file: {ts.in_yaml}\n"
        f"\nActual:\n{actual!r}\n"
        f"\nExpected:\n{expected!r}\n"
    )


@pytest.mark.parametrize("ts", INVALID_YAMLS, ids=lambda ts: ts.id)
def test_invalid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    with pytest.raises(ryaml.InvalidYamlError):
        ryaml.loads_all(ts.in_yaml.read_text("utf-8"))
