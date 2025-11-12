import ryaml

def test_dumps_none():
    assert ryaml.dumps(None) == 'null\n'

def test_dumps_key():
    assert ryaml.dumps({ 'key': None }) == 'key: null\n'

def test_dumps_key_value():
    assert ryaml.dumps({ 'key': 4 }) == 'key: 4\n'

def test_dumps_key_sequence():
    assert ryaml.dumps({ 'key': [4, 5] }) == 'key:\n- 4\n- 5\n'
