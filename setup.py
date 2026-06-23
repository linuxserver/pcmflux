# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

from setuptools import setup
from setuptools_rust import Binding, RustExtension, Strip

with open("README.md", "r", encoding="utf-8") as fh:
    long_description = fh.read()

setup(
    name="pcmflux",
    version="2.0.0",
    author="Linuxserver.io",
    author_email="pypi@linuxserver.io",
    description="A performant audio capture pipeline that encodes raw PCM to Opus, skipping silence.",
    long_description=long_description,
    long_description_content_type="text/markdown",
    license="MPL-2.0",
    url="https://github.com/linuxserver/pcmflux",
    # Single self-contained Rust extension: the top-level `pcmflux` module does
    # PulseAudio capture + Opus encoding. No C/C++ sources, no Python package layer.
    packages=[],
    rust_extensions=[
        RustExtension(
            "pcmflux",
            "pcmflux/Cargo.toml",
            binding=Binding.PyO3,
            debug=False,
            strip=Strip.All,
        )
    ],
    classifiers=[
        "Programming Language :: Python :: 3",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.9",
    zip_safe=False,
)
