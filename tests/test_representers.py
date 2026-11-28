"""Custom representer compatibility with PyYAML's SafeDumper."""

from dataclasses import dataclass
from datetime import timedelta
import io
from pathlib import PurePath, PurePosixPath, PureWindowsPath

from hypothesis import given, settings, strategies as st
import pytest
import ryaml

yaml = pytest.importorskip("yaml")

STR = "tag:yaml.org,2002:str"
INT = "tag:yaml.org,2002:int"
SEQ = "tag:yaml.org,2002:seq"
MAP = "tag:yaml.org,2002:map"


def dumper_pair():
    class Actual(ryaml.RSafeDumper):
        pass

    class Expected(yaml.SafeDumper):
        pass

    return Actual, Expected


def test_paths_and_durations_work_in_all_dump_apis():
    actual, expected = dumper_pair()

    def represent_path(dumper, path):
        return dumper.represent_scalar(STR, path.as_posix())

    def represent_duration(dumper, duration):
        return dumper.represent_scalar(INT, str(int(duration.total_seconds())))

    ryaml.add_multi_representer(PurePath, represent_path, Dumper=actual)
    ryaml.add_representer(timedelta, represent_duration, Dumper=actual)
    yaml.add_multi_representer(PurePath, represent_path, Dumper=expected)
    yaml.add_representer(timedelta, represent_duration, Dumper=expected)
    value = {
        "timeout": timedelta(minutes=25),
        "paths": [PurePosixPath("src/config.yml"), PureWindowsPath("C:/config.yml")],
    }
    reference = yaml.dump(value, Dumper=expected, sort_keys=False)
    assert yaml.dump(value, Dumper=actual, sort_keys=False) == reference
    assert ryaml.dumps(value, Dumper=actual) == reference
    for stream in (io.StringIO(), io.BytesIO()):
        ryaml.dump(stream, value, Dumper=actual)
        expected_output = reference if isinstance(stream, io.StringIO) else reference.encode()
        assert stream.getvalue() == expected_output
    assert ryaml.loads(reference) == {"timeout": 1500, "paths": ["src/config.yml", "C:/config.yml"]}


def test_module_registration_defaults_to_rsafe_dumper(monkeypatch):
    class Value:
        pass

    class Derived(Value):
        pass

    actual = ryaml.RSafeDumper
    monkeypatch.setattr(actual, "yaml_representers", dict(actual.yaml_representers))
    monkeypatch.setattr(actual, "yaml_multi_representers", dict(actual.yaml_multi_representers))
    ryaml.add_representer(Value, lambda dumper, value: dumper.represent_scalar(STR, "exact"))
    ryaml.add_multi_representer(Value, lambda dumper, value: dumper.represent_scalar(STR, "multi"))

    assert ryaml.loads(ryaml.dumps([Value(), Derived()])) == ["exact", "multi"]
    stream = io.StringIO()
    ryaml.dump(stream, Derived())
    assert ryaml.loads(stream.getvalue()) == "multi"
    with pytest.raises(yaml.representer.RepresenterError):
        yaml.safe_dump(Value())
    with pytest.raises(ryaml.RepresenterError):
        ryaml.dumps(object())


def test_exact_type_mro_fallback_and_registration_scope():
    class Left:
        pass

    class Right:
        pass

    class Both(Left, Right):
        pass

    class Exact(Both):
        pass

    class IntSubclass(int):
        pass

    def label(text):
        return lambda dumper, value: dumper.represent_scalar(STR, text)

    actual, expected = dumper_pair()
    for parent in (actual, expected):
        parent.add_multi_representer(Right, label("right"))
        parent.add_multi_representer(Left, label("left"))
        parent.add_multi_representer(int, label("int subclass"))
        parent.add_representer(None, label("exact fallback"))

    class ActualChild(actual):
        pass

    class ExpectedChild(expected):
        pass

    for child in (ActualChild, ExpectedChild):
        yaml.add_representer(Exact, label("exact"), Dumper=child)
        yaml.add_representer(int, label("int override"), Dumper=child)
        yaml.add_multi_representer(None, label("multi fallback"), Dumper=child)

    value = [Left(), Right(), Both(), Exact(), 1, IntSubclass(1), object()]
    actual_parent = yaml.dump(value, Dumper=actual)
    actual_child = yaml.dump(value, Dumper=ActualChild)
    assert actual_parent == yaml.dump(value, Dumper=expected)
    assert actual_child == yaml.dump(value, Dumper=ExpectedChild)
    assert yaml.safe_load(actual_parent) == [
        "left", "right", "left", "left", 1, "int subclass", "exact fallback",
    ]
    assert yaml.safe_load(actual_child) == [
        "left", "right", "left", "exact", "int override", "int subclass", "multi fallback",
    ]
    with pytest.raises(ryaml.RepresenterError):
        ryaml.dumps(Both())


