"""PyYAML compatibility regressions, including the original 15 examples."""

import codecs
import datetime as dt
import io

import pytest
import yaml

import ryaml


@pytest.fixture
def reference():
    loader = getattr(yaml, "CSafeLoader", None)
    if loader is None:
        pytest.skip("PyYAML was built without LibYAML")
    return lambda source: yaml.load(source, Loader=loader)


@pytest.fixture(params=["loads", "RSafeLoader", "loads_all"])
def load(request):
    if request.param == "loads":
        return ryaml.loads
    if request.param == "loads_all":
        return lambda source: ryaml.loads_all(source)[0]
    return lambda source: yaml.load(source, Loader=ryaml.RSafeLoader)


def assert_same(actual, expected):
    """Check types and mapping order as well as values (acyclic examples)."""
    assert type(actual) is type(expected)
    if isinstance(expected, dict):
        assert list(actual) == list(expected)
        for (ak, av), (ek, ev) in zip(actual.items(), expected.items()):
            assert_same(ak, ek)
            assert_same(av, ev)
    elif isinstance(expected, (list, tuple)):
        assert len(actual) == len(expected)
        for a, e in zip(actual, expected):
            assert_same(a, e)
    else:
        assert actual == expected


@pytest.mark.parametrize(("source", "expected"), [
    pytest.param("2026-11-04", dt.date(2026, 11, 4), id="date"),
    pytest.param("2026-11-04T12:34:56Z",
                 dt.datetime(2026, 11, 4, 12, 34, 56, tzinfo=dt.timezone.utc),
                 id="datetime"),
    pytest.param("!!binary SGVsbG8=", b"Hello", id="binary"),
    pytest.param("!!omap [{a: 1}, {b: 2}]", [("a", 1), ("b", 2)], id="omap"),
    pytest.param("!!pairs [{a: 1}, {a: 2}]", [("a", 1), ("a", 2)], id="pairs"),
    pytest.param("!!bool TrUe", True, id="explicit-mixed-case-bool"),
    pytest.param("<<: {x: 1}\n<<: {x: 2}\n", {"x": 2}, id="repeated-merges"),
    pytest.param("<<: {a: 1, b: 2}\nc: 3\n", {"a": 1, "b": 2, "c": 3},
                 id="merge-key-order"),
    pytest.param("!!set {<<: {a: null}}", {"a"}, id="set-merge"),
    pytest.param("!!set {=: null}", {"="}, id="set-value-key"),
])
def test_original_examples(load, reference, source, expected):
    assert_same(reference(source), expected)
    assert_same(load(source), expected)


@pytest.mark.parametrize(("source", "error"), [
    pytest.param("2026-99-99", ValueError, id="invalid-date"),
    pytest.param("!!binary a", yaml.constructor.ConstructorError, id="invalid-base64"),
    pytest.param("!!omap [1]", yaml.constructor.ConstructorError, id="invalid-omap"),
    pytest.param("!!pairs [{a: 1, b: 2}]", yaml.constructor.ConstructorError,
                 id="invalid-pairs"),
    ("!!binary café", yaml.constructor.ConstructorError),
    ("!!binary a==", yaml.constructor.ConstructorError),
    ("!!omap [{}]", yaml.constructor.ConstructorError),
    ("!!omap [{a: 1, a: 2}]", yaml.constructor.ConstructorError),
    ("!!pairs [a]", yaml.constructor.ConstructorError),
    ("entry: &entry {a: 1, a: 2}\npairs: !!pairs [*entry]",
     yaml.constructor.ConstructorError),
    ("!!timestamp nonsense", AttributeError),
    ("2026-11-04T25:00:00Z", ValueError),
    ("!!bool perhaps", KeyError),
    ("<<: [1]", yaml.constructor.ConstructorError),
])
def test_constructor_validation(load, reference, source, error):
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


