"""Compatibilty layer with pyyaml to load YAML documents with Rust code."""

from typing import Protocol, TypeAlias, TypeVar

from ryaml._ryaml import _RSafeLoader

__all__ = ["RSafeLoader"]

# SupportsRead Protocol from the definition in typeshed
_T_co = TypeVar("_T_co", covariant=True)

class SupportsRead(Protocol[_T_co]):
    def read(self, length: int = ..., /) -> _T_co: ...

# From the pyyaml type defintions in typeshed
Readable: TypeAlias = SupportsRead[str | bytes]

class RSafeLoader(_RSafeLoader):
    def __init__(self, stream: str | bytes | Readable) -> None:
        if hasattr(stream, "read"):
            data = stream.read() # type: ignore
        else:
            data = stream
        if isinstance(data, bytes):
            data = data.decode('utf8')
        super().__init__(data) # type: ignore
