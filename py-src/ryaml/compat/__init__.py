"""PyYAML-compatible loaders and dumpers backed by Rust."""

from types import MappingProxyType
from typing import TYPE_CHECKING, Any, Callable, Protocol, TypeAlias, TypeVar

from ryaml._ryaml import _RSafeLoader, _RSafeDumper
from ryaml.error import RepresenterError

try:
    from yaml import BaseLoader
    from yaml.constructor import BaseConstructor
    from yaml.representer import SafeRepresenter
    from yaml.resolver import BaseResolver
except ModuleNotFoundError as exc:
    if exc.name != "yaml":
        raise
    BaseLoader = object

if TYPE_CHECKING:
    from yaml.nodes import Node

__all__ = ["RSafeLoader", "RSafeDumper"]

Representer: TypeAlias = Callable[["RSafeDumper", Any], "Node"]

# SupportsRead Protocol from the definition in typeshed
_T_co = TypeVar("_T_co", covariant=True)

class SupportsRead(Protocol[_T_co]):
    def read(self, length: int = ..., /) -> _T_co: ...

# From the pyyaml type defintions in typeshed
Readable: TypeAlias = SupportsRead[str | bytes]

class RSafeLoader(_RSafeLoader, BaseLoader):
    def __new__(cls, stream: str | bytes | Readable) -> "RSafeLoader":
        try:
            data = stream.read() # type: ignore
        except AttributeError:
            data = stream
        return super().__new__(cls, data) # type: ignore

    def __init__(self, stream: str | bytes | Readable) -> None:
        # The Rust loader reads and decodes the stream itself. BaseLoader's
        # Reader/Scanner would read it again in Python and raise PyYAML's own
        # errors first (eagerly, for invalid bytes or characters), so only set
        # up the parts of BaseLoader that hold no input.
        self.stream = stream
        self.name = getattr(stream, "name", "<file>")
        if BaseLoader is not object:
            BaseConstructor.__init__(self)
            BaseResolver.__init__(self)

if BaseLoader is object:
    class _SafeRepresenter:
        yaml_representers = MappingProxyType({})
        yaml_multi_representers = MappingProxyType({})

        @classmethod
        def add_representer(cls, data_type: type | None, representer: Representer) -> None:
            raise ModuleNotFoundError("Custom representers require PyYAML", name="yaml")

        add_multi_representer = add_representer
else:
    def _represent_undefined(dumper: "RSafeDumper", data: Any) -> "Node":
        raise RepresenterError(f"cannot represent an object: {data!r}")

    class _SafeRepresenter(SafeRepresenter):
        # PyYAML copies each registry when a subclass first registers a callback.
        # Keep the defaults independent of PyYAML's own registration API.
        yaml_representers = MappingProxyType({
            **SafeRepresenter.yaml_representers,
            None: _represent_undefined,
        })
        yaml_multi_representers = MappingProxyType(SafeRepresenter.yaml_multi_representers.copy())


class RSafeDumper(_RSafeDumper, _SafeRepresenter):
    """PyYAML-compatible safe YAML dumper backed by Rust.

    The built-in types SafeRepresenter handles are represented in Rust unless
    a representer registered on the class replaces SafeRepresenter's. Other
    types are looked up like PyYAML: the exact type in ``yaml_representers``,
    then the type's MRO in ``yaml_multi_representers``, then ``None`` in each.
    Representers get this dumper, whose ``represent_data``, ``alias_key`` and
    ``represented_objects`` are backed by Rust, so PyYAML's own
    ``represent_scalar``, ``represent_sequence`` and ``represent_mapping``
    work with it.
    """

    def __init__(
        self, stream, default_style=None, default_flow_style=False, canonical=None,
        indent=None, width=None, allow_unicode=None, line_break=None, encoding=None,
        explicit_start=None, explicit_end=None, version=None, tags=None, sort_keys=True,
    ) -> None:
        # The options PyYAML's representer methods read, as given.
        self.default_style = default_style
        self.default_flow_style = default_flow_style
        self.sort_keys = sort_keys

    @classmethod
    def _has_custom_representers(cls) -> bool:
        return (
            cls.yaml_representers is not _SafeRepresenter.yaml_representers
            or cls.yaml_multi_representers is not _SafeRepresenter.yaml_multi_representers
        )