def test_recursive_merge(load, reference):
    source = "a: &a {b: {<<: *a}, c: 1}"
    for result in (reference(source), load(source)):
        assert list(result["a"]["b"]) == ["b", "c"]
        assert result["a"]["b"]["c"] == 1
        assert result["a"]["b"]["b"] is result["a"]["b"]


@pytest.mark.parametrize("source", [
    "!!timestamp 2026-1-4",
    "2026-1-4 1:02:03.123456789 -5:30",
    "2026-11-04t01:02:03.1+01",
    "2026-11-04 01:02:03",
    "2026-11-04T01:02:03.123456789Z",
    "2026-11-04T01:02:03.1Z",
    "2026-11-04T01:02:03.",
    "2026-11-4  01:02:03Z",
    "2026-11-04T01:02:03.Z",
    "2026-11-04T01:02:03.+01:30",
    '!!timestamp "2026-11-04\\t1:02:03.000001\\tZ"',
    "2026-11-04T01:02:03-00:00",
    "2026-11-04T01:02:03+01:99",
    '!!timestamp "2026-11-04\\n"',
    "TrUe",  # The mixed-case change applies only with an explicit tag.
    "!!bool oFf",
    "!!bool YeS",
    "!!binary |\n  SGVs\n  bG8=\n",
    "!!binary 'SGVs!!bG8='",  # PyYAML's decoder tolerates non-base64 chars.
    "!!binary ''",
    "!!binary Zh==",  # Non-zero padding bits require the permissive decoder.
    "!!binary Zg====",
    "!!omap [{[a, b]: 1}]",  # Pair keys do not have to be hashable.
    "!!pairs [!custom {a: 1}]",  # The wrapper's tag is not constructed.
    "!!omap [&entry {a: 1}, *entry]",
    "entry: &entry {a: 1}\npairs: !!pairs [*entry]",
    "<<: [{x: 1, a: 1}, {x: 2, b: 2}]\nx: 3",
    "<<: [{x: 1}, {x: 2}]\n<<: {x: 3}",
    "<<: {x: 1}\n<<: [{x: 2}, {x: 3}]",
    "base: &base {<<: {x: 1}, x: 2}\ncopy: {<<: *base}",
    "!!set {<<: [{a: 1}, {b: 2}]}",
    "<<: !!set {a: null}",
    "set: &set !!set {a: null}\ncopy: {<<: *set}",
    "<<: []\na: 1",
    "&a {<<: *a, x: 1}",
    '"<<": value\n"=": value',
])
def test_related_forms(load, reference, source):
    assert_same(load(source), reference(source))


def test_merge_alias_identity(load, reference):
    source = "base: &base {<<: {x: 1}}\nalias: *base\ncopy: {<<: *base}"
    actual = load(source)
    assert_same(actual, reference(source))
    assert actual["base"] is actual["alias"]
    assert actual["copy"] is not actual["base"]


def test_merge_state_is_document_local():
    source = "---\n&base {<<: {x: 1}}\n---\n&base {<<: {x: 2}}\n"
    expected = [{"x": 1}, {"x": 2}]
    assert ryaml.loads_all(source) == expected
    assert list(yaml.load_all(source, Loader=ryaml.RSafeLoader)) == expected


def test_recursive_pair_wrapper(load, reference):
    source = "!!pairs [&wrapper {self: *wrapper}]"
    for result in (reference(source), load(source)):
        assert type(result[0]) is tuple
        key, mapping = result[0]
        assert key == "self"
        assert mapping["self"] is mapping


@pytest.mark.parametrize("key", [
    pytest.param("é" * 1000, id="two-byte"),
    pytest.param("�" * 1000, id="three-byte"),
    pytest.param("🙂" * 1000, id="four-byte"),
])
def test_simple_key_limit_counts_characters(load, reference, key):
    # LibYAML's 1024 limit on implicit keys is in characters, not UTF-8 bytes.
    source = f"{key}: 1"
    assert_same(load(source), reference(source))
    source = f"{key * 2}: 1"
    with pytest.raises(yaml.YAMLError):
        reference(source)
    with pytest.raises(yaml.YAMLError):
        load(source)


