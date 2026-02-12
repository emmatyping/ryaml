from pathlib import Path
import pytest

import ryaml
try:
    from yaml import CSafeLoader as SafeLoader, CSafeDumper as SafeDumper
except ImportError:
    from yaml import SafeLoader, SafeDumper

import yaml

from helpers import VALID_YAMLS, normalize_yaml

@pytest.mark.parametrize("input", VALID_YAMLS, ids=lambda val: f"{val.name[:-5]}")
def test_valid_yamls_dumps_from_test_suite_pyyaml(input: Path) -> None:
    if "2XXW" in str(input):
        pytest.skip("Test is unpredictable due to set ordering")
    load_from_str = yaml.load(input.read_text(encoding="utf-8"), Loader=SafeLoader)

    docs = [load_from_str] if isinstance(load_from_str, dict) else load_from_str

    for doc in docs:
        try:
            parsed_yaml = list(yaml.load_all(normalize_yaml(doc), Loader=SafeLoader))
        except Exception:
            pytest.skip("pyyaml cannot load document")
            return
        if len(parsed_yaml) == 1:
            parsed_yaml = parsed_yaml[0]

        dumps_rust = yaml.dump(parsed_yaml, Dumper=ryaml.RSafeDumper)
        try:
            dumps_c = yaml.dump(parsed_yaml, Dumper=SafeDumper)
        except Exception:
            pytest.skip("pyyaml fails this test")
            return
        assert dumps_rust == dumps_c
