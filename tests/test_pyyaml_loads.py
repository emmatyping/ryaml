import ryaml

import pytest

try:
    from yaml import CSafeLoader as SafeLoader, CSafeDumper as SafeDumper
except ImportError:
    from yaml import SafeLoader, SafeDumper

import yaml


from ryaml.compat import RSafeLoader

from helpers import VALID_YAMLS, INVALID_YAMLS, YamlTestSuite, _is_nan

def test_loads_empty():
    assert yaml.load('', Loader=RSafeLoader) is None

def test_loads_key():
    assert yaml.load('''
    key:

    ''', Loader=RSafeLoader) == { 'key': None }

def test_loads_key_value():
    assert yaml.load('''
    key:
        4

    ''', Loader=RSafeLoader) == { 'key': 4 }

def test_loads_key_sequence():
    assert yaml.load('''
    key:
        - 4
        - 5

    ''', Loader=RSafeLoader) == { 'key': [4, 5] }


@pytest.mark.parametrize("ts", VALID_YAMLS, ids=lambda ts: ts.id)
def test_valid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    source = ts.in_yaml.read_text("utf-8")
    actual = list(yaml.load_all(source, Loader=RSafeLoader))
    # JSON fixtures cannot express YAML 1.1 dates, binary data, sets or tuples.
    expected = list(yaml.load_all(source, Loader=SafeLoader))
    assert _is_nan(actual) == _is_nan(expected), (
        f"YAML test suite case {ts.id}: {actual!r} != {expected!r}"
    )

@pytest.mark.parametrize("ts", INVALID_YAMLS, ids=lambda ts: ts.id)
def test_invalid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    with pytest.raises(ryaml.InvalidYamlError):
        ryaml.loads_all(ts.in_yaml.read_text("utf-8"))
