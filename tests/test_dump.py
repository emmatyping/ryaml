import ryaml

def test_dump_none(yaml_file):
    ryaml.dump(yaml_file, None)
    yaml_file.seek(0)
    assert yaml_file.read() == "null\n"

def test_dump_key(yaml_file):
    ryaml.dump(yaml_file, { 'key': None })
    yaml_file.seek(0)
    assert yaml_file.read() == 'key: null\n'

def test_dump_key_value(yaml_file):
    ryaml.dump(yaml_file, { 'key': 4 })
    yaml_file.seek(0)
    assert yaml_file.read() == 'key: 4\n'

def test_dump_key_sequence(yaml_file):
    ryaml.dump(yaml_file, { 'key': [4, 5] })
    yaml_file.seek(0)
    assert yaml_file.read() == 'key:\n- 4\n- 5\n'
