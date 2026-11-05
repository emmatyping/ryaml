"""Compatibilty layer with pyyaml to load YAML documents with Rust code."""

from typing import Protocol, TypeAlias, TypeVar

from ryaml._ryaml import _RSafeLoader, _RSafeDumper

from yaml import BaseLoader
from yaml.constructor import BaseConstructor
from yaml.resolver import BaseResolver

__all__ = ["RSafeLoader", "RSafeDumper"]

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
        BaseConstructor.__init__(self)
        BaseResolver.__init__(self)

class RSafeDumper(_RSafeDumper):
    """pyyaml-compatible safe YAML dumper backed by Rust."""
    pass
