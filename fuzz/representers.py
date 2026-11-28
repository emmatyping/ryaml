#!/usr/bin/env python3
"""Differential fuzzing for PyYAML-compatible custom representers."""

from collections import OrderedDict
import copy
from dataclasses import dataclass
from datetime import timedelta
import io
from pathlib import PurePath, PurePosixPath, PureWindowsPath
import sys

import atheris

with atheris.instrument_imports():
    import ryaml
    import yaml


STR = "tag:yaml.org,2002:str"
INT = "tag:yaml.org,2002:int"
SEQ = "tag:yaml.org,2002:seq"
MAP = "tag:yaml.org,2002:map"
CUSTOM = "tag:example.com,2026:custom"
SCALAR_TAGS = [STR, INT, "tag:yaml.org,2002:bool", "!scalar", CUSTOM, "!"]
SCALAR_STYLES = [None, "'", '"', "|", ">"]
FLOW_STYLES = [None, False, True]
SPECIAL_TEXT = [
    "", " ", "null", "on", "0", "0x10", "1:20", "1e+5", "---", "...",
    "a: b", "# comment", "a\n\nb\n", "a\r\nb", "\t", "\x00", "\x85",
    "\u2028", "\ufeff", "café", "😀", "\ud800",
]


@dataclass(eq=False)
class Scalar:
    value: str
    tag: str
    style: str | None


class ExactScalar(Scalar):
    pass


class OtherBase:
    pass


class DerivedScalar(Scalar, OtherBase):
    pass


@dataclass(eq=False)
class Sequence:
    tag: str
    style: bool | None
    items: list


@dataclass(eq=False)
class Mapping:
    tag: str
    style: bool | None
    items: dict | list


@dataclass(eq=False)
class RawNode:
    node: yaml.nodes.Node


@dataclass(eq=False)
class Delegate:
    value: object


@dataclass(eq=False)
class Clobber:
    """Represents its value, then a scalar recorded under the moved alias_key."""
    value: object


@dataclass(eq=False)
class Retag:
    """Changes the tag of the node represent_data returns."""
    value: object


class Unknown:
    pass


class CallbackError(Exception):
    pass


class Raises:
    pass


def represent_scalar(dumper, data):
    return dumper.represent_scalar(data.tag, data.value, style=data.style)


def represent_exact(dumper, data):
    return dumper.represent_scalar(STR, f"exact:{data.value}", style=data.style)


def fail(dumper, data):
    raise CallbackError("custom callback failed")


def clobber(dumper, data):
    dumper.represent_data(data.value)
    return dumper.represent_scalar("!clobber", "v")


def retag(dumper, data):
    node = dumper.represent_data(data.value)
    node.tag = "!retagged"
    return node


def register(exact, multi):
    multi(OtherBase, fail)
    multi(Scalar, represent_scalar)
    exact(ExactScalar, represent_exact)
    exact(Sequence, lambda d, x: d.represent_sequence(x.tag, x.items, flow_style=x.style))
    exact(Mapping, lambda d, x: d.represent_mapping(x.tag, x.items, flow_style=x.style))
    exact(RawNode, lambda d, x: x.node)
    exact(Delegate, lambda d, x: d.represent_data(x.value))
    exact(Clobber, clobber)
    exact(Retag, retag)
    exact(OrderedDict, lambda d, x: d.represent_dict(x.items()))
    exact(Raises, fail)
    multi(PurePath, lambda d, x: d.represent_scalar(STR, x.as_posix()))
    exact(timedelta, lambda d, x: d.represent_scalar(INT, str(int(x.total_seconds()))))


class Reference(yaml.CSafeDumper):
    pass


class Actual(ryaml.RSafeDumper):
    pass


register(Reference.add_representer, Reference.add_multi_representer)
register(
    lambda typ, callback: ryaml.add_representer(typ, callback, Dumper=Actual),
    lambda typ, callback: ryaml.add_multi_representer(typ, callback, Dumper=Actual),
)


class ReferenceWithFallback(Reference):
    pass


class ActualWithFallback(Actual):
    pass


for dumper in (ReferenceWithFallback, ActualWithFallback):
    dumper.add_multi_representer(None, lambda d, x: d.represent_scalar(STR, type(x).__name__))
    dumper.add_representer(int, lambda d, x: d.represent_scalar("!integer", str(x)))
    dumper.add_representer(
        str, lambda d, x: d.represent_scalar(STR, x, style="|" if "\n" in x else None),
    )


