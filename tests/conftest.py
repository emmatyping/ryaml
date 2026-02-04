import pytest
import io

@pytest.fixture
def yaml_file(tmp_path):
    with open(tmp_path / 'testfile.yaml', 'w+', encoding='utf8') as y:
        assert isinstance(y, io.TextIOBase)
        yield y