def test_recursive_collections_and_aliases_reset_between_documents():
    actual, expected = dumper_pair()
    calls = []

    class Loop:
        pass

    def represent_loop(dumper, value):
        calls.append(dumper)
        return dumper.represent_sequence(SEQ, [value], flow_style=True)

    for dumper in (actual, expected):
        dumper.add_representer(Loop, represent_loop)

    value = Loop()
    docs = [[value, value], [value, value]]
    output = yaml.dump_all(docs, Dumper=actual)
    assert len(calls) == 2 and all(isinstance(call, actual) for call in calls)
    assert output == yaml.dump_all(docs, Dumper=expected)
    first, second = yaml.safe_load_all(output)
    assert first[0] is first[1] is first[0][0]
    assert second[0] is second[1] is second[0][0]
    assert first[0] is not second[0]


def test_callback_exception_propagates():
    actual, expected = dumper_pair()
    failure = RuntimeError("representer failed")

    def fail(dumper, value):
        raise failure

    for dumper in (actual, expected):
        dumper.add_representer(object, fail)
        with pytest.raises(RuntimeError) as error:
            yaml.dump({"nested": object()}, Dumper=dumper)
        assert error.value is failure


def test_retry_after_a_representer_fails_partway_through_a_collection():
    class Collection:
        pass

    def represent_collection(dumper, value):
        return dumper.represent_sequence(SEQ, [1, object()])

    def run(dumper_class):
        dumper_class.add_representer(Collection, represent_collection)
        stream = io.StringIO()
        dumper = dumper_class(stream)
        value = Collection()
        dumper.open()
        with pytest.raises(yaml.representer.RepresenterError):
            dumper.represent(value)
        dumper.represent(value)
        dumper.close()
        return stream.getvalue()

    actual, expected = dumper_pair()
    assert run(actual) == run(expected) == "- 1\n"


@dataclass
class Scalar:
    value: str
    style: str | None


class DerivedScalar(Scalar):
    pass


@dataclass
class Sequence:
    items: list


@dataclass
class Mapping:
    items: dict


scalars = st.builds(
    DerivedScalar, st.text(max_size=24), st.sampled_from([None, "'", '"', "|", ">"]),
)
nested = st.recursive(
    st.one_of(scalars, st.integers(), st.none()),
    lambda children: st.one_of(
        st.builds(Sequence, st.lists(children, max_size=4)),
        st.builds(Mapping, st.dictionaries(st.text(max_size=8), children, max_size=4)),
    ),
    max_leaves=12,
)


def yaml_events(text):
    """Compare node values, tags, and anchor topology across emitters."""
    return [
        (type(event), {
            key: val for key, val in vars(event).items()
            if key not in {"start_mark", "end_mark", "style"}
        })
        for event in yaml.parse(text)
    ]


@given(scalars, st.lists(nested, max_size=4), st.sampled_from([None, True, False]), st.booleans())
@settings(max_examples=300)
def test_custom_node_graphs_match_safe_dumper(shared, children, flow_style, sort_keys):
    actual, expected = dumper_pair()
    for dumper in (actual, expected):
        dumper.add_multi_representer(
            Scalar, lambda d, value: d.represent_scalar(STR, value.value, style=value.style),
        )
        dumper.add_representer(Sequence, lambda d, value: d.represent_sequence("!seq", value.items))
        dumper.add_representer(Mapping, lambda d, value: d.represent_mapping("!map", value.items))

    value = Mapping({"z": shared, "a": Sequence([shared, *children])})
    options = {"default_flow_style": flow_style, "sort_keys": sort_keys}
    assert yaml_events(yaml.dump(value, Dumper=actual, **options)) == yaml_events(
        yaml.dump(value, Dumper=expected, **options),
    )