@atheris.instrument_func
def text(provider):
    if provider.ConsumeBool():
        return provider.PickValueInList(SPECIAL_TEXT)
    return provider.ConsumeUnicodeNoSurrogates(provider.ConsumeIntInRange(0, 80))


@atheris.instrument_func
def tag(provider, tags):
    """A node tag: mostly str, sometimes bytes or None, as CEmitter accepts."""
    choice = provider.ConsumeIntInRange(0, 9)
    if choice == 0:
        return None
    if choice == 1:
        return provider.PickValueInList(tags).encode()
    return provider.PickValueInList(tags)


@atheris.instrument_func
def scalar_value(provider):
    value = text(provider)
    if provider.ConsumeIntInRange(0, 5) == 0:
        # Valid UTF-8 (CEmitter raises MemoryError for invalid UTF-8).
        return value.encode("utf-8", "replace")
    return value


@atheris.instrument_func
def raw_node(provider):
    pick = provider.PickValueInList
    leaf = yaml.nodes.ScalarNode(
        tag(provider, SCALAR_TAGS), scalar_value(provider), style=pick(SCALAR_STYLES),
    )
    kind = provider.ConsumeIntInRange(0, 2)
    if kind == 0:
        return leaf
    style = pick(FLOW_STYLES)
    if kind == 1:
        items = [leaf, leaf]
        node = yaml.nodes.SequenceNode(tag(provider, [SEQ, "!seq"]), items, flow_style=style)
        if provider.ConsumeBool():
            node.value.append(node)
        return node
    pair = (leaf, leaf) if provider.ConsumeBool() else [leaf, leaf]
    node = yaml.nodes.MappingNode(tag(provider, [MAP, "!map"]), [pair], flow_style=style)
    if provider.ConsumeBool():
        node.value.append((leaf, node))
    return node


@atheris.instrument_func
def documents(provider):
    pick = provider.PickValueInList
    objects = []
    containers = []
    for _ in range(provider.ConsumeIntInRange(1, 16)):
        kind = provider.ConsumeIntInRange(0, 13)
        if kind <= 2:
            cls = (Scalar, ExactScalar, DerivedScalar)[kind]
            value = cls(text(provider), pick(SCALAR_TAGS), pick(SCALAR_STYLES))
        elif kind == 3:
            value = Sequence(pick([SEQ, "!sequence", CUSTOM]), pick(FLOW_STYLES), [])
            containers.append(value)
        elif kind == 4:
            items = {} if provider.ConsumeBool() else []
            value = Mapping(pick([MAP, "!mapping", CUSTOM]), pick(FLOW_STYLES), items)
            containers.append(value)
        elif kind == 5:
            value = RawNode(raw_node(provider))
        elif kind == 6:
            value = pick([PurePosixPath, PureWindowsPath])(text(provider))
        elif kind == 7:
            value = timedelta(microseconds=provider.ConsumeInt(8))
        elif kind == 8:
            value = Unknown()
        elif kind == 9:
            value = Raises()
        elif kind == 10:
            value = Delegate(pick(objects)) if objects else Delegate(text(provider))
        elif kind == 11:
            value = Clobber(pick(objects) if objects else [text(provider)])
        elif kind == 12:
            value = Retag(pick(objects) if objects else {text(provider): 1})
        else:
            value = OrderedDict()
            containers.append(value)
        objects.append(value)

    values = objects + [None, True, text(provider), provider.ConsumeInt(8)]
    for container in containers:
        for _ in range(provider.ConsumeIntInRange(0, 5)):
            if isinstance(container, Sequence):
                container.items.append(pick(values))
            elif isinstance(container, OrderedDict):
                key = pick(values)
                try:
                    container[key] = pick(values)
                except TypeError:  # unhashable
                    pass
            else:
                key = pick(values)
                item = pick(values)
                if isinstance(container.items, dict):
                    try:
                        container.items[key] = item
                    except TypeError:  # unhashable
                        pass
                else:
                    container.items.append((key, item))

    docs = [objects[0]]
    for _ in range(provider.ConsumeIntInRange(0, 2)):
        if provider.ConsumeBool():
            docs.append(pick(objects))
        else:
            shared = pick(objects)
            docs.append({"z": shared, "a": [pick(objects), shared]})
    return docs


