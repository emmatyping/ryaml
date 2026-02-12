import json
from pathlib import Path

import ryaml

import pytest
import yaml

from ryaml.compat import RSafeLoader

from helpers import VALID_YAMLS, INVALID_YAMLS, normalize_yaml

def test_loads_empty():
    assert yaml.load('', Loader=RSafeLoader) is None

def test_loads_key():
    assert yaml.load('''
    key:

    ''', Loader=RSafeLoader) == { 'key': None }

def test_loads_key_value():
    assert yaml.load('''
    key:
        4

    ''', Loader=RSafeLoader) == { 'key': 4 }

def test_loads_key_sequence():
    assert yaml.load('''
    key:
        - 4
        - 5

    ''', Loader=RSafeLoader) == { 'key': [4, 5] }

@pytest.mark.parametrize("input", VALID_YAMLS, ids=lambda val: f"{val.name[:-5]}")
def test_valid_yamls_from_test_suite_pyyaml(input: Path) -> None:
    load_from_str = yaml.load(input.read_text(encoding="utf-8"), Loader=yaml.CSafeLoader)

    docs = [load_from_str] if isinstance(load_from_str, dict) else load_from_str

    for doc in docs:
        parsed_yaml = list(yaml.load_all(normalize_yaml(doc), Loader=RSafeLoader))
        if len(parsed_yaml) == 1:
            parsed_yaml = parsed_yaml[0]


        get_json_key = doc.get("json")

        if get_json_key is None:
            assert parsed_yaml is not None
            continue

        if get_json_key == "":  # noqa: PLC1901
            get_json_key = None
            continue

        try:
            parsed_json = json.loads(get_json_key)
        except json.decoder.JSONDecodeError:
            json_decoder = json.JSONDecoder()
            parsed_json = []
            pos = 0
            while pos < len(get_json_key):
                obj, pos = json_decoder.raw_decode(get_json_key, pos)
                parsed_json.append(obj)
                while pos < len(get_json_key) and get_json_key[pos] in " \t\n\r":
                    pos += 1

            if len(parsed_json) == 1:
                parsed_json = parsed_json[0]

        assert parsed_yaml == parsed_json


@pytest.mark.parametrize("input", INVALID_YAMLS, ids=lambda val: f"{val.name[:-5]}")
def test_invalid_yamls_from_test_suite_pyyaml(input: Path) -> None:
    docs = list(yaml.load_all(input.read_text(encoding="utf-8"), Loader=RSafeLoader))
    if len(docs) == 1:
        docs = docs[0]
    if isinstance(docs, dict):
        docs = [docs]
    doc = next((d for d in docs if d.get("fail") is True), None)
    assert doc is not None, "No document!"
    with pytest.raises(ryaml.InvalidYamlError):
        list(yaml.load_all(normalize_yaml(doc), Loader=RSafeLoader))
