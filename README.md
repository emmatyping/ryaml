ryaml
=====

### *Quickly and safely parse yaml*

### What is ryaml?

ryaml is a Python library that wraps a Rust yaml parser, [libyaml-safer](https://github.com/simonask/libyaml-safer),
to quickly and safely parse and dump yaml to and from Python objects.

The project is *drop-in compatible* with PyYAML, meaning that any behavior differences are a bug and should be reported against this project.

Like PyYAML, this library implements the YAML 1.1 specification.

## Installation

We ship binary wheels for Windows, Linux, and macOS, so as long as you are using Python 3.10+,
you can run:

```
$ python -m pip install ryaml
```

Otherwise, you will need to build from source. To do so, first install Rust 1.41 stable.

Then you should be able to just

```shell
$ git clone https://github.com/emmatyping/ryaml
$ cd ryaml
$ python -m pip install .
```

Or if you want to build a wheel:

```shell
$ git clone https://github.com/emmatyping/ryaml
$ cd ryaml
$ python -m pip install maturin
$ maturin build --release --no-sdist
# OR if you want an abi3 wheel (compatible with Python 3.10+)
$ maturin build --release --no-sdist --cargo-extra-args="--features=abi3"
```

And a wheel will be created in `target/wheels` which you can install.

## Usage

The API of `ryaml` is very similar to that of `json` in the standard library:

You can use `ryaml.loads` to read from a `str`:

```python
import ryaml
obj = ryaml.loads('key: [10, "hi"]')
assert isinstance(obj, dict) # True
assert obj['key'][1] == "hi" # True
```

And `ryaml.dumps` to dump an object into a yaml file:

```python
import ryaml
s = ryaml.dumps({ 'key' : None })
print(s)
# prints:
# ---
# key: ~
```

There are also `ryaml.load` and `ryaml.load_all` to read yaml document(s) from files:

```python
import ryaml
obj = {'a': [{'b': 1}]}
with open('test.yaml', 'w') as w:
    ryaml.dump(w, obj)
with open('test.yaml', 'r') as r:
    assert ryaml.load(r) == obj
with open('multidoc.yaml', 'w') as multi:
    multi.write('''
---
a:
  key:
...
---
b:
  key:
    ''')
with open('multidoc.yaml', 'r') as multi:
    docs = ryaml.load_all(multi)
assert len(docs) == 2
assert docs[0]['a']['key'] is None
```

`ryaml.load_all` will, as seen above, load multiple documents from a single file.

### Custom representers

With PyYAML installed, register callbacks on `ryaml.RSafeDumper` or on a subclass
to control how Python objects become YAML nodes. `add_representer` matches an
exact type. `add_multi_representer` matches a type and its subclasses using
Python's method resolution order. An exact registration takes precedence.

```python
from datetime import timedelta
from pathlib import PurePath

import ryaml


class ConfigDumper(ryaml.RSafeDumper):
    pass


def represent_path(dumper, path):
    return dumper.represent_scalar("tag:yaml.org,2002:str", path.as_posix())


def represent_duration(dumper, duration):
    return dumper.represent_scalar("tag:yaml.org,2002:int", str(int(duration.total_seconds())))


ConfigDumper.add_multi_representer(PurePath, represent_path)
ConfigDumper.add_representer(timedelta, represent_duration)

config = {"path": PurePath("config.yaml"), "timeout": timedelta(minutes=25)}
print(ryaml.dumps(config, Dumper=ConfigDumper))
```

Pass the same class as `Dumper=` to `ryaml.dump` or `yaml.dump`. The module-level
`ryaml.add_representer` and `ryaml.add_multi_representer` functions default to
registering on `ryaml.RSafeDumper`.

Representers work as in PyYAML: they can call `dumper.represent_scalar`,
`represent_sequence`, `represent_mapping` and `represent_data`, change the nodes
these return, or build `yaml.nodes` objects themselves. Registering a representer
for a built-in type such as `str` or `dict` replaces ryaml's. Built-in types without
one are still represented in Rust, so registering representers does not slow down
dumping the rest of the data.

The nodes a representer returns are written as PyYAML's `CSafeDumper` writes them,
with three differences: ryaml raises `TypeError` for anything that is not a node,
which `CSafeDumper` silently leaves out; it writes subclasses of the node classes,
as PyYAML's pure-Python `SafeDumper` does; and for a `bytes` tag or value that is
not UTF-8 it raises `UnicodeDecodeError`, where `CSafeDumper` raises `MemoryError`.


## Thanks

This project is standing on the shoulders of giants, and would not be possible without:

[pyo3](https://pyo3.rs/)

[libyaml-safer](https://github.com/simonask/libyaml-safer)

[saphyr-parser-bw](https://github.com/bourumir-wyngs/saphyr/tree/dev/saphyr-parser)

[pyyaml](https://github.com/yaml/pyyaml/)

[yaml-test-suite](https://github.com/yaml/yaml-test-suite)

Earlier versions of this library were based on the following projects:

[serde-yaml](https://github.com/dtolnay/serde-yaml)

[yaml-rust](https://github.com/chyh1990/yaml-rust)

[pythonize](https://github.com/davidhewitt/pythonize)