def test_node_helpers_honor_explicit_styles_and_accept_pyyaml_nodes():
    actual, expected = dumper_pair()

    def represent(dumper, value):
        return dumper.represent_mapping(MAP, [
            ("scalar", yaml.nodes.ScalarNode(STR, "one\ntwo\n", style="|")),
            ("sequence", Sequence([1, 2])),
        ], flow_style=False)

    for dumper in (actual, expected):
        # A callback can also construct nodes directly with PyYAML's node API.
        dumper.add_representer(yaml.nodes.ScalarNode, lambda d, node: node)
        dumper.add_representer(
            Sequence, lambda d, value: d.represent_sequence("!seq", value.items, flow_style=True),
        )
        dumper.add_representer(object, represent)

    assert yaml.dump(object(), Dumper=actual) == yaml.dump(object(), Dumper=expected)


needs_libyaml = pytest.mark.skipif(
    not hasattr(yaml, "CSafeDumper"), reason="PyYAML was built without LibYAML",
)


def assert_same_outcome(actual, expected):
    """The same output, or exceptions where ryaml's is (a subclass of) PyYAML's."""
    if isinstance(expected, type) and issubclass(expected, BaseException):
        assert isinstance(actual, type) and issubclass(actual, expected), (actual, expected)
    else:
        assert actual == expected


class Box:
    def __init__(self, value=None):
        self.value = value


class Wrap(Box):
    pass


class Clobber(Box):
    pass


class Retag(Box):
    pass


def reentrant_representers(dumper):
    dumper.add_representer(Box, lambda d, x: d.represent_sequence("!box", x.value))
    dumper.add_representer(Wrap, lambda d, x: d.represent_data(x.value))

    def clobber(d, x):
        # As in PyYAML, representing x.value moves `alias_key` on, so the
        # scalar is recorded for the last object represented, not for x.
        d.represent_data(x.value)
        return d.represent_scalar("!clobber", "v")

    def retag(d, x):
        node = d.represent_data(x.value)
        node.tag = "!retagged"
        return node

    dumper.add_representer(Clobber, clobber)
    dumper.add_representer(Retag, retag)
    dumper.add_representer(
        __import__("collections").OrderedDict, lambda d, x: d.represent_dict(x.items()),
    )
    # Replacing a built-in type's representer.
    dumper.add_representer(
        str, lambda d, x: d.represent_scalar(STR, x, style="|" if "\n" in x else None),
    )
    return dumper


def reentrant_cases():
    box = Box()
    box.value = [box]
    yield "box-cycle", box
    native = []
    native.append(Box(native))
    yield "native-through-callback-to-itself", native
    shared = [1, 2]
    yield "shared-after-callback", {"a": shared, "b": Wrap(shared)}
    yield "shared-before-callback", {"a": Wrap(shared), "b": shared}
    inner = {"k": 1}
    clobber = Clobber(inner)
    yield "alias-key-moves-on", [clobber, clobber, inner]
    retagged = Retag({"x": [1, 2]})
    yield "node-changed-after-represent-data", [retagged, retagged]
    mapping = {"q": 1}
    yield "changed-node-shared", {"a": mapping, "b": Retag(mapping)}
    yield "ordered-dict", __import__("collections").OrderedDict([("b", 1), ("a", [2, 3])])
    yield "str-override", {"text": "a\nb\n", "n": 1}
    loop = {}
    loop["self"] = Wrap(loop)
    yield "dict-through-callback-to-itself", loop
    wrap = Wrap()
    wrap.value = [wrap]
    yield "callback-recursion", wrap


