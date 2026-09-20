#!/usr/bin/env python3
"""Compatibility command: package a development binary in the shared release format."""
import argparse
import importlib.util
from pathlib import Path

SPEC = importlib.util.spec_from_file_location("package_release", Path(__file__).with_name("package-release.py"))
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)
package = RELEASE.package
subprocess = RELEASE.subprocess

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path("dist/agents"))
    args = parser.parse_args()
    package(args.binary, args.output_dir)
