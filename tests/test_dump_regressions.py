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


@pytest.mark.parametrize("encoding", [
    "utf-16-le", "utf-16-be", "utf-8", "utf8", "UTF-8", "utf-16", "utf-16le",
    "UTF-16-LE", "latin-1", "",
])
def test_encodings_like_pyyaml(reference, encoding):
    # Only "utf-16-le" and "utf-16-be" select UTF-16 (with a byte order
    # mark); any other encoding gives UTF-8 bytes.
    assert_same_dump(reference, [{"a": "\xe9"}, None, ["x"]], encoding=encoding)


@pytest.mark.parametrize("value", [
    1e16, 1e17, 123456789012345678.0, 1.5e300, 1e-4, 1e-5, 0.0001234, 5e-324,
    1.7976931348623157e308, -0.0, 0.1, 1.0, -2.5e-07, 1e22, 9999999999999998.0,
])
def test_floats_like_python_repr(reference, value):
    # PyYAML writes repr(value).lower(), adding ".0" before a bare exponent.
    assert_same_dump(reference, [value])


@pytest.mark.parametrize("value", [
    b"", b"a", b"\x00" * 57, b"\x00" * 58, bytes(range(256)),
])
@pytest.mark.parametrize("options", [{}, {"default_flow_style": True}, {"canonical": True}],
                         ids=["block", "flow", "canonical"])
def test_binary_like_base64_encodebytes(reference, value, options):
    # base64.encodebytes gives "" for no bytes and ends every line (76
    # characters) with a newline.
    assert_same_dump(reference, [value, {"k": value}], **options)


def _recursive_list():
    items = [1]
    items.append(items)
    return items


def _recursive_dict():
    mapping = {"a": 1}
    mapping["self"] = mapping
    return mapping


def _shared():
    shared = [1, 2]
    return {"a": shared, "b": [shared, {"c": shared}]}


@pytest.mark.parametrize("make", [
    pytest.param(_recursive_list, id="list"),
    pytest.param(_recursive_dict, id="dict"),
    pytest.param(lambda: (_recursive_list(),), id="list-in-tuple"),
    pytest.param(lambda: [_recursive_dict(), _recursive_list()], id="two-cycles"),
    pytest.param(_shared, id="shared"),
])
@pytest.mark.parametrize("options", [{}, {"default_flow_style": True}],
                         ids=["block", "flow"])
def test_recursive_and_shared_data_use_aliases(reference, make, options):
    # PyYAML records a collection's node before representing its items, so
    # data that contains itself is written with an anchor and an alias.
    assert_same_dump(reference, [make()], **options)


@pytest.mark.parametrize("docs", [
    pytest.param(["\ud800"], id="alone"),
    pytest.param([{"\udfff": 1}], id="key"),
    pytest.param([["\ud800", collections.OrderedDict()]], id="before-unrepresentable"),
    pytest.param([("\ud800", frozenset())], id="before-frozenset"),
    pytest.param(["ok", ["\ud800"]], id="second-document"),
])
def test_lone_surrogates_fail_when_written(reference, docs):
    # PyYAML's emitter encodes each str when it writes it, so a representer
    # error anywhere in the document comes first.
    assert_same_dump(reference, docs)


@pytest.mark.parametrize("options", [
    pytest.param({"version": (1, 1)}, id="version-1.1"),
    pytest.param({"version": (1, 2)}, id="version-1.2"),
    pytest.param({"version": (2, 0)}, id="version-2.0"),
    pytest.param({"tags": {"!e!": "tag:example.com,2000:", "!b!": "x:"}}, id="tags"),
    pytest.param({"tags": {"!y!": "tag:yaml.org,2002:"}}, id="tags-shorten"),
    pytest.param({"version": (1, 1), "tags": {"!a!": "p"}, "explicit_end": True},
                 id="both"),
])
def test_version_and_tag_directives(reference, options):
    # PyYAML passes version and tags to every DOCUMENT-START event.
    assert_same_dump(reference, ["a", {"b": b"\x00", "s": {1}}], **options)


@pytest.mark.parametrize("value", [
    "\U0001f600", "a\U00010000b", "\U0010ffff", "�", "", "é",
    {"\U0001f600": "\U0001f389"},
])
@pytest.mark.parametrize("allow_unicode", [True, False])
def test_characters_beyond_the_bmp_are_escaped(reference, value, allow_unicode):
    # libyaml's IS_PRINTABLE only accepts characters of up to three UTF-8
    # bytes, so even with allow_unicode, U+10000 and above are escaped.
    assert_same_dump(reference, [value], allow_unicode=allow_unicode)