@needs_libyaml
@pytest.mark.parametrize("options", [
    {}, {"default_flow_style": None}, {"default_style": '"'}, {"default_style": "x"},
    {"sort_keys": False},
], ids=repr)
@pytest.mark.parametrize("name,value", list(reentrant_cases()), ids=lambda v: v if isinstance(v, str) else "")
def test_reentrant_representers_match_csafedumper(name, value, options):
    class Actual(ryaml.RSafeDumper):
        pass

    class Expected(yaml.CSafeDumper):
        pass

    reentrant_representers(Actual)
    reentrant_representers(Expected)

    def outcome(dumper):
        try:
            return yaml.dump(value, Dumper=dumper, **options)
        except Exception as error:
            return type(error)

    assert_same_outcome(outcome(Actual), outcome(Expected))


class Holder:
    pass


class SubclassedScalarNode(yaml.nodes.ScalarNode):
    pass


class StrSubclass(str):
    pass


class Reiterable:
    """Iterates as `first` the first time and as `then` after that."""

    def __init__(self, first, then):
        self.first, self.then = first, then

    def __iter__(self):
        items, self.first = self.first, self.then
        return iter(items)


def failing_after(items):
    """Yields `items`, then raises RuntimeError."""
    yield from items
    raise RuntimeError("no more items")


def dump_session(base, documents, representer, **options):
    """Each call's outcome on a dumper registering `representer` for Holder,
    and what it wrote, so that what a bad node leaves behind is compared too."""
    dumper_class = type("Dumper", (base,), {})
    dumper_class.add_representer(Holder, representer)
    stream = io.StringIO()
    dumper = dumper_class(stream, **options)
    calls = [dumper.open, *(lambda d=d: dumper.represent(d) for d in documents), dumper.close]
    outcomes = []
    for call in calls:
        try:
            call()
            outcomes.append(None)
        except Exception as error:
            outcomes.append(type(error))
    return outcomes, stream.getvalue()


def assert_same_session(actual, expected):
    (actual_outcomes, actual_output), (expected_outcomes, expected_output) = actual, expected
    assert actual_output == expected_output
    for actual_outcome, expected_outcome in zip(actual_outcomes, expected_outcomes, strict=True):
        assert_same_outcome(actual_outcome, expected_outcome)


def key():
    return yaml.nodes.ScalarNode(STR, "k")


def scalar(tag: object = STR, value: object = "v", **kwargs):
    return yaml.nodes.ScalarNode(tag, value, **kwargs)