def test_nan_is_shared(load, reference):
    # PyYAML returns one NaN object, so duplicate `.nan` keys collapse.
    source = "[.nan, .NaN, .NAN, !!float .nan]"
    for result in (reference(source), load(source)):
        assert all(x is result[0] for x in result)
    source = ".nan: 1\n.NaN: 2\n"
    for result in (reference(source), load(source)):
        assert len(result) == 1
        assert list(result.values()) == [2]


@pytest.mark.parametrize("source", [
    pytest.param("&x <<: {a: 1}\nb: &x 2\n", id="merge-then-value"),
    pytest.param("a: &x 1\n&x <<: {b: 2}\n", id="value-then-merge"),
    pytest.param("&x <<: {a: 1}\n&x <<: {b: 2}\n", id="merge-then-merge"),
])
def test_anchored_merge_key_duplicate(load, reference, source):
    with pytest.raises(yaml.composer.ComposerError):
        reference(source)
    with pytest.raises(yaml.composer.ComposerError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("&m <<: {a: 1}\nb: *m\n", id="mapping-value"),
    pytest.param("- {&m <<: {a: 1}}\n- *m\n", id="sequence-item"),
])
def test_alias_to_merge_key_as_value(load, reference, source):
    # The aliased node is tagged !!merge, which has no constructor.
    with pytest.raises(yaml.constructor.ConstructorError):
        reference(source)
    with pytest.raises(yaml.constructor.ConstructorError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("&m <<: {a: 1}\n*m : {b: 2}\n", id="same-mapping"),
    pytest.param("- {&m <<: {a: 1}}\n- {*m : {c: 3}}\n", id="other-mapping"),
])
def test_alias_to_merge_key_as_key(load, reference, source):
    assert_same(load(source), reference(source))


@pytest.mark.parametrize("value", [
    "0 :6", "00:01", "01:30", "_0_8", "09",
])
def test_explicit_int_leading_zero_is_octal(load, reference, value):
    # PyYAML checks for a leading 0 (octal) before ':' (sexagesimal).
    source = f"!!int '{value}'"
    with pytest.raises(ValueError):
        reference(source)
    with pytest.raises(ValueError):
        load(source)


@pytest.mark.parametrize("value", [
    "--0", "+ 1", "- 1", "-0", "0_x1F", "0_b101", "_0_7", "-_5", "1: 5", "1:-5",
    "1_000:30", "-12345678901234567890123",
])
def test_explicit_int_matches_pyyaml(load, reference, value):
    # Underscores are removed first, one sign is stripped, and the rest is
    # passed to int(), which also accepts whitespace and a second sign.
    source = f"!!int '{value}'"
    assert_same(load(source), reference(source))


@pytest.mark.parametrize("source", [
    pytest.param("<<: [{x: 2}, !o {x: 3}]\n", id="unknown-tag-in-sequence"),
    pytest.param("<<: !o {x: 1}\n", id="unknown-tag-mapping"),
    pytest.param("<<: !o [{x: 1}]\n", id="unknown-tag-sequence"),
    pytest.param("<<: !!str {x: 1}\n", id="str-tag-mapping"),
    pytest.param("<<: !!omap [{x: 1}]\n", id="omap"),
    pytest.param("<<: !!pairs [{x: 1}]\n", id="pairs"),
    pytest.param("<<: !o {<<: {x: 1}}\n", id="nested-merge"),
    pytest.param("<<: &a !o {x: 1}\nb: {<<: *a}\n", id="alias-merged-again"),
    pytest.param("<<: &o !!omap [{x: 1}, {y: 2}]\nb: *o\n", id="omap-alias-value"),
    pytest.param("<<: &o !!omap [{x: 1, y: 2}]\nb: {<<: *o}\n",
                 id="omap-alias-merged-again"),
])
def test_merge_source_tag_is_ignored(load, reference, source):
    # PyYAML merges by node kind and never constructs merge sources by tag.
    assert_same(load(source), reference(source))