@pytest.mark.parametrize("mapping", [
    pytest.param({"b": 1, "a": 2, "c": 3}, id="sortable"),
    pytest.param({("", ""): "", " ": "", "": ""}, id="tuple-and-str"),
    pytest.param({"b": 1, None: 2, "a": 3}, id="none-and-str"),
    pytest.param({2: 1, "a": 2, 1: 3}, id="int-and-str"),
    pytest.param({__import__("datetime").datetime(2001, 1, 1): 1,
                  __import__("datetime").date(2000, 1, 1): 2}, id="datetime-and-date"),
    pytest.param({float("nan"): 1, 1.5: 2, float("nan"): 3, 0.5: 4}, id="nan"),
    pytest.param({True: 1, 0: 2, 2.5: 3}, id="bool-int-float"),
])
@pytest.mark.parametrize("sort_keys", [True, False])
def test_keys_sorted_like_python_sorted(reference, mapping, sort_keys):
    # PyYAML sorts the items with sorted() and keeps their order if that raises
    # TypeError; a partial sort would differ.
    assert_same_dump(reference, [mapping], sort_keys=sort_keys)


@pytest.mark.parametrize("value", [
    pytest.param({3, 1, 2}, id="ints"),
    pytest.param({"s": {"b", "a"}}, id="nested"),
    pytest.param({1, "a"}, id="unsortable"),
    pytest.param(set(), id="empty"),
])
@pytest.mark.parametrize("options", [
    {}, {"sort_keys": False}, {"default_flow_style": True}, {"default_flow_style": None},
], ids=["sorted", "unsorted", "flow", "auto-flow"])
def test_sets_are_represented_as_mappings(reference, value, options):
    # PyYAML's represent_set builds {member: None} and calls represent_mapping,
    # so members are sorted and the flow style is chosen like a mapping's.
    assert_same_dump(reference, [value], **options)


@pytest.mark.parametrize("value", [
    "a\n b", "a\n\n b", "a\n\tb", "a\nb", "a\n\nb", "a\n", "a\n\n", "a b\n  c\nd",
])
def test_folded_scalar_line_breaks(reference, value):
    # libyaml adds a blank line before a break only when the text after the
    # run of breaks is not blank (it looks at the first non-break character).
    assert_same_dump(reference, [value], default_style=">")


def _nested(depth, leaf, kind):
    value = leaf
    for _ in range(depth):
        value = {"k": value} if kind == "dict" else [value]
    return value


def _dump_outcome(dumper, value):
    try:
        yaml.dump_all([value], Dumper=dumper)
        return "ok"
    except Exception as e:  # noqa: BLE001
        return type(e).__name__


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("kind", ["list", "dict"])
@pytest.mark.parametrize("leaf", ["x", [], object()], ids=["scalar", "empty", "unrepresentable"])
def test_deep_nesting_fails_where_pyyaml_does(kind, leaf):
    # PyYAML's representer is recursive Python (three frames per level), so it
    # raises RecursionError past a depth set by the recursion limit and the
    # caller's stack. Both dumpers are called from the same place here.
    outcomes = {
        depth: (_dump_outcome(yaml.CSafeDumper, _nested(depth, leaf, kind)),
                _dump_outcome(ryaml.RSafeDumper, _nested(depth, leaf, kind)))
        for depth in range(250, 340)
    }
    assert {d: r for d, (r, _) in outcomes.items()} == {d: a for d, (_, a) in outcomes.items()}
    assert {r for r, _ in outcomes.values()} >= {"RecursionError"}


def test_very_deep_nesting_raises_instead_of_crashing():
    value = _nested(200_000, "x", "list")
    with pytest.raises(RecursionError):
        ryaml.dumps(value)
    with pytest.raises(RecursionError):
        yaml.dump(value, Dumper=ryaml.RSafeDumper)


def _protocol(dumper, ops):
    import io
    stream = io.StringIO()
    instance = dumper(stream)
    results = []
    for op in ops:
        try:
            if op == "represent":
                instance.represent({"a": [1]})
            elif op == "represent-bad":
                instance.represent(object())
            else:
                getattr(instance, op)()
            results.append("ok")
        except Exception as e:  # noqa: BLE001
            results.append(type(e).__name__)
    return results, stream.getvalue()


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("ops", [
    ("represent", "open", "represent", "close"),
    ("represent-bad", "open", "close"),
    ("open", "open", "close", "close"),
    ("open", "close", "represent", "open", "represent-bad"),
    ("close", "open", "represent", "close"),
    ("dispose", "open", "represent", "dispose", "close"),
    ("open", "dispose", "represent", "close", "dispose", "open"),
], ids="-".join)
def test_dumper_protocol_like_pyyaml(ops):
    # open/represent/close/dispose in any order: PyYAML represents first, then
    # raises SerializerError if not open or already closed, and its dispose()
    # does nothing.
    assert _protocol(ryaml.RSafeDumper, ops) == _protocol(yaml.CSafeDumper, ops)