MALFORMED_NODES = {
    # CEmitter accepts exactly str or bytes for tags and scalar values.
    "bytes-tag": lambda: scalar(b"!b"),
    "bytes-standard-tag": lambda: scalar(b"tag:yaml.org,2002:str"),
    "bytes-tag-like-the-resolved-one": lambda: scalar(b"tag:yaml.org,2002:int", "1"),
    "bytes-collection-tag": lambda: yaml.nodes.SequenceNode(b"!s", [scalar()]),
    "bytes-value": lambda: scalar(STR, b"v"),
    "bytes-value-like-an-int": lambda: scalar(STR, b"1"),
    "bytes-value-tagged-int": lambda: scalar(INT, b"1"),
    "empty-bytes-value": lambda: scalar(STR, b""),
    "str-subclass-tag": lambda: scalar(StrSubclass("!s")),
    "str-subclass-value": lambda: scalar(STR, StrSubclass("v")),
    "int-value": lambda: scalar(STR, 5),
    "int-tag": lambda: scalar(5),
    "no-tag": lambda: scalar(None),
    "no-collection-tag": lambda: yaml.nodes.MappingNode(None, []),
    "empty-tag": lambda: scalar(""),
    # Unlike no tag, which a collection can have, an empty one is refused.
    "empty-collection-tag": lambda: yaml.nodes.SequenceNode("", []),
    "empty-bytes-collection-tag": lambda: yaml.nodes.MappingNode(b"", []),
    "nul-first-collection-tag": lambda: yaml.nodes.SequenceNode("\0!s", []),
    "nul-in-tag": lambda: scalar("tag:yaml.org,2002:str\0x"),
    "nul-in-bytes-tag": lambda: scalar(b"!a\0b"),
    "unknown-style": lambda: scalar(style="x"),
    "bytes-style": lambda: scalar(style=b"'"),
    "truthy-flow-style": lambda: yaml.nodes.SequenceNode(SEQ, [], flow_style="yes"),
    "falsy-flow-style": lambda: yaml.nodes.SequenceNode(SEQ, [scalar()], flow_style=0),
    # Mapping items are unpacked like `key, value = item`.
    "list-pairs": lambda: yaml.nodes.MappingNode(MAP, [[key(), scalar()]]),
    "iterable-pairs": lambda: yaml.nodes.MappingNode(MAP, [Reiterable([key(), scalar()], None)]),
    "three-item-pair": lambda: yaml.nodes.MappingNode(MAP, [(key(),) * 3]),
    "one-item-pair": lambda: yaml.nodes.MappingNode(MAP, [(key(),)]),
    "non-iterable-pair": lambda: yaml.nodes.MappingNode(MAP, [5]),
    "dict-items": lambda: yaml.nodes.MappingNode(MAP, {key(): scalar()}),
    "tuple-items": lambda: yaml.nodes.SequenceNode(SEQ, (scalar(),)),
    "non-iterable-items": lambda: yaml.nodes.SequenceNode(SEQ, 5),
    # CEmitter iterates a collection's value twice: in the anchor pass, then
    # to serialize the items, using what it finds the second time.
    "generator-items": lambda: yaml.nodes.SequenceNode(SEQ, (x for x in [scalar()])),
    "iterator-mapping-items": lambda: yaml.nodes.MappingNode(MAP, iter([(key(), scalar())])),
    "iterator-pair": lambda: yaml.nodes.MappingNode(MAP, [iter([key(), scalar()])]),
    "items-fail-the-second-time": lambda: yaml.nodes.SequenceNode(
        SEQ, Reiterable([scalar()], 5),
    ),
    "items-shrink": lambda: yaml.nodes.SequenceNode(SEQ, Reiterable([scalar(), scalar()], [])),
    "items-reorder": lambda: (lambda a, b: yaml.nodes.SequenceNode(
        SEQ, Reiterable([a, b], [b, a]),
    ))(scalar(value="a"), scalar(value="b")),
    "items-gain-an-unseen-node": lambda: yaml.nodes.SequenceNode(
        SEQ, Reiterable([scalar()], [scalar(None), scalar()]),
    ),
    "pair-value-unseen": lambda: (lambda k: yaml.nodes.MappingNode(
        MAP, Reiterable([(k, scalar())], [(k, scalar())]),
    ))(scalar(None)),
    # The anchor pass goes through each item before it takes the next.
    "bad-items-in-a-pair-before-a-bad-pair": lambda: (lambda k: yaml.nodes.MappingNode(
        MAP, [(k, yaml.nodes.SequenceNode(SEQ, 5)), (k,)],
    ))(key()),
    "bad-items-in-an-item-before-the-items-fail": lambda: yaml.nodes.SequenceNode(
        SEQ, failing_after([yaml.nodes.SequenceNode(SEQ, [yaml.nodes.MappingNode(MAP, [5])])]),
    ),
    "pair-key-unseen": lambda: yaml.nodes.MappingNode(
        MAP, Reiterable([(key(), scalar(None))], [(key(), scalar(None))]),
    ),
}


@needs_libyaml
@pytest.mark.parametrize("make_node", MALFORMED_NODES.values(), ids=MALFORMED_NODES.keys())
def test_node_objects_are_read_like_csafedumper(make_node):
    # The second document shows whether the bad node left the emitter stuck.
    documents = [["first", Holder(), "last"], ["after"]]
    assert_same_session(
        dump_session(ryaml.RSafeDumper, documents, lambda d, x: make_node()),
        dump_session(yaml.CSafeDumper, documents, lambda d, x: make_node()),
    )