def options(provider):
    pick = provider.PickValueInList
    return {
        "sort_keys": provider.ConsumeBool(),
        "default_style": pick(SCALAR_STYLES),
        "default_flow_style": pick(FLOW_STYLES),
        "canonical": provider.ConsumeBool(),
        "allow_unicode": provider.ConsumeBool(),
        "explicit_start": provider.ConsumeBool(),
        "explicit_end": provider.ConsumeBool(),
        "encoding": pick([None, None, "utf-8", "utf-16-le", "utf-16-be"]),
        "indent": pick([None, 2, 4, 9]),
        "width": pick([None, 20, 40, 80]),
        "line_break": pick([None, "\r", "\r\n"]),
        "tags": pick([None, {"!e!": "tag:example.com,2026:"}]),
    }


def outcome(operation):
    try:
        return operation(), None
    except Exception as error:
        return None, type(error)


def check(operation, expected, actual):
    expected_value, expected_error = expected
    actual_value, actual_error = actual
    if expected_error is None:
        match = actual_error is None and type(actual_value) is type(expected_value)
        match = match and actual_value == expected_value
    else:
        match = actual_error is not None and issubclass(actual_error, expected_error)
    assert match, f"{operation}\nCSafeDumper: {expected!r}\nryaml: {actual!r}"


def fresh(docs):
    return copy.deepcopy(docs)


def protocol(dumper_class, docs, kwargs, calls):
    stream = io.BytesIO() if kwargs["encoding"] else io.StringIO()
    dumper = dumper_class(stream, **kwargs)
    dumper.open()
    results = [outcome(lambda: dumper.represent(docs[index])) for index in calls]
    results.append(outcome(dumper.close))
    return results, stream.getvalue()


@atheris.instrument_func
def fuzz_one_input(data):
    provider = atheris.FuzzedDataProvider(data)
    actual, reference = ((ActualWithFallback, ReferenceWithFallback) if provider.ConsumeBool()
                         else (Actual, Reference))
    mode = provider.ConsumeIntInRange(0, 3)
    kwargs = options(provider) if mode != 2 else {"sort_keys": False}
    if mode == 3:
        # Drive the serializer directly with a possibly cyclic PyYAML node.
        for option in ("sort_keys", "default_style", "default_flow_style"):
            del kwargs[option]
        node = raw_node(provider)
        check(f"serialize with {kwargs!r}",
              outcome(lambda: yaml.serialize(node, Dumper=reference, **kwargs)),
              outcome(lambda: yaml.serialize(node, Dumper=actual, **kwargs)))
        return

    generated = documents(provider)
    # Representers can change node objects in the data (`Retag`), so each
    # dump gets its own copy, with the same objects shared within it.
    docs = copy.deepcopy(generated)
    if mode == 0:
        check(f"dump_all with {kwargs!r}",
              outcome(lambda: yaml.dump_all(docs, Dumper=reference, **kwargs)),
              outcome(lambda: yaml.dump_all(fresh(generated), Dumper=actual, **kwargs)))
    elif mode == 1:
        calls = [provider.ConsumeIntInRange(0, len(docs) - 1) for _ in range(3)]
        expected, expected_output = protocol(reference, docs, kwargs, calls)
        observed, actual_output = protocol(actual, fresh(generated), kwargs, calls)
        for index, (lhs, rhs) in enumerate(zip(expected, observed, strict=True)):
            check(f"protocol call {index} with {kwargs!r}", lhs, rhs)
        check(f"protocol output with {kwargs!r}", (expected_output, None), (actual_output, None))
    else:
        expected = outcome(lambda: yaml.dump(docs[0], Dumper=reference, **kwargs))
        check("ryaml.dumps", expected,
              outcome(lambda: ryaml.dumps(fresh(generated)[0], Dumper=actual)))
        stream = io.BytesIO() if provider.ConsumeBool() else io.StringIO()

        def dump_to_stream():
            ryaml.dump(stream, fresh(generated)[0], Dumper=actual)
            return stream.getvalue()

        if isinstance(stream, io.BytesIO) and expected[1] is None:
            expected = expected[0].encode("utf-8"), None
        check("ryaml.dump", expected, outcome(dump_to_stream))


if __name__ == "__main__":
    atheris.Setup(sys.argv, fuzz_one_input, enable_python_coverage=True)
    atheris.Fuzz()
