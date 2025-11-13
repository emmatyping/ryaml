import ryaml
import yaml
import platform
import pytest

if platform.python_implementation() == 'CPython':
    try:
        from yaml import CSafeLoader as Loader, CDumper as Dumper
    except ImportError:
        pytest.skip(allow_module_level=True)
else:
    from yaml import SafeLoader as Loader, Dumper as Dumper

DATA = [['college', -380608299.3165369, {'closely': 595052867, 'born': False, 'stomach': True, 'expression': True,
                                         'chosen': 34749965, 'somebody': False}, 'positive', True, False], 'price', 2018186817, 'average', 'young', -1447308110]

SRC = """
---
- - college
  - -380608299.3165369
  - closely: 595052867
    born: false
    stomach: true
    expression: true
    chosen: 34749965
    somebody: false
  - positive
  - true
  - false
- price
- 2018186817
- average
- young
- -1447308110

"""

MULTIDOC = """
---
basis: true
discussion: 1690275082
twice: count
another: false
tiny:
  worth: straw
  plus: ride
  duty: basis
  wave:
    - seeing
    - outline
    - true
    - congress
    - -870479755
    - truck
  large: rhyme
  load: did
...
---
getting:
  final:
    - false
    - true
    - -2020793880.414512
    - true
    - -950872146.990103
    - thing
  arrange: naturally
  breakfast: 1065730575
  clothes: drop
  mean: flame
  north: silly
fireplace: why
prove: true
various: true
breeze: true
us: true
"""


@pytest.mark.benchmark(group="dump")
def test_ryaml_bench_dump(benchmark, yaml_file):
    benchmark(ryaml.dump, yaml_file, DATA)


@pytest.mark.benchmark(group="dump")
def test_pyyaml_bench_dump(benchmark, yaml_file):
    benchmark(yaml.dump, DATA, yaml_file, Dumper=Dumper)


@pytest.mark.benchmark(group="dumps")
def test_ryaml_bench_dumps(benchmark):
    benchmark(ryaml.dumps, DATA)


@pytest.mark.benchmark(group="dumps")
def test_pyyaml_bench_dumps(benchmark):
    benchmark(yaml.dump, DATA, Dumper=Dumper)


@pytest.mark.benchmark(group="load")
def test_ryaml_bench_load(benchmark, yaml_file):
    benchmark(ryaml.load, yaml_file)


@pytest.mark.benchmark(group="load")
def test_pyyaml_bench_load(benchmark, yaml_file):
    benchmark(yaml.load, yaml_file, Loader=Loader)


@pytest.mark.benchmark(group="loads")
def test_ryaml_bench_loads(benchmark):
    benchmark(ryaml.loads, SRC)


@pytest.mark.benchmark(group="loads")
def test_pyyaml_bench_loads(benchmark):
    benchmark(yaml.load, SRC, Loader=Loader)


def ryaml_load_all(file):
    ryaml.load_all(file)


def pyyaml_load_all(file):
    list(yaml.load_all(file, Loader=Loader))


@pytest.mark.benchmark(group="load_all")
def test_ryaml_bench_load_all(benchmark, yaml_file):
    benchmark(ryaml_load_all, yaml_file)


@pytest.mark.benchmark(group="load_all")
def test_pyyaml_bench_load_all(benchmark, yaml_file):
    benchmark(pyyaml_load_all, yaml_file)


def ryaml_loads_all(str):
    list(ryaml.loads_all(str))


def pyyaml_loads_all(str):
    list(yaml.load_all(str, Loader=Loader))


@pytest.mark.benchmark(group="loads_all")
def test_ryaml_bench_loads_all(benchmark):
    benchmark(ryaml_loads_all, MULTIDOC)


@pytest.mark.benchmark(group="loads_all")
def test_pyyaml_bench_loads_all(benchmark):
    benchmark(pyyaml_loads_all, MULTIDOC)
