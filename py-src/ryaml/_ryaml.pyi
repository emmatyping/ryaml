# pyright: strict
from typing import Any

class InvalidYamlError(ValueError): ...

def loads(s: str) -> Any: ...
def loads_all(s: str) -> list[Any]: ...
def dumps(obj: Any) -> str: ...

class _RSafeLoader:
    # Note that this class only takes str | bytes because we want to do all I/O
    # at the Python layer
    def __init__(self, stream: str | bytes) -> None: ...

class _RSafeDumper:
    def __init__(
        self,
        stream: str | bytes,
        default_style: str | None = None,
        default_flow_style: bool = False,
        canonical: bool | None = None,
        indent: int | None = None,
        width: int | None = None,
        allow_unicode: bool | None = None,
        line_break: str | None = None,
        encoding: str | None = None,
        explicit_start: bool | None = None,
        explicit_end: bool | None = None,
        version: tuple[int, int] | None = None,
        tags: dict[str, str] | None = None,
        sort_keys: bool = True,
    ) -> None: ...