# Each raises a different error, at a different point: items are read in the
# anchor pass, the rest as the node is serialized.
FAILING_NODES = [
    "no-tag", "empty-bytes-value", "int-value", "int-tag", "non-iterable-items",
    "one-item-pair", "items-fail-the-second-time", "items-gain-an-unseen-node",
]


@needs_libyaml
@pytest.mark.parametrize("second", FAILING_NODES)
@pytest.mark.parametrize("first", FAILING_NODES)
def test_the_first_error_is_csafedumpers(first, second):
    def session(base):
        nodes = iter([MALFORMED_NODES[first](), MALFORMED_NODES[second]()])
        return dump_session(base, [[Holder(), "x", Holder()], ["after"]], lambda d, x: next(nodes))

    assert_same_session(session(ryaml.RSafeDumper), session(yaml.CSafeDumper))


@needs_libyaml
def test_a_node_from_a_failed_document_is_an_alias_in_the_next():
    # Like CEmitter, ryaml keeps the anchors and serialized nodes of a
    # document that failed.
    shared = yaml.nodes.SequenceNode(SEQ, [scalar()])

    def represent(dumper, value):
        return yaml.nodes.SequenceNode(SEQ, [shared, shared, scalar(None)])

    documents = [[Holder()], [Holder()], ["after"]]
    assert_same_session(
        dump_session(ryaml.RSafeDumper, documents, represent),
        dump_session(yaml.CSafeDumper, documents, represent),
    )


@needs_libyaml
@pytest.mark.parametrize("documents", [
    [[Holder()], [Holder()], ["after"]],
    [Holder(), Holder(), ["after"]],
], ids=["in-a-list", "as-the-document"])
@pytest.mark.parametrize("make_node", [
    lambda: yaml.nodes.SequenceNode("!s", 5),
    lambda: yaml.nodes.SequenceNode(5, 5),
    lambda: yaml.nodes.MappingNode(MAP, [(key(),)]),
    lambda: yaml.nodes.SequenceNode(SEQ, [scalar(), yaml.nodes.MappingNode(MAP, 5)]),
], ids=["items", "items-and-tag", "pair", "nested-items"])
def test_a_node_whose_items_failed_is_anchored_for_the_next_document(make_node, documents):
    # CEmitter's anchor pass, which reads the items, keeps the nodes it
    # reached before failing, so the next document does not read their
    # items but serializes them: it reads the tag, then the stuck emitter
    # fails.
    def session(base):
        node = make_node()
        return dump_session(base, documents, lambda d, x: node)

    assert_same_session(session(ryaml.RSafeDumper), session(yaml.CSafeDumper))


def test_an_alias_without_an_anchor_is_an_emittererror():
    # The second read finds the scalar twice, but the anchor pass saw it
    # once, so it has no anchor. LibYAML aborts on that, so compare with
    # PyYAML's Python dumper, which raises EmitterError.
    def represent(dumper, value):
        item = scalar()
        return yaml.nodes.SequenceNode(SEQ, Reiterable([item], [item, item]))

    documents = [["first", Holder()]]
    actual = dump_session(ryaml.RSafeDumper, documents, represent)
    expected = dump_session(yaml.SafeDumper, documents, represent)
    assert expected[0][1] is yaml.emitter.EmitterError
    assert_same_outcome(actual[0][1], expected[0][1])


@pytest.mark.parametrize("make_node", [
    lambda: scalar(STR, b"\xff"),
    lambda: scalar(b"!\xff"),
    lambda: yaml.nodes.SequenceNode(b"!\xff", []),
], ids=["value", "tag", "collection-tag"])
def test_invalid_utf8_bytes_raise_unicodedecodeerror(make_node):
    # CSafeDumper raises MemoryError.
    class Actual(ryaml.RSafeDumper):
        pass

    Actual.add_representer(Holder, lambda d, x: make_node())
    with pytest.raises(UnicodeDecodeError):
        yaml.dump([Holder()], Dumper=Actual)


