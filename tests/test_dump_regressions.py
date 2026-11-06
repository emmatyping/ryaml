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


# ryaml's dumper recurses natively, up to a fixed depth on every Python
# version (PyYAML's limit depends on the recursion limit and the caller's stack).
MAX_DEPTH = 500


@pytest.mark.parametrize("kind", ["list", "dict"])
@pytest.mark.parametrize("leaf", ["x", [], object()], ids=["scalar", "empty", "unrepresentable"])
@pytest.mark.parametrize("recursion_limit", [100, 1000, 100_000])
def test_deep_nesting_limit_is_fixed(kind, leaf, recursion_limit):
    import sys
    old_limit = sys.getrecursionlimit()
    sys.setrecursionlimit(recursion_limit)
    try:
        within = _dump_outcome(ryaml.RSafeDumper, _nested(MAX_DEPTH, leaf, kind))
        beyond = _dump_outcome(ryaml.RSafeDumper, _nested(MAX_DEPTH + 1, leaf, kind))
        with pytest.raises(RecursionError):
            ryaml.dumps(_nested(MAX_DEPTH + 1, leaf, kind))
    finally:
        sys.setrecursionlimit(old_limit)
    assert within == ("RepresenterError" if leaf.__class__ is object else "ok")
    assert beyond == "RecursionError"


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("kind", ["list", "dict"])
def test_deep_nesting_like_pyyaml_within_both_limits(kind):
    # Where PyYAML can dump the data, ryaml writes the same output.
    value = _nested(250, "x", kind)
    assert yaml.dump(value, Dumper=ryaml.RSafeDumper) == yaml.dump(value, Dumper=yaml.CSafeDumper)


@pytest.mark.parametrize("kind", ["list", "dict"])
def test_deep_nesting_in_a_thread_with_a_small_stack(kind):
    # The limit holds whatever the thread's stack size (musl gives threads
    # 128 KB): the dumper grows its stack instead of overflowing it.
    import threading
    results = []

    def dump():
        try:
            results.append(ryaml.dumps(_nested(MAX_DEPTH, "x", kind)).count("x"))
            ryaml.dumps(_nested(MAX_DEPTH + 1, "x", kind))
        except RecursionError:
            results.append("RecursionError")

    old_size = threading.stack_size(128 * 1024)
    try:
        thread = threading.Thread(target=dump)
        thread.start()
        thread.join()
    finally:
        threading.stack_size(old_size)
    assert results == [1, "RecursionError"]


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


class _RecordingStream:
    def __init__(self):
        self.writes = []

    def write(self, data):
        self.writes.append(data)


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("docs", [
    pytest.param([{"a": 1}, [1, 2]], id="small"),
    pytest.param([["x" * 50] * 1000], id="many-items"),
    pytest.param(["\xe9" * 20000], id="long-scalar"),
    pytest.param([{"k%d" % i: "v" * 30 for i in range(2000)}, "tail"], id="large-mapping"),
])
@pytest.mark.parametrize("encoding", [None, "utf-16-le"])
@pytest.mark.parametrize("style", [None, "'", '"'])
def test_output_written_when_libyaml_flushes(docs, encoding, style):
    # CEmitter writes each libyaml flush to the stream: at every document end
    # and whenever the 16 KB buffer fills, before every character.
    expected, actual = _RecordingStream(), _RecordingStream()
    yaml.dump_all(docs, expected, Dumper=yaml.CSafeDumper, encoding=encoding, default_style=style)
    yaml.dump_all(docs, actual, Dumper=ryaml.RSafeDumper, encoding=encoding, default_style=style)
    assert actual.writes == expected.writes


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
def test_documents_written_before_close():
    streams = []
    for dumper in (yaml.CSafeDumper, ryaml.RSafeDumper):
        stream = _RecordingStream()
        instance = dumper(stream)
        instance.open()
        instance.represent({"a": [1]})
        instance.represent("b")
        streams.append(stream.writes)
    assert streams[1] == streams[0]


class _BinaryStream(_RecordingStream):
    pass


class _TextStream(_RecordingStream):
    encoding = None


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("make_stream", [
    __import__("io").StringIO, __import__("io").BytesIO, _BinaryStream, _TextStream,
], ids=["StringIO", "BytesIO", "no-encoding-attribute", "encoding-attribute"])
@pytest.mark.parametrize("encoding", [None, "utf-8", "utf-16-le"])
def test_text_or_bytes_chosen_by_stream_like_pyyaml(make_stream, encoding):
    # CEmitter writes str (UTF-8) to a stream with an `encoding` attribute, or
    # without an encoding; otherwise bytes in the requested encoding.
    def run(dumper):
        stream = make_stream()
        try:
            yaml.dump_all([{"a": "\xe9"}], stream, Dumper=dumper, encoding=encoding)
        except Exception as e:  # noqa: BLE001
            return type(e).__name__
        if hasattr(stream, "getvalue"):
            return stream.getvalue()
        return stream.writes
    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


class _RefusesFirstWrite(_RecordingStream):
    def write(self, data):
        if not hasattr(self, "refused"):
            self.refused = True
            raise TypeError("first write refused")
        super().write(data)


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("make_stream", [__import__("io").BytesIO, _RefusesFirstWrite],
                         ids=["always-refuses", "refuses-first"])
