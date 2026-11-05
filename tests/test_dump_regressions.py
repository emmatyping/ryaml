"""Dumper compatibility regressions: ryaml must write PyYAML's exact bytes."""

import collections

import pytest
import yaml

import ryaml


@pytest.fixture
def reference():
    dumper = getattr(yaml, "CSafeDumper", None)
    if dumper is None:
        pytest.skip("PyYAML was built without LibYAML")
    return lambda docs, **options: yaml.dump_all(docs, Dumper=dumper, **options)


def outcome(dump):
    try:
        return dump()
    except Exception as e:  # noqa: BLE001
        return type(e).__name__


def assert_same_dump(reference, docs, **options):
    expected = outcome(lambda: reference(docs, **options))
    actual = outcome(lambda: yaml.dump_all(docs, Dumper=ryaml.RSafeDumper, **options))
    assert actual == expected
    if not options:
        expected = outcome(lambda: reference(docs[:1], sort_keys=False))
        assert outcome(lambda: ryaml.dumps(docs[0])) == expected


class StrSubclass(str):
    pass


class IntSubclass(int):
    pass


class ListSubclass(list):
    pass


class DictSubclass(dict):
    pass


@pytest.mark.parametrize("value", [
    pytest.param(frozenset(), id="frozenset-empty"),
    pytest.param(frozenset({1}), id="frozenset"),
    pytest.param(collections.OrderedDict(a=1), id="OrderedDict"),
    pytest.param(DictSubclass(a=1), id="dict-subclass"),
    pytest.param(ListSubclass([1]), id="list-subclass"),
    pytest.param(StrSubclass("s"), id="str-subclass"),
    pytest.param(IntSubclass(3), id="int-subclass"),
    pytest.param({"k": [1, frozenset()]}, id="nested"),
])
def test_only_exact_types_are_represented(reference, value):
    # SafeRepresenter looks up type(data) itself, so subclasses of the types
    # it represents are refused like any other object.
    assert_same_dump(reference, [value])