@pytest.mark.parametrize("result", [None, "text", object()], ids=["none", "str", "object"])
def test_representer_returning_a_non_node_raises_typeerror(result):
    # CSafeDumper silently leaves out anything but its node classes.
    class Actual(ryaml.RSafeDumper):
        pass

    Actual.add_representer(Holder, lambda d, x: result)
    with pytest.raises(TypeError, match="expected a ScalarNode, SequenceNode or MappingNode"):
        yaml.dump([Holder()], Dumper=Actual)
    nested = lambda d, x: yaml.nodes.SequenceNode(SEQ, [result])
    Actual.add_representer(Holder, nested)
    with pytest.raises(TypeError, match="expected a ScalarNode"):
        yaml.dump([Holder()], Dumper=Actual)


def test_node_subclasses_are_written_like_the_pure_python_dumper():
    # CSafeDumper leaves them out; PyYAML's Python serializer uses isinstance.
    actual, expected = dumper_pair()
    for dumper in (actual, expected):
        dumper.add_representer(Holder, lambda d, x: SubclassedScalarNode(STR, "v"))
    value = ["first", Holder(), "last"]
    assert yaml.dump(value, Dumper=actual) == yaml.dump(value, Dumper=expected) == (
        "- first\n- v\n- last\n"
    )


@needs_libyaml
def test_bad_node_breaks_the_document_like_csafedumper():
    # The node is read after the document starts, so like CEmitter the
    # emitter is left expecting a node.
    def run(base):
        dumper_class = type("Dumper", (base,), {})
        dumper_class.add_representer(
            Holder, lambda d, x: yaml.nodes.SequenceNode(SEQ, [yaml.nodes.ScalarNode(None, "v")]),
        )
        stream = io.StringIO()
        dumper = dumper_class(stream)
        dumper.open()
        outcomes = []
        for call in (lambda: dumper.represent([Holder()]), lambda: dumper.represent([1]), dumper.close):
            try:
                call()
                outcomes.append(None)
            except Exception as error:
                outcomes.append(type(error))
        return outcomes, stream.getvalue()

    (actual, actual_output), (expected, expected_output) = (
        run(ryaml.RSafeDumper), run(yaml.CSafeDumper),
    )
    assert actual_output == expected_output
    for actual_outcome, expected_outcome in zip(actual, expected, strict=True):
        assert_same_outcome(actual_outcome, expected_outcome)


def test_represent_cannot_start_inside_a_representer():
    class Actual(ryaml.RSafeDumper):
        pass

    def nested(dumper, value):
        dumper.represent([1])
        return dumper.represent_scalar(STR, "x")

    Actual.add_representer(Holder, nested)
    stream = io.StringIO()
    dumper = Actual(stream)
    dumper.open()
    with pytest.raises(RuntimeError, match="cannot be used while"):
        dumper.represent([Holder()])
    dumper.represent(["after"])
    dumper.close()
    assert stream.getvalue() == "- after\n"


@needs_libyaml
def test_represent_data_and_serialize_outside_represent():
    def run(dumper_class):
        stream = io.StringIO()
        dumper = dumper_class(stream)
        dumper.open()
        node = dumper.represent_data({"a": [1, 2], "b": "text"})
        assert isinstance(node, yaml.nodes.MappingNode)
        dumper.serialize(node)
        dumper.close()
        return stream.getvalue()

    assert run(ryaml.RSafeDumper) == run(yaml.CSafeDumper)


def test_alias_key_and_represented_objects_are_pyyaml_compatible():
    class Peek:
        pass

    seen = {}

    def peek(dumper, value):
        seen["alias_key"] = dumper.alias_key
        items = [1]
        node = dumper.represent_data(items)
        seen["recorded"] = id(items) in dumper.represented_objects
        seen["same_node"] = dumper.represented_objects[id(items)] is node
        # Representing the item 1 (never aliased) leaves alias_key None.
        seen["alias_key_after"] = dumper.alias_key
        return node

    actual, expected = dumper_pair()
    for dumper in (actual, expected):
        dumper.add_representer(Peek, peek)
        value = Peek()
        yaml.dump(value, Dumper=dumper)
        assert seen == {
            "alias_key": id(value), "recorded": True, "same_node": True, "alias_key_after": None,
        }, dumper
        seen.clear()


