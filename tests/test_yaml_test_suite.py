import pytest
import ryaml

pytest.importorskip("yaml")

from helpers import (
    VALID_YAMLS, INVALID_YAMLS, YamlTestSuite, _is_nan, load_reference,
    requires_csafeloader,
)


@requires_csafeloader
@pytest.mark.parametrize("ts", VALID_YAMLS, ids=lambda ts: ts.id)
def test_valid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    source = ts.in_yaml.read_text("utf-8")
    actual = ryaml.loads_all(source)
    if source == "":
        # The native API's existing empty-stream return convention is None.
        assert actual is None
        actual = []
    # JSON fixtures cannot express YAML 1.1 dates, binary data, sets or tuples.
    expected = load_reference(source)
    assert _is_nan(actual) == _is_nan(expected), (
        f"YAML test suite case {ts.id}: {actual!r} != {expected!r}"
    )


@pytest.mark.parametrize("ts", INVALID_YAMLS, ids=lambda ts: ts.id)
def test_invalid_yamls_from_test_suite(ts: YamlTestSuite) -> None:
    with pytest.raises(ryaml.InvalidYamlError):
        ryaml.loads_all(ts.in_yaml.read_text("utf-8"))
