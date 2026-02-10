import textwrap
import ryaml

def test_load_all_empty(yaml_file):
    yaml_file.write('')
    yaml_file.seek(0)
    assert ryaml.load_all(yaml_file) is None

def test_load_all_keys(yaml_file):
    yaml_file.write(textwrap.dedent('''
    ---
    key: ~
    ...
    ---
    key2: ~
    '''))
    yaml_file.seek(0)
    assert ryaml.load_all(yaml_file) == [{ 'key': None }, { 'key2': None }]
