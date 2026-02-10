"""Compatibilty layer with pyyaml to load YAML documents with Rust code."""

from typing import Protocol, Self, TypeAlias, TypeVar

from ryaml._ryaml import _RSafeLoader

from yaml import BaseLoader

__all__ = ["RSafeLoader"]

# SupportsRead Protocol from the definition in typeshed
_T_co = TypeVar("_T_co", covariant=True)

class SupportsRead(Protocol[_T_co]):
    def read(self, length: int = ..., /) -> _T_co: ...

# From the pyyaml type defintions in typeshed
Readable: TypeAlias = SupportsRead[str | bytes]

class RSafeLoader(_RSafeLoader, BaseLoader):
    def __new__(cls, stream: str | bytes | Readable) -> Self:
        try:
            data = stream.read() # type: ignore
        except AttributeError:
            data = stream
        if isinstance(data, bytes):
            data = data.decode('utf8')
        return super().__new__(cls, data) # type: ignore
