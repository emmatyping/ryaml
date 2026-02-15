import ryaml

def test_load_empty(yaml_file):
    yaml_file.write('')
    yaml_file.seek(0)
    assert ryaml.load(yaml_file) is None

def test_load_key(yaml_file):
    yaml_file.write('key:')
    yaml_file.seek(0)
    assert ryaml.load(yaml_file) == { 'key': None }

def test_load_key_value(yaml_file):
    yaml_file.write('''
    key:
        4

    ''')
    yaml_file.seek(0)
    assert ryaml.load(yaml_file) == { 'key': 4 }

def test_load_key_sequence(yaml_file):
    yaml_file.write('''
    key:
        - 4
        - 5

    ''')
    yaml_file.seek(0)
    assert ryaml.load(yaml_file) == { 'key': [4, 5] }

def test_load_non_ascii(yaml_file):
    yaml_file.write('key: 你好')
    yaml_file.seek(0)
    assert ryaml.load(yaml_file) == { 'key': '你好' }
