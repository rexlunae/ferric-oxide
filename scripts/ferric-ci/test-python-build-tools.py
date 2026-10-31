#!/usr/bin/env python3.12
"""Import a compiled host build module through the cookbook's real Python setup.

Requires Python 3.12, its development headers and a host C compiler. No network,
recipe downloads or target interpreter are used.
"""

import os
from pathlib import Path
import shlex
import subprocess
import sys
import sysconfig
import tempfile


def main():
    if sys.version_info[:2] != (3, 12):
        raise RuntimeError("run this regression with python3.12 to match the build recipes")
    root = Path(__file__).resolve().parents[2]
    source = (root / "tools/cookbook/src/cook/script.rs").read_text()
    prescript = source.split('pub(crate) static BUILD_PRESCRIPT: &str = r#"', 1)[1].split('"#;', 1)[0]
    version = "python3.12"
    suffix = sysconfig.get_config_var("EXT_SUFFIX")
    if not suffix:
        raise RuntimeError("host Python must support extension modules")
    with tempfile.TemporaryDirectory(prefix="python-build-tools ") as temporary:
        work = Path(temporary)
        toolchain = work / "toolchain"
        sysroot = work / "sysroot"
        host_modules = toolchain / "lib" / version / "site-packages"
        target_stdlib = sysroot / "usr/lib" / version
        target_modules = target_stdlib / "site-packages"
        for path in [toolchain / "bin", host_modules, target_modules, work / "build", work / "shims"]:
            path.mkdir(parents=True, exist_ok=True)
        (toolchain / "bin/python3").symlink_to(sys.executable)
        native_source = work / "module.c"
        native_source.write_text("""
#include <Python.h>
static PyObject *answer(PyObject *self, PyObject *args) {
    (void)self; (void)args;
    return PyLong_FromLong(42);
}
static PyMethodDef methods[] = {
    {"answer", answer, METH_NOARGS, "Check that a host extension was imported."},
    {NULL, NULL, 0, NULL}
};
static struct PyModuleDef module = {
    PyModuleDef_HEAD_INIT, "build_native", NULL, -1, methods,
    NULL, NULL, NULL, NULL
};
PyMODINIT_FUNC PyInit_build_native(void) { return PyModule_Create(&module); }
""")
        flags = ["-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"]
        if sys.platform == "darwin":
            flags += ["-undefined", "dynamic_lookup"]
        subprocess.run([
            *shlex.split(os.environ.get("CC", "cc")), *flags,
            "-I", sysconfig.get_path("include"), str(native_source),
            "-o", str(host_modules / f"build_native{suffix}"),
        ], check=True)
        # A target extension must never be offered to the host interpreter.
        (target_modules / f"build_native{suffix}").write_bytes(b"incompatible Redox extension")
        (target_modules / "target_only.py").write_text("raise RuntimeError('target module imported')\n")
        config_name = "_sysconfigdata__redox_x86_64-redox"
        (target_stdlib / f"{config_name}.py").write_text("build_time_vars = {'FERRIC_TARGET_SENTINEL': 73}\n")
        script = prescript + """
test "$COOKBOOK_PYTHON_V" = python3.12
PYTHON_INIT
"$COOKBOOK_PYTHON" - <<'CHECK'
import build_native
import importlib.util
import sysconfig
assert build_native.answer() == 42
assert importlib.util.find_spec('target_only') is None
assert sysconfig.get_config_var('FERRIC_TARGET_SENTINEL') == 73
CHECK
"""
        environment = os.environ.copy()
        environment.update({
            "TARGET": "x86_64-unknown-redox",
            "COOKBOOK_HOST_TARGET": "x86_64-unknown-linux-gnu",
            "FERRIC_BUILD_TOOLS": "guest",
            "COOKBOOK_ROOT": str(root),
            "COOKBOOK_BUILD": str(work / "build"),
            "COOKBOOK_TOOLCHAIN": str(toolchain),
            "COOKBOOK_SYSROOT": str(sysroot),
            "COOKBOOK_GUEST_TOOLS_SHIMS": str(work / "shims"),
            "PYTHONPATH": "",
            "LD_LIBRARY_PATH": "",
        })
        subprocess.run(["bash", "-e", "-c", script], cwd=work, env=environment, check=True)
    print("Python build-tool import regression passed")


if __name__ == "__main__":
    main()