@pytest.mark.parametrize("source", [
    pytest.param("<<: [!o {x: !o 1}]\n", id="tagged-value-inside"),
    pytest.param("<<: &a !o {x: 1}\ny: *a\n", id="alias-as-value"),
    pytest.param("<<: [&a !o {x: 1}]\nb: *a\n", id="item-alias-as-value"),
    pytest.param("<<: &o !!omap [{x: 1, y: 2}]\nb: *o\n", id="invalid-omap-alias"),
    pytest.param("<<: [!o 1]\n", id="tagged-scalar-item"),
])
def test_merge_source_tag_still_constructed(load, reference, source):
    # Values inside merge sources, and aliases used outside a merge, are
    # constructed by tag as usual.
    with pytest.raises(yaml.constructor.ConstructorError):
        reference(source)
    with pytest.raises(yaml.constructor.ConstructorError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("!!str\n=:", id="fuzz-block"),
    pytest.param("!!str {=: hi}", id="str"),
    pytest.param("!!int {=: 5}", id="int"),
    pytest.param("!!float {=: 1.5}", id="float"),
    pytest.param("!!bool {=: yes}", id="bool"),
    pytest.param("!!null {=: 7}", id="null"),
    pytest.param("!!binary {=: aGk=}", id="binary"),
    pytest.param("!!str {a: !o 1, =: b, =: c}", id="first-value-key-other-entries-skipped"),
    pytest.param("!!str {=: {=: deep}}", id="nested"),
    pytest.param("!!str {!!value v: x}", id="explicit-value-tag"),
    pytest.param("!!str {=: !!int 5}", id="raw-text-of-value"),
    pytest.param("- &v 05\n- !!str {=: *v}\n", id="alias-value"),
    pytest.param("- !!str {a: &k 1, =: 2}\n- *k\n", id="anchor-in-skipped-entry"),
    pytest.param("- &a !!str {=: x}\n- *a\n", id="anchored"),
])
def test_scalar_tag_on_mapping_uses_value_key(load, reference, source):
    # SafeConstructor.construct_scalar uses a mapping's `=` key as the scalar.
    assert_same(load(source), reference(source))


@pytest.mark.parametrize(("source", "error"), [
    ("!!str {a: 1}", yaml.constructor.ConstructorError),
    ("!!str {=: [1]}", yaml.constructor.ConstructorError),
    ("!!str {=: {x: 1}}", yaml.constructor.ConstructorError),
    ("!!int {=: x}", ValueError),
    ("!!bool {=: 5}", KeyError),
    ("!!timestamp {=: 2001-12-14}", TypeError),
])
def test_scalar_tag_on_mapping_errors(load, reference, source, error):
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


@pytest.mark.parametrize(("source", "error"), [
    ("!!int ''", IndexError),
    ("!!int '-'", IndexError),
    ("!!float ''", IndexError),
    ("!!float '-'", ValueError),
    ("!!float hello", ValueError),
    ("!!float 0x1F", ValueError),
    ("!!float 1:x", ValueError),
    ("!!float '" + ":".join(["0"] * 200) + "'", OverflowError),
])
def test_explicit_float_and_int_errors(load, reference, source, error):
    # PyYAML's int/float constructors index value[0] and call float(), so
    # malformed values raise IndexError / ValueError rather than YAMLError.
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


@pytest.mark.parametrize("value", [
    ".I_NF", "-.In_F", "._nan", " 1.5 ", "--5", "+-1.5", "1: 30", "1_0:3_0.5",
    "١٢.٥", "-Infinity", ":".join(["1"] * 30), ":".join(["7.3"] * 25),
])
def test_explicit_float_matches_pyyaml(load, reference, value):
    # Underscores removed and lowercased first, one sign stripped, Python
    # float() for the rest; sexagesimal parts summed least significant first.
    source = f"!!float '{value}'"
    expected, actual = reference(source), load(source)
    assert type(actual) is float
    assert actual == expected or (actual != actual and expected != expected)


