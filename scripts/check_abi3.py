"""Install one wheel unchanged into isolated CPython environments and test it."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import zipfile


def run(*command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheel", type=Path)
    parser.add_argument("--uv", default="uv")
    parser.add_argument("--python", nargs="+", default=["3.10", "3.11", "3.12", "3.13", "3.14", "3.15"])
    options = parser.parse_args()
    wheel = options.wheel.resolve(strict=True)
    tests = Path(__file__).resolve().parents[1] / "tests"
    with zipfile.ZipFile(wheel) as archive:
        extensions = [name for name in archive.namelist() if name.endswith((".so", ".pyd"))]
        if len(extensions) != 1:
            raise ValueError("expected exactly one native extension in the wheel")
        digest = hashlib.sha256(archive.read(extensions[0])).hexdigest()

    environment = dict(os.environ, PYTHONMALLOC="debug", PYTHONNOUSERSITE="1")
    # Prevent a caller's source checkout from hiding the installed distribution.
    environment.pop("PYTHONPATH", None)
    environment.pop("PYTHONHOME", None)
    probe = """
import hashlib, json, platform
from pathlib import Path
from rustializer import _feasibility
print(json.dumps({
    'python': platform.python_version(),
    'implementation': platform.python_implementation(),
    'sha256': hashlib.sha256(Path(_feasibility.__file__).read_bytes()).hexdigest(),
}))
"""
    with tempfile.TemporaryDirectory(prefix="rustializer-abi3-") as directory:
        for version in options.python:
            virtualenv = Path(directory) / f"python-{version}"
            run(options.uv, "venv", "--python", version, str(virtualenv), env=environment)
            python = virtualenv / ("Scripts/python.exe" if os.name == "nt" else "bin/python")
            run(options.uv, "pip", "install", "--no-deps", "--link-mode", "copy", "--python", str(python), str(wheel), env=environment)
            result = run(str(python), "-I", "-X", "dev", "-c", probe, env=environment, text=True, capture_output=True)
            metadata = json.loads(result.stdout)
            if metadata["sha256"] != digest or metadata["implementation"] != "CPython":
                raise RuntimeError(f"binary or interpreter mismatch: {metadata}")
            if not metadata["python"].startswith(version + "."):
                raise RuntimeError(f"requested {version}, got {metadata['python']}")
            print(json.dumps(metadata), flush=True)
            run(str(python), "-I", "-X", "dev", "-m", "unittest", "discover", "-s", str(tests), "-p", "test_native_object_model.py", "-v", env=environment)
    print(f"Verified identical extension SHA-256 {digest} on {len(options.python)} interpreters.", flush=True)


if __name__ == "__main__":
    main()
