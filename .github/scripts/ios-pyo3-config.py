"""Configure PyO3 using the Python framework in cibuildwheel's iOS cross venv."""

import os
import sys
import sysconfig
from pathlib import Path

version = f"{sys.version_info.major}.{sys.version_info.minor}"
extension_suffix = sysconfig.get_config_var("EXT_SUFFIX")
framework = sysconfig.get_config_var("PYTHONFRAMEWORK")
framework_prefix = sysconfig.get_config_var("PYTHONFRAMEWORKPREFIX")

Path(os.environ["PYO3_CONFIG_FILE"]).write_text(
    f"version={version}\n"
    f"ext_suffix={extension_suffix}\n"
    # PyO3's default iOS linker flags expect a standalone libpython.
    "suppress_build_script_link_lines=true\n"
    f"extra_build_script_line=cargo:rustc-link-search=framework={framework_prefix}\n"
    f"extra_build_script_line=cargo:rustc-link-lib=framework={framework}\n",
    encoding="utf-8",
)
