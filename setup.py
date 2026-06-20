import os
import subprocess
from pathlib import Path

import setuptools
from setuptools import Extension, setup


def _pkg_config(*args):
    """pkg-config output tokens, or [] when pkg-config / the .pc files are
    missing (caller falls back to hardcoded -l flags)."""
    # Honor $PKG_CONFIG for cross-builds (e.g. aarch64-linux-gnu-pkg-config).
    pkg_config = os.environ.get("PKG_CONFIG", "pkg-config")
    try:
        out = subprocess.check_output([pkg_config, *args], stderr=subprocess.DEVNULL)
        return out.decode(errors="surrogateescape").split()
    except (OSError, subprocess.CalledProcessError):
        return []


# Prefer pkg-config (conda/Homebrew/cross sysroots); fall back to -l flags.
# Coupled: only trust pkg-config when --libs yields something, else we'd mix
# pkg-config cflags with hardcoded -l linking.
_pkg_libs = _pkg_config("--libs", "opus", "libpulse-simple")
if _pkg_libs:
    extra_link_args = _pkg_libs + ["-lpthread"]  # .pc files may omit pthread
    extra_compile_args_pkg = _pkg_config("--cflags", "opus", "libpulse-simple")
    libraries = []
else:
    extra_link_args = []
    extra_compile_args_pkg = []
    libraries = ["pulse", "pulse-simple", "opus", "pthread"]

extra_compile_args = ["-std=c++17", "-O3", "-fvisibility=hidden"] + extra_compile_args_pkg

capture_ext = Extension(
    "pcmflux._capture",
    sources=["pcmflux/audio_capture_module.cpp"],
    language="c++",
    libraries=libraries,
    extra_compile_args=extra_compile_args,
    extra_link_args=extra_link_args,
)

with open(Path(__file__).parent / "README.md", "r", encoding="utf-8") as fh:
    long_description = fh.read()

setup(
    name="pcmflux",
    version="1.0.8",
    author="Linuxserver.io",
    author_email="pypi@linuxserver.io",
    description="A performant audio capture pipeline that encodes raw PCM to Opus, skipping silence.",
    long_description=long_description,
    long_description_content_type="text/markdown",
    license="MPL-2.0",
    url="https://github.com/linuxserver/pcmflux",
    packages=setuptools.find_packages(),
    ext_modules=[capture_ext],
    classifiers=[
        "Programming Language :: Python :: 3",
        "Programming Language :: Python :: 3.9",
        "Programming Language :: Python :: 3.10",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
        "Programming Language :: Python :: 3.13",
        "Programming Language :: Python :: 3.14",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.9",
)
