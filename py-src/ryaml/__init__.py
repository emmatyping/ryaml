from ._ryaml import InvalidYamlError, loads, loads_all, dumps as _dumps
from .error import (
    ScannerError,
    ParserError,
    ComposerError,
    ConstructorError,
    EmitterError,
    SerializerError,
    RepresenterError,
    ReaderError,
)
from .compat import RSafeDumper, RSafeLoader, Representer

from typing import IO, AnyStr, Any
import io

__all__ = [
    "InvalidYamlError",
    "loads",
    "loads_all",
    "dumps",
    "add_representer",
    "add_multi_representer",
    "ScannerError",
    "ParserError",
    "ComposerError",
    "ConstructorError",
    "EmitterError",
    "SerializerError",
    "RepresenterError",
    "ReaderError",
    "RSafeDumper",
    "RSafeLoader",
]


def _read_file(fp: IO[AnyStr]) -> str | bytes:
    data = fp.read()
    if isinstance(data, str):
        return data
    # Decoded in Rust: a UTF-16 BOM selects UTF-16, otherwise UTF-8.
    return bytes(data)


def load(fp: IO[AnyStr]) -> Any:
    if not isinstance(fp, io.IOBase):
        raise TypeError("fp must be a file-like object")
    return loads(_read_file(fp))


def load_all(fp: IO[AnyStr]) -> list[Any]:
    if not isinstance(fp, io.IOBase):
        raise TypeError("fp must be a file-like object")
    return loads_all(_read_file(fp))


def add_representer(
    data_type: type | None, representer: Representer, Dumper: type[RSafeDumper] = RSafeDumper,
) -> None:
    """Register a representer for an exact type on the given dumper class."""
    Dumper.add_representer(data_type, representer)


def add_multi_representer(
    data_type: type | None, multi_representer: Representer,
    Dumper: type[RSafeDumper] = RSafeDumper,
) -> None:
    """Register a representer for a type and its subclasses on the given dumper class."""
    Dumper.add_multi_representer(data_type, multi_representer)


def dumps(obj: Any, *, Dumper: type[RSafeDumper] = RSafeDumper) -> str:
    if Dumper is RSafeDumper and not Dumper._has_custom_representers():
        return _dumps(obj)
    stream = io.StringIO()
    dumper = Dumper(stream, sort_keys=False)
    try:
        dumper.open()
        dumper.represent(obj)
        dumper.close()
    finally:
        dumper.dispose()
    return stream.getvalue()


def dump(fp: IO[AnyStr], obj: Any, *, Dumper: type[RSafeDumper] = RSafeDumper) -> None:
    yaml = dumps(obj, Dumper=Dumper)
    if isinstance(fp, io.TextIOBase):
        fp.write(yaml)  # type: ignore
    else:
        fp.write(yaml.encode("utf8"))  # type: ignore
