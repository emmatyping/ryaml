"""Test cases to prevent regressions in compatibility with pyyaml"""

import textwrap
import ryaml


def test_nonspecific_tag():
    assert ryaml.loads("x: ! 4444") == {"x": 4444}


def test_long_int():
    assert ryaml.loads(
        "x: 99999999999999999999999999999999999999999999999999999999"
    ) == {"x": 99999999999999999999999999999999999999999999999999999999}


def test_set_type():
    assert (
        ryaml.loads(
            textwrap.dedent("""
                # sets are represented as a
                # mapping where each key is
                # associated with the empty string
                --- !!set
                ? Mark McGwire
                ? Sammy Sosa
                ? Ken Griff
                """)
        )
        == set(["Mark McGwire", "Ken Griff", "Sammy Sosa"])
    )
