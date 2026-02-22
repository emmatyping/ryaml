import math
from pathlib import Path
from typing import Any
from collections.abc import Iterable
from dataclasses import dataclass

# https://github.com/yaml/yaml-test-suite/releases/tag/data-2022-01-17
YAML_TEST_SUITE = Path(__file__).resolve().parent / "yaml-test-suite"

ALL_YAMLS = 402

KNOWN_BAD = [
    "2JQS",
    "2LFX",
    "2SXE",
    "4ABK",
    "4MUZ/00",
    "4MUZ/01",
    "4MUZ/02",
    "58MP",
    "5MUD",
    "5T43",
    "652Z",
    "6BCT",
    "6CA3",
    "6HB6",
    "6LVF",
    "6M2F",
    "7Z25",
    "8G76",
    "8XYN",
    "96NN/00",
    "96NN/01",
    "98YD",
    "9C9N",
    "9HCY",
    "9JBA",
    "9SA2",
    "A2M4",
    "BEC7",
    "CFD4",
    "CVW2",
    "DBG4",
    "DC7X",
    "DK3J",
    "DK95",
    "DK95/00",
    "DK95/01",
    "DK95/03",
    "DK95/04",
    "EB22",
    "FP8R",
    "FRK4",
    "G5U8",
    "HM87/00",
    "HM87/01",
    "HS5T",
    "HWV9",
    "J3BT",
    "JEF9/02",
    "JR7V",
    "K3WX",
    "K54U",
    "L24T/01",
    "M2N8",
    "M7A3",
    "MUS6/05",
    "MUS6/06",
    "NB6Z",
    "NHX8",
    "NJ66",
    "NKF9",
    "Q5MG",
    "QB6E",
    "QT73",
    "R4YG",
    "RHX7",
    "S3PD",
    "S4JQ",
    "S98Z",
    "SM9W",
    "SU5Z",
    "U99R",
    "UKK6",
    "UT92",
    "UV7Q",
    "VJP3/01",
    "W4TN",
    "W5VH",
    "WZ62",
    "X4QW",
    "Y2GN",
    "Y79Y/001",
    "Y79Y/010",
    "YJV2"
]

TIME_PARSE_TEST = ["U9NS"]


@dataclass(slots=True, frozen=True)
class YamlTestSuite:
    id: str
    dir: Path
    in_yaml: Path
    out_yaml: Path | None
    in_json: Path | None
    is_error: bool


def iter_yaml_test_suite(root: Path) -> Iterable[YamlTestSuite]:
    root = root.resolve()

    for in_yaml in root.rglob("in.yaml"):
        dir_ = in_yaml.parent

        in_json = dir_ / "in.json"
        out_yaml = dir_ / "out.yaml"
        err = (dir_ / "error").exists()

        rel = dir_.relative_to(root).as_posix()

        yield YamlTestSuite(
            id=rel,
            dir=dir_,
            in_yaml=in_yaml,
            out_yaml=out_yaml if out_yaml.exists() else None,
            in_json=in_json if in_json.exists() else None,
            is_error=err,
        )


def split_cases(cases: Iterable[YamlTestSuite]) -> tuple:
    valid = []
    invalid = []
    skipped = []

    for ts in cases:
        if ts.in_json is None or (ts.id in KNOWN_BAD + TIME_PARSE_TEST):
            skipped.append(ts)
        elif ts.is_error:
            invalid.append(ts)
        else:
            valid.append(ts)

    return valid, invalid, skipped


YAML_FILES = list(iter_yaml_test_suite(YAML_TEST_SUITE))
VALID_YAMLS, INVALID_YAMLS, SKIPPED_YAMLS = split_cases(YAML_FILES)

assert (
    len(YAML_FILES)
    == len(VALID_YAMLS) + len(INVALID_YAMLS) + len(SKIPPED_YAMLS)
    == ALL_YAMLS
)


def _is_nan(obj: Any) -> Any | dict[Any, Any] | list[Any]:
    if isinstance(obj, dict):
        return {k: _is_nan(v) for k, v in obj.items()}
    if isinstance(obj, list):
        return [_is_nan(v) for v in obj]
    if isinstance(obj, float) and math.isnan(obj):
        return "ryaml_tests_nan"
    return obj
