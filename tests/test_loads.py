import ryaml


def test_loads_empty():
    assert ryaml.loads('') is None

def test_loads_key():
    assert ryaml.loads('''
    key:

    ''') == { 'key': None }

def test_loads_key_value():
    assert ryaml.loads('''
    key:
        4

    ''') == { 'key': 4 }

def test_loads_key_sequence():
    assert ryaml.loads('''
    key:
        - 4
        - 5

    ''') == { 'key': [4, 5] }