@pytest.mark.parametrize("source", [
    pytest.param("!!str\n�: &node1\n=: &node1", id="fuzz"),
    pytest.param("!!str {a: &k 1, b: &k 2, =: x}", id="two-skipped-entries"),
    pytest.param("- !!str {a: [&k 1], =: x}\n- &k 2\n", id="nested-in-skipped"),
    pytest.param("- !!str {a: &k [1], =: x}\n- &k 2\n", id="skipped-collection"),
])
def test_scalar_tag_on_mapping_duplicate_anchor(load, reference, source):
    # PyYAML composes every entry (so anchors count) but constructs only `=`.
    with pytest.raises(yaml.composer.ComposerError):
        reference(source)
    with pytest.raises(yaml.composer.ComposerError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("- !!str {a: &k !!int 7, =: y}\n- [*k, *k]\n", id="deferred-scalar"),
    pytest.param("- !!str {a: {b: &k 1}, =: x}\n- *k\n", id="nested-deferred-scalar"),
    pytest.param("- !!str {=: &k 05}\n- *k\n", id="value-anchor"),
    pytest.param("- !!str {a: &k ~, =: x}\n- *k\n", id="deferred-null"),
])
def test_scalar_tag_on_mapping_alias_to_skipped_scalar(load, reference, source):
    assert_same(load(source), reference(source))


@pytest.mark.parametrize(("source", "error"), [
    ("- !!str {a: &k !!int x, =: y}\n- *k\n", ValueError),
    ("- !!str {a: &k !o 1, =: 2}\n- *k\n", yaml.constructor.ConstructorError),
])
def test_scalar_tag_on_mapping_alias_constructs_lazily(load, reference, source, error):
    # The skipped scalar is constructed (and fails) only when an alias uses it.
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("!!str\n*1:\n=:", id="fuzz-alias-key"),
    pytest.param("!!str {a: *u, =: x}", id="alias-value"),
    pytest.param("!!str {a: [1, {b: *u}], =: x}", id="nested"),
    pytest.param("!!str {=: x, a: *u}", id="after-value-key"),
])
def test_scalar_tag_on_mapping_undefined_alias(load, reference, source):
    # PyYAML's composer resolves aliases even in entries it never constructs.
    with pytest.raises(yaml.composer.ComposerError):
        reference(source)
    with pytest.raises(yaml.composer.ComposerError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("- &k 1\n- !!str {a: *k, =: x}\n", id="earlier-anchor"),
    pytest.param("- !!str {a: &k [1, *k], =: x}\n", id="recursive-skipped"),
    pytest.param("- !!str {a: &k [1], b: *k, =: x}\n", id="skipped-collection"),
])
def test_scalar_tag_on_mapping_defined_alias_in_skipped_entry(load, reference, source):
    assert_same(load(source), reference(source))


@pytest.mark.parametrize("source", [
    pytest.param("&base\n- e: {<<: *base}", id="fuzz"),
    pytest.param("&b\n- {x: 1}\n- {y: 2, z: {<<: *b}}\n", id="later-item"),
    pytest.param("&b [{a: {<<: *b}, k: 1}]", id="sibling-key"),
    pytest.param("&b [{<<: *b}]", id="self"),
])
def test_recursive_merge_sequence(load, reference, source):
    # The aliased sequence is still under construction when its << is read, so
    # it must be expanded after the document is built, as PyYAML does.
    assert repr(load(source)) == repr(reference(source))


