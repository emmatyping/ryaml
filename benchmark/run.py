import io
import platform
import time
from collections.abc import Callable
from pathlib import Path

import altair as alt
import cpuinfo
import oyaml
import polars as pl
import ruamel.yaml
import ryaml
import yaml as pyyaml
import yaml_rs

N = 50

FILE_PATH = Path(__file__).resolve().parent
YAMLS = FILE_PATH / "data"
# example of a config file for app
FILE_1 = YAMLS / "config.yaml"
# file from https://github.com/yaml/yaml-test-suite
FILE_2 = YAMLS / "UGM3.yaml"
# file from `https://examplefile.com`
FILE_3 = YAMLS / "bench.yaml"

FILES = [FILE_1, FILE_2, FILE_3]

CPU = cpuinfo.get_cpu_info()["brand_raw"]
PY_VERSION = f"{platform.python_version()} ({platform.system()} {platform.release()})"


def benchmark(func: Callable, count: int) -> float:
    start = time.perf_counter()
    for _ in range(count):
        func()
    end = time.perf_counter()
    return end - start


def plot_benchmark(
        results: dict[str, float],
        save_path: Path,
        run_type: str,
) -> None:
    df = (
        pl.DataFrame({
            "parser": list(results.keys()),
            "exec_time": list(results.values()),
        })
        .sort("exec_time")
        .with_columns([
            (pl.col("exec_time") / pl.col("exec_time").min()).alias("slowdown"),
        ])
    )

    chart = (
        alt.Chart(df)
        .mark_bar(cornerRadiusTopLeft=6, cornerRadiusTopRight=6)
        .encode(
            x=alt.X("parser:N", sort=None, title="Parser", axis=alt.Axis(labelAngle=0)),
            y=alt.Y(
                "exec_time:Q",
                title="Execution Time (seconds, lower=better)",
                scale=alt.Scale(domain=(0, df["exec_time"].max() * 1.04)),
                axis=alt.Axis(grid=False),
            ),
            color=alt.Color("parser:N", legend=None, scale=alt.Scale(scheme="dark2")),
            tooltip=[
                alt.Tooltip("parser:N", title=""),
                alt.Tooltip("exec_time:Q", title="Execution Time (s)", format=".4f"),
                alt.Tooltip("slowdown:Q", title="Slowdown", format=".2f"),
            ],
        )
    )

    text = (
        chart.mark_text(
            align="center",
            baseline="bottom",
            dy=-2,
            fontSize=9,
            fontWeight="bold",
        )
        .transform_calculate(
            label='format(datum.exec_time, ".4f") + '
            '"s (x" + format(datum.slowdown, ".2f") + ")"',
        )
        .encode(text="label:N")
    )

    (chart + text).properties(
        width=800,
        height=600,
        title={
            "text": f"YAML parsers benchmark ({run_type})",
            "subtitle": f"Python: {PY_VERSION} | CPU: {CPU}",
        },
    ).save(save_path)


def run(run_count: int) -> None:
    load_total = {}
    dump_total = {}

    for path in FILES:
        data = path.read_text(encoding="utf-8")

        loads = {
            "yaml_rs": lambda d=data: yaml_rs.loads(d),
            "yaml_rs (parse_dt=False)": lambda d=data: yaml_rs.loads(
                d, parse_datetime=False,
            ),
            "ryaml": lambda d=data: ryaml.loads_all(d),
            "PyYAML": lambda d=data: list(pyyaml.safe_load_all(d)),
            "PyYAML (CLoader)": lambda d=data: list(pyyaml.load_all(
                d, Loader=pyyaml.CLoader,
            )),
            "PyYAML (CSafeLoader)": lambda d=data: list(
                pyyaml.load_all(d, Loader=pyyaml.CSafeLoader),
            ),
            "ruamel.yaml": lambda d=data: list(ruamel.yaml.YAML(typ="safe").load_all(d)),
            "oyaml": lambda d=data: list(oyaml.safe_load_all(d)),
        }

        dumps = {
            "yaml_rs": lambda d=data: yaml_rs.dumps(d),
            "ryaml": lambda d=data: ryaml.dumps(d),
            "PyYAML": lambda d=data: pyyaml.dump(d),
            "PyYAML (CDumper)": lambda d=data: pyyaml.dump(d, Dumper=pyyaml.CDumper),
            "PyYAML (CSafeDumper)": lambda d=data: pyyaml.dump(
                d, Dumper=pyyaml.CSafeDumper,
            ),
            "ruamel.yaml": (
                lambda d=data: (
                    (lambda yaml: (lambda buf: (yaml.dump(d, buf), buf.getvalue())[1])(
                        io.StringIO(),
                    ))(ruamel.yaml.YAML(typ="safe"))
                )
            ),
            "oyaml": lambda d=data: oyaml.dump(d),
        }

        for name, func in loads.items():
            load_total.setdefault(name, []).append(benchmark(func, run_count))

        for name, func in dumps.items():
            dump_total.setdefault(name, []).append(benchmark(func, run_count))

    avg_loads = {name: sum(times) / len(times) for name, times in load_total.items()}
    avg_dumps = {name: sum(times) / len(times) for name, times in dump_total.items()}

    plot_benchmark(avg_loads, FILE_PATH / "loads.svg", "loads")
    plot_benchmark(avg_dumps, FILE_PATH / "dumps.svg", "dumps")


if __name__ == "__main__":
    run(N)
