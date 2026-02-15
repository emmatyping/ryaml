"""Compatibilty layer with pyyaml to load YAML documents with Rust code."""

from typing import Protocol, TypeAlias, TypeVar

from ryaml._ryaml import _RSafeLoader, _RSafeDumper

from yaml import BaseLoader

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
        if isinstance(data, bytes):
            data = data.decode('utf8')
        return super().__new__(cls, data) # type: ignore

class RSafeDumper(_RSafeDumper):
    """pyyaml-compatible safe YAML dumper backed by Rust."""
    pass
