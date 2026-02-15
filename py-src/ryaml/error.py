"""Exception classes for ryaml, compatible with both older ryaml and pyyaml.

All exceptions subclass InvalidYamlError for backwards compatibility.
When pyyaml is installed, they also subclass the corresponding pyyaml
exception so that ``except yaml.ScannerError`` (etc.) catches ryaml errors.
"""

from ._ryaml import InvalidYamlError

try:
    from yaml.error import YAMLError as _YAMLError, MarkedYAMLError as _MarkedYAMLError
    from yaml.scanner import ScannerError as _PyScannerError
    from yaml.parser import ParserError as _PyParserError
    from yaml.composer import ComposerError as _PyComposerError
    from yaml.constructor import ConstructorError as _PyConstructorError
    from yaml.emitter import EmitterError as _PyEmitterError
    from yaml.serializer import SerializerError as _PySerializerError
    from yaml.representer import RepresenterError as _PyRepresenterError
    from yaml.reader import ReaderError as _PyReaderError
    _HAS_YAML = True
except ImportError:
    _HAS_YAML = False


class _MarkedErrorMixin:
    """Provide MarkedYAMLError-compatible attributes for exceptions raised with a plain string."""

    def __init__(self, context=None, context_mark=None,
                 problem=None, problem_mark=None, note=None):
        # When raised from Rust with a single positional string, map it to `problem`.
        if isinstance(context, str) and context_mark is None and problem is None:
            problem = context
            context = None
        self.context = context
        self.context_mark = context_mark
        self.problem = problem
        self.problem_mark = problem_mark
        self.note = note
        parts = []
        if context is not None:
            parts.append(str(context))
        if problem is not None:
            parts.append(str(problem))
        super().__init__("\n".join(parts) if parts else "")


if _HAS_YAML:
    class ScannerError(_MarkedErrorMixin, InvalidYamlError, _PyScannerError):
        pass

    class ParserError(_MarkedErrorMixin, InvalidYamlError, _PyParserError):
        pass

    class ComposerError(_MarkedErrorMixin, InvalidYamlError, _PyComposerError):
        pass

    class ConstructorError(_MarkedErrorMixin, InvalidYamlError, _PyConstructorError):
        pass

    class EmitterError(InvalidYamlError, _PyEmitterError):
        pass

    class SerializerError(InvalidYamlError, _PySerializerError):
        pass

    class RepresenterError(InvalidYamlError, _PyRepresenterError):
        pass

    class ReaderError(InvalidYamlError, _PyReaderError):
        def __init__(self, *args, **kwargs):
            # Accept either a plain string (from Rust) or the pyyaml 5-arg form.
            if len(args) == 1 and isinstance(args[0], str) and not kwargs:
                InvalidYamlError.__init__(self, args[0])
                self.name = None
                self.position = None
                self.character = None
                self.encoding = None
                self.reason = args[0]
            else:
                _PyReaderError.__init__(self, *args, **kwargs)

else:
    class ScannerError(_MarkedErrorMixin, InvalidYamlError):
        pass

    class ParserError(_MarkedErrorMixin, InvalidYamlError):
        pass

    class ComposerError(_MarkedErrorMixin, InvalidYamlError):
        pass

    class ConstructorError(_MarkedErrorMixin, InvalidYamlError):
        pass

    class EmitterError(InvalidYamlError):
        pass

    class SerializerError(InvalidYamlError):
        pass

    class RepresenterError(InvalidYamlError):
        pass

    class ReaderError(InvalidYamlError):
        pass