@pytest.mark.parametrize("source", [
    "&b [1, {c: {<<: *b}}]",
    "&b\n- {c: {<<: *b}}\n- 1\n",
])
def test_recursive_merge_sequence_non_mapping(load, reference, source):
    with pytest.raises(yaml.constructor.ConstructorError):
        reference(source)
    with pytest.raises(yaml.constructor.ConstructorError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("- &o !!omap [{x: 1}]\n- {<<: *o}\n", id="omap"),
    pytest.param("- &o !!pairs [{x: 1}, {x: 2}, {y: 3}]\n- {<<: *o, z: 0}\n",
                 id="pairs-first-wins"),
    pytest.param("- &o !!omap [{x: 1}, {y: 2}]\n- *o\n- {<<: *o}\n",
                 id="also-used-as-value"),
    pytest.param("&o !!omap [{x: {<<: *o}}]", id="recursive"),
    pytest.param("- &o !!omap []\n- {<<: *o, a: 1}\n", id="empty"),
])
def test_omap_and_pairs_as_merge_source(load, reference, source):
    # PyYAML merges the node (single-entry mappings), not the tuple list.
    assert repr(load(source)) == repr(reference(source))


@pytest.mark.parametrize(("source", "error"), [
    ("!!str &gode0\n=: &gode0\n =:", yaml.composer.ComposerError),
    ("!!str &a {=: &a x}", yaml.composer.ComposerError),
    ("!!str &a {b: &a x, =: y}", yaml.composer.ComposerError),
    ("!!str {a: &k {b: &k 1}, =: q}", yaml.composer.ComposerError),
    ("!!str &a {=: *a}", RecursionError),
    ("!!str &a {=: {=: *a}}", RecursionError),
])
def test_scalar_tag_on_mapping_own_anchor_errors(load, reference, source, error):
    # The mapping's anchor is registered before its entries, as PyYAML does.
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("- !!str &a {b: *a, =: y}\n- *a\n", id="self-alias-skipped"),
    pytest.param("!!str &o {b: !!int &i {=: *o}, =: q}", id="self-alias-in-skipped-value"),
    pytest.param("- !!str &a {=: !!str &b {=: v}}\n- [*a, *b]\n", id="nested-anchored"),
    pytest.param("- !!str {a: !!int &i {=: 7}, =: q}\n- *i\n", id="skipped-anchored"),
    pytest.param("- !!str {a: &m {=: 7}, =: *m}\n", id="alias-to-value-mapping"),
])
def test_scalar_tag_on_mapping_own_anchor(load, reference, source):
    assert_same(load(source), reference(source))


@pytest.mark.parametrize(("source", "error"), [
    ("[1", yaml.parser.ParserError),
    ("- a\nb: c", yaml.parser.ParserError),
    ("&a &b x", yaml.parser.ParserError),
    ("%YAML 2.0\n--- x", yaml.parser.ParserError),
    ("key: 'x", yaml.scanner.ScannerError),
    ("a: b: c", yaml.scanner.ScannerError),
    ("a: \x07", yaml.reader.ReaderError),
])
def test_parse_error_classes(load, reference, source, error):
    # libyaml-safer's error kind maps to PyYAML's class for that stage.
    for loader in (reference, load):
        with pytest.raises(error) as info:
            loader(source)
        assert type(info.value).__name__ == error.__name__


@pytest.mark.parametrize(("source", "error"), [
    ("[!!int x, &a 1, &a 2]", yaml.composer.ComposerError),
    ("[!!int x, *u]", yaml.composer.ComposerError),
    ("[!!bool maybe, *u]", yaml.composer.ComposerError),
    ("[{<<: 1}, *u]", yaml.composer.ComposerError),
    ("[{a: [!!int x]}, &d 1, &d 2]", yaml.composer.ComposerError),
    ("[&a !!int x, &a 1]", yaml.composer.ComposerError),
    ("[&a 1, &a !!int x]", yaml.composer.ComposerError),
    ("a: !!int x\nb: [1\n", yaml.parser.ParserError),
    ("[!!int x, \x07]", yaml.reader.ReaderError),
])
def test_compose_errors_win_over_construction_errors(load, reference, source, error):
    # PyYAML composes the whole document before constructing it, so parse and
    # composer errors anywhere in the document beat earlier construction errors.
    for loader in (reference, load):
        with pytest.raises(error) as info:
            loader(source)
        assert type(info.value).__name__ == error.__name__