# Node graphs mixing well-formed and malformed node objects, without the
# intentional differences from CSafeDumper (objects that are not nodes, node
# subclasses and bytes that are not UTF-8). Each draw is a function building a
# fresh graph, as each dumper needs its own (iterators are used up); `pool`
# holds the nodes built so far, so later ones can share them.
safe_text = st.text(st.characters(exclude_categories=["Cs"]), max_size=6)
node_tags = st.one_of(
    st.sampled_from([STR, INT, SEQ, MAP, "!x", "", None, 5, "!a\0b"]),
    st.sampled_from([STR, "!x", "tag:yaml.org,2002:int"]).map(str.encode),
    st.just("!s").map(StrSubclass),
)
scalar_values = st.one_of(
    safe_text, safe_text.map(str.encode), st.sampled_from([5, StrSubclass("v")]),
)
scalar_styles = st.sampled_from([None, "'", '"', "|", ">", "", "x", b"'"])
flow_styles = st.sampled_from([None, True, False, 0, "yes"])


def hold(how, items):
    """`items` as a collection node's value."""
    return {
        "list": lambda: list(items),
        "tuple": lambda: tuple(items),
        "iterator": lambda: iter(items),
        "shrinks": lambda: Reiterable(list(items), items[:-1]),
        "not-iterable": lambda: 5,
    }[how]()


def pair(how, key, value):
    return {
        "tuple": lambda: (key, value),
        "list": lambda: [key, value],
        "iterator": lambda: iter([key, value]),
        "short": lambda: (key,),
        "long": lambda: (key, value, value),
    }[how]()


def kept(pool, node):
    pool.append(node)
    return node


holds = st.sampled_from(["list"] * 6 + ["tuple", "iterator", "shrinks", "not-iterable"])
pairs = st.sampled_from(["tuple"] * 6 + ["list", "iterator", "short", "long"])
scalar_builders = st.builds(
    lambda tag, value, style: lambda pool: kept(pool, yaml.nodes.ScalarNode(tag, value, style=style)),
    node_tags, scalar_values, scalar_styles,
)
shared_builders = st.integers(0, 20).map(
    lambda index: lambda pool: pool[index % len(pool)] if pool else scalar(),
)


def sequence_builder(tag, flow_style, how, children):
    def build(pool):
        items = [child(pool) for child in children]
        return kept(pool, yaml.nodes.SequenceNode(tag, hold(how, items), flow_style=flow_style))
    return build


def mapping_builder(tag, flow_style, how, children):
    def build(pool):
        items = [pair(kind, key(pool), value(pool)) for kind, key, value in children]
        return kept(pool, yaml.nodes.MappingNode(tag, hold(how, items), flow_style=flow_style))
    return build


node_builders = st.recursive(
    st.one_of(scalar_builders, shared_builders),
    lambda children: st.one_of(
        st.builds(sequence_builder, node_tags, flow_styles, holds, st.lists(children, max_size=4)),
        st.builds(
            mapping_builder, node_tags, flow_styles, holds,
            st.lists(st.tuples(pairs, children, children), max_size=3),
        ),
    ),
    max_leaves=10,
)


@needs_libyaml
@given(
    node_builders,
    st.booleans(),
    st.sampled_from([{}, {"canonical": True}, {"default_flow_style": True, "explicit_start": True}]),
)
@settings(max_examples=500, deadline=None)
# The builders' reprs are long; failures are easier to read from the outcomes.
@pytest.mark.filterwarnings("ignore::hypothesis.errors.HypothesisWarning")
def test_node_object_graphs_match_csafedumper(build, cycle, options):
    def session(base):
        pool = []
        root = build(pool)
        if cycle and isinstance(root, yaml.nodes.SequenceNode) and isinstance(root.value, list):
            root.value.append(root)
        # Representing it twice in a document makes it an alias, and the
        # second document shows what a failure left behind.
        documents = [["first", Holder(), "last", Holder()], Holder(), [Holder()], ["after"]]
        return dump_session(base, documents, lambda d, x: root, **options)

    assert_same_session(session(ryaml.RSafeDumper), session(yaml.CSafeDumper))
