import math
from pathlib import Path
from typing import Any

import yaml

# https://github.com/yaml/yaml-test-suite
YAML_TEST_SUITE = Path(__file__).resolve().parent / "yaml-test-suite"
YAML_FILES = list(YAML_TEST_SUITE.glob("*.yaml"))

ALL_YAMLS = 351

KNOWN_BAD = [
    "6M2F",
    "2JQS",
    "NHX8",
    "CFD4",
    "NKF9",
    "M2N8",
    "SM9W",
    "FRK4",
    "S3PD",
    "UKK6",
    "W5VH",
    "Y2GN",
    "8XYN",
    "2SXE",
    "7Z25",
    "K3WX",
    "5MUD",
    "VJP3",
    "4MUZ",
    "4MUZ",
    "4MUZ",
    "9SA2",
    "NJ66",
    "5T43",
    "58MP",
    "HM87",
    "DBG4",
    "QT73",
    "HWV9",
    "M7A3",
    "A2M4",
    "6BCT",
    "Q5MG",
    "6CA3",
    "Y79Y",
    "DK95",
    "DK95",
    "DK95",
    "652Z",
    "HM87",
    "UT92",
    "W4TN",
    "L24T",
    "JEF9",
    "FP8R",
    "DK3J",
    "MUS6",
    "MUS6",
    "6LVF",
    "2LFX",
    "BEC7",
    "96NN",
    "96NN",
    "R4YG",
    "Y79Y",
    "4ABK",
    "MUS6",
    "UV7Q",
    "NB6Z",
    "HS5T",
    "J3BT",
    "6HB6",
    "K54U",
    "Y79Y",
    "DK95",
    "DK95",
    "DC7X",
    "JR7V",
    "WZ62",
    "S98Z",
    "SU5Z",
    "CVW2",
    "9JBA",
    "YJV2",
    "G5U8",
    "MUS6",
    "EB22",
    "9HCY",
    "RHX7",
    "DK95",
    "9C9N",
    "QB6E",
    "X4QW",
    "MUS6",
    "Y79Y",
    "U99R",
]

TIME_PARSE_TEST = ["U9NS"]

def _get_yamls():
    valid = []
    invalid = []
    skipped = []

    for yaml_file in YAML_FILES:
        docs = yaml.load(yaml_file.read_text(encoding="utf-8"), Loader=yaml.CSafeLoader)
        docs = [docs] if isinstance(docs, dict) else docs

        has_fail = any(doc.get("fail", False) for doc in docs)
        has_skip = any(doc.get("skip", False) for doc in docs)

        if has_skip or yaml_file.name[:-5] in KNOWN_BAD + TIME_PARSE_TEST:
            skipped.append(yaml_file)
        elif has_fail:
            invalid.append(yaml_file)
        else:
            valid.append(yaml_file)

    return valid, invalid, skipped

VALID_YAMLS, INVALID_YAMLS, SKIPPED_YAMLS = _get_yamls()
assert (
    len(YAML_FILES)
    == len(VALID_YAMLS) + len(INVALID_YAMLS) + len(SKIPPED_YAMLS)
    == ALL_YAMLS
)


def normalize_yaml(doc: dict) -> Any:
    return (
        doc.get("yaml", "")
        .replace("␣", " ")
        .replace("»", "\t")
        .replace("—", "")  # Tab line continuation ——»
        .replace("←", "\r")
        .replace("⇔", "\ufeff")  # BOM character
        .replace("↵", "")  # Trailing newline marker
        .replace("∎\n", "")
    )