@pytest.mark.skipif(not hasattr(yaml, "CSafeLoader"),
                    reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize(("source", "single", "multi"), [
    ("[!!int x]\n---\n[*u]\n", yaml.composer.ComposerError, ValueError),
    ("[!!int x]\n--- [\n", yaml.composer.ComposerError, ValueError),
    ("[!!int x, ]]", yaml.parser.ParserError, ValueError),
    ("[!!int x]\n...\n", ValueError, ValueError),
])
def test_construction_error_vs_stream_errors(source, single, multi):
    # Single-document loading checks for the end of the stream before
    # constructing; load_all constructs each document before reading the next.
    cases = [
        (single, lambda s: yaml.load(s, Loader=yaml.CSafeLoader)),
        (single, lambda s: yaml.load(s, Loader=ryaml.RSafeLoader)),
        (single, ryaml.loads),
        (multi, lambda s: list(yaml.load_all(s, Loader=yaml.CSafeLoader))),
        (multi, lambda s: list(yaml.load_all(s, Loader=ryaml.RSafeLoader))),
        (multi, ryaml.loads_all),
    ]
    for error, loader in cases:
        with pytest.raises(error) as info:
            loader(source)
        assert type(info.value).__name__ == error.__name__


@pytest.mark.parametrize("source", [
    pytest.param("!!pairs [!o {a: 1}, !!str {b: 2}]", id="entry-tags-ignored"),
    pytest.param("- !!omap [&e !!set {a}]\n- *e\n", id="set-alias-is-set"),
    pytest.param("- !!pairs [&e !!set {a: 1}]\n- {<<: *e}\n", id="set-alias-merges"),
    pytest.param("- !!omap [&e !o {a: 1}]\n- {<<: *e}\n", id="tagged-alias-merges"),
    pytest.param("- !!omap [&e !!map {a: 1}]\n- *e\n", id="map-alias"),
])
def test_tagged_pair_entries(load, reference, source):
    assert_same(load(source), reference(source))


@pytest.mark.parametrize("source", [
    "- !!omap [&e !o {a: 1}]\n- *e\n",
    "- !!pairs [&e !!omap {a: 1}]\n- *e\n",
    "- !!pairs [&e !!str {a: 1}]\n- *e\n",
])
def test_tagged_pair_entry_alias_constructed_by_tag(load, reference, source):
    # The entry's tag is ignored for the pair, but an alias used as a value is
    # constructed by it.
    with pytest.raises(yaml.constructor.ConstructorError):
        reference(source)
    with pytest.raises(yaml.constructor.ConstructorError):
        load(source)


@pytest.mark.parametrize("source", [
    pytest.param("!!pairs\n- &y {{}}", id="fuzz"),
    pytest.param("!!omap [&y {[1]: 2}]", id="list-key"),
    pytest.param("- !!pairs [&y !!set {[1]}]\n", id="set-entry"),
    pytest.param("- &p !!pairs [&y {[1]: 2}]\n- *p\n", id="alias-to-pairs"),
])
def test_anchored_pair_entry_unhashable_key(load, reference, source):
    # The pair itself is fine; only using the entry as a mapping fails.
    assert repr(load(source)) == repr(reference(source))


@pytest.mark.parametrize(("source", "error"), [
    ("- !!pairs [&y {[1]: 2}]\n- *y\n", yaml.constructor.ConstructorError),
    ("- !!pairs [&y {[1]: 2}]\n- {<<: *y}\n", yaml.constructor.ConstructorError),
    ("- !!pairs [&y !!set {[1]}]\n- *y\n", yaml.constructor.ConstructorError),
    ("- !!pairs [&y {[1]: 2}]\n- &y 3\n", yaml.composer.ComposerError),
])
def test_anchored_pair_entry_unhashable_key_alias(load, reference, source, error):
    with pytest.raises(error):
        reference(source)
    with pytest.raises(error):
        load(source)


@pytest.mark.parametrize(("source", "error"), [
    ("!- &entry\n- *entry", yaml.constructor.ConstructorError),
    ("&e !o {a: *e}", yaml.constructor.ConstructorError),
    ("&m !!str {a: *m}", yaml.constructor.ConstructorError),
    ("&s !!set [*s]", yaml.constructor.ConstructorError),
    ("[&e !o [1], &e 2]", yaml.composer.ComposerError),
    ("&e !o [1, *u]", yaml.composer.ComposerError),
])
def test_failed_collection_keeps_its_anchor(load, reference, source, error):
    # A node that fails to construct still holds its anchor (PyYAML composed
    # it first), so aliases to it are defined and duplicates still count.
    for loader in (reference, load):
        with pytest.raises(error) as info:
            loader(source)
        assert type(info.value).__name__ == error.__name__


def test_reader_error_from_rust_has_message():
    # Raised from Rust with a plain message (control character), str() must
    # work like any YAMLError.
    with pytest.raises(yaml.reader.ReaderError) as info:
        ryaml.loads("a: \x07")
    assert "control characters are not allowed" in str(info.value)


BYTE_STREAMS = {
    "utf8-bom": codecs.BOM_UTF8 + "a: é".encode(),
    "utf16le-bom": codecs.BOM_UTF16_LE + "a: é".encode("utf-16-le"),
    "utf16be-bom": codecs.BOM_UTF16_BE + "a: é".encode("utf-16-be"),
    "utf16le-astral": codecs.BOM_UTF16_LE + "a: 😀".encode("utf-16-le"),
    "utf16le-two-docs": codecs.BOM_UTF16_LE + "a\n---\nb".encode("utf-16-le"),
    "utf16le-no-bom": "a: 1".encode("utf-16-le"),
    "utf32-bom": codecs.BOM_UTF32_LE + "a: 1".encode("utf-32-le"),
    "invalid-start": b"a: \xff",
    "truncated": b"a: \xc3",
    "surrogate": b"a: \xed\xa0\x80",
    "latin1": "a: é".encode("latin-1"),
    "utf16-odd-length": codecs.BOM_UTF16_LE + b"a\x00:",
    "utf16-lone-surrogate": codecs.BOM_UTF16_LE + b"\x00\xd8",
    "bom-mid-document": b"a: 1\n" + codecs.BOM_UTF8 + b"b: 2",
    "values": b"[1, 2.5, yes]",
}


@pytest.mark.skipif(not hasattr(yaml, "CSafeLoader"),
                    reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("data", BYTE_STREAMS.values(), ids=BYTE_STREAMS.keys())
def test_byte_streams(data):
    # YAML 1.1 5.2: a UTF-16 BOM selects UTF-16, otherwise UTF-8 (no UTF-32,
    # no BOM-less detection). Invalid bytes raise ReaderError, not
    # UnicodeDecodeError (which is not a yaml.YAMLError).
    def outcome(load):
        try:
            return repr(load(data))
        except Exception as e:  # noqa: BLE001
            return type(e).__name__
    expected = outcome(lambda d: list(yaml.load_all(d, Loader=yaml.CSafeLoader)))
    assert outcome(lambda d: list(yaml.load_all(d, Loader=ryaml.RSafeLoader))) == expected
    assert outcome(lambda d: ryaml.load_all(io.BytesIO(d))) == expected
    assert outcome(ryaml.loads_all) == expected


def test_rsafeloader_uses_native_methods():
    # If these are not the extension's own methods, RSafeLoader silently falls
    # back to PyYAML's pure-Python BaseLoader (every scalar a string).
    for name in ("check_data", "get_data", "get_single_data"):
        method = getattr(ryaml.RSafeLoader, name)
        assert type(method).__name__ == "method_descriptor", (name, method)
