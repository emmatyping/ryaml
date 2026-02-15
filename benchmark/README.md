# ryaml benchmarks

These benchmarks come from [yaml-rs](github.com/lava-sh/yaml-rs).

## Create and activate virtual environment

```bash
# Linux / MacOS
python3 -m venv .venv
source .venv/bin/activate

# Windows
py -m venv .venv
.venv\scripts\activate
```

## Install benchmark dependencies

```bash
# Using pip
pip install -r benchmark/bench_requirements.txt
pip install .

# Using uv
uv pip install -r benchmark/bench_requirements.txt
uv pip install .
```

## Run `benchmark/run.py`

```bash
python benchmark/run.py
```

## Results

### loads

![YAML loads benchmark](loads.svg)

### dumps

![YAML dumps benchmark](dumps.svg)