@pytest.mark.parametrize("options", [
    {}, {"explicit_end": True}, {"encoding": "utf-16-le"},
    {"encoding": "utf-16-le", "explicit_end": True},
], ids=["implicit", "explicit-end", "utf-16", "utf-16-explicit-end"])
def test_failed_writes_like_libyaml(make_stream, options):
    # When the stream's write fails, libyaml drops the UTF-8 output (for UTF-16
    # it keeps it, and writes it twice later) and processes the failed event
    # again with the next one; the stream's own exception is raised.
    def run(dumper):
        stream = make_stream()
        instance = dumper(stream, **options)
        results = []
        for op in ("open", "represent", "represent", "close", "close"):
            try:
                if op == "represent":
                    instance.represent({"a": [1]})
                else:
                    getattr(instance, op)()
                results.append(None)
            except Exception as e:  # noqa: BLE001
                results.append(type(e).__name__)
        return results, getattr(stream, "writes", None) or stream.getvalue()
    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


def _partly_unrepresentable():
    items = []
    items.append(items)
    items.append(StrSubclass("s"))
    mapping = {"a": 1}
    mapping["self"] = mapping
    mapping["bad"] = frozenset()
    return [items, mapping, {"k": [1, 2, object()]}]


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("index", range(3))
def test_failed_represent_leaves_partial_nodes_like_pyyaml(index):
    # PyYAML adds each item to the collection's node as it goes and does not
    # forget represented objects when representing fails, so representing the
    # object again reuses the partly filled node.
    def run(dumper):
        import io
        value = _partly_unrepresentable()[index]
        stream = io.StringIO()
        instance = dumper(stream, sort_keys=False)
        instance.open()
        results = []
        again = value[0] if isinstance(value, list) else value.get("self", value.get("k"))
        for target in (value, again):
            try:
                instance.represent(target)
                results.append(None)
            except Exception as e:  # noqa: BLE001
                results.append(type(e).__name__)
        return results, stream.getvalue()
    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
def test_dumper_class_sorts_keys_by_default():
    # PyYAML's dumper classes default to sort_keys=True (yaml.dump_all always
    # passes it, so only direct use shows the default).
    def run(dumper):
        import io
        stream = io.StringIO()
        instance = dumper(stream)
        instance.open()
        instance.represent({"b": 1, "a": {2, 1}})
        instance.close()
        return stream.getvalue()
    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


@pytest.mark.parametrize(("name", "value"), [
    ("default_style", "'x"), ("default_style", "''"), ("default_style", 0),
    ("default_style", "|x"), ("default_style", "x"),
    ("default_flow_style", 0), ("default_flow_style", 1), ("default_flow_style", "yes"),
    ("canonical", 1), ("canonical", ""), ("allow_unicode", 1),
    ("explicit_start", "x"), ("explicit_end", 1), ("sort_keys", 0),
    ("indent", 2.7), ("indent", "3"), ("indent", float("nan")), ("width", 9.99),
    ("width", 2**31), ("line_break", 5), ("line_break", "x"),
    ("version", (1, 1, 0)), ("version", [1, 2]), ("version", (1,)), ("version", "11"),
    ("tags", {"!e!": 1}), ("tags", {1: "x"}), ("tags", {"!e!": b"p:"}),
    ("tags", {"!e%d!" % i: "p:" for i in range(129)}), ("tags", {}),
])
def test_option_values_like_pyyaml(reference, name, value):
    # PyYAML uses most options by truthiness, compares default_style and
    # line_break with exact strings, converts indent and width like int(), and
    # checks version and tags when serializing each document.
    docs = [{"a": [1, "x y"], "b": "line\nbreak"}, "s"]
    assert_same_dump(reference, docs, **{name: value})


@pytest.mark.skipif(not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML")
@pytest.mark.parametrize("options", [{"tags": {1: "x"}}, {"version": (1,)}],
                         ids=["tags", "version"])
def test_directives_checked_per_document(options):
    def run(dumper):
        import io
        try:
            instance = dumper(io.StringIO(), **options)
        except Exception as e:  # noqa: BLE001
            return "init", type(e).__name__
        instance.open()
        try:
            instance.represent("a")
        except Exception as e:  # noqa: BLE001
            return "represent", type(e).__name__
        return "ok", None
    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


@pytest.mark.parametrize("value", ["\u7eff", "a\u7effb", "\ufeff", "a\ufeffb", "\u7eff\U000136ef"])
@pytest.mark.parametrize("style", [None, "'", '"'])
@pytest.mark.parametrize("allow_unicode", [True, False])
def test_only_the_bom_is_escaped_as_a_bom(reference, value, style, allow_unicode):
    # libyaml escapes U+FEFF in double-quoted scalars; U+7EFF (a CJK
    # character) is ordinary text.
    assert_same_dump(reference, [value], default_style=style, allow_unicode=allow_unicode)


@pytest.mark.parametrize("value", [
    2**24, 2**63 - 1, 2**63, 2**64, 2**127, 2**128, -2**128, 10**40,
], ids=lambda v: f"bits-{v.bit_length()}")
def test_big_ints_like_pyyaml(reference, value):
    # https://github.com/emmatyping/ryaml/issues/44: ints past 64 bits failed
    # to dump (the old dumper went through 128-bit integers).
    assert_same_dump(reference, [{"t1": value}, [value]])
    import io
    stream = io.StringIO()
    ryaml.dump(stream, {"t1": value})
    assert stream.getvalue() == yaml.safe_dump({"t1": value})
    assert ryaml.loads(stream.getvalue()) == {"t1": value}
