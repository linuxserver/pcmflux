import os
import shlex
import subprocess
import sys
from pathlib import Path
import setuptools
from setuptools import Extension, setup
from setuptools.command.build_ext import build_ext


def _pkg_config(*args):
    """Return pkg-config output tokens, or [] when pkg-config or the requested
    .pc files are unavailable (so the caller falls back to hardcoded flags)."""
    # Honor $PKG_CONFIG so cross-builds can point at a target-specific pkg-config
    # (e.g. aarch64-linux-gnu-pkg-config); default to the host "pkg-config".
    pkg_config = os.environ.get("PKG_CONFIG", "pkg-config")
    try:
        out = subprocess.check_output([pkg_config, *args], stderr=subprocess.DEVNULL)
        # Decode inside the try (surrogateescape) so non-UTF-8 build flags fall
        # back to the hardcoded -l flags instead of aborting the whole build.
        return out.decode(errors="surrogateescape").split()
    except (OSError, subprocess.CalledProcessError):
        return []


class BuildCtypesExt(build_ext):
    def get_ext_filename(self, fullname):
        # ctypes loads a fixed bare name, so emit a non-ABI-tagged filename that
        # build_lib, --inplace, and get_outputs() all agree on.
        return os.path.join(*fullname.split(".")) + ".so"

    def build_extensions(self):
        # Full compiler command (preserves a multi-token CXX like "ccache g++" or
        # a cross toolchain); fall back if compiler_cxx is empty.
        compiler = self.compiler.compiler_cxx or shlex.split(os.environ.get("CXX") or "g++")
        if isinstance(compiler, str):
            compiler = [compiler]

        ext = self.extensions[0]
        output_path = Path(self.get_ext_fullpath(ext.name))
        output_path.parent.mkdir(parents=True, exist_ok=True)

        # Resolve sources relative to this file so out-of-tree / sdist builds work.
        here = Path(__file__).parent.resolve()
        sources = [str(here / s) for s in ext.sources]

        libraries = ['pulse', 'pulse-simple', 'opus', 'pthread']
        extra_compile_args = ['-std=c++17', '-Wno-unused-function', '-fPIC', '-O3', '-shared']

        # Prefer pkg-config (conda/Homebrew/cross sysroots); fall back to hardcoded
        # -l flags. Coupled: only use pkg-config when --libs yields flags, else the
        # build would mix pkg-config includes with hardcoded -l linking.
        pkg_libs = _pkg_config("--libs", "opus", "libpulse-simple")
        use_pkg_config = bool(pkg_libs)
        pkg_cflags = _pkg_config("--cflags", "opus", "libpulse-simple") if use_pkg_config else []

        command = list(compiler)
        command.extend(extra_compile_args)
        command.extend(pkg_cflags)
        command.append('-o')
        command.append(str(output_path))

        command.extend(sources)

        if use_pkg_config:
            command.extend(pkg_libs)
            command.append('-lpthread')  # the opus/libpulse-simple .pc files may omit it
        else:
            for lib in libraries:
                command.append(f'-l{lib}')

        print("Running build command:")
        print(" ".join(command))
        try:
            subprocess.check_call(command)
        except subprocess.CalledProcessError as e:
            print(f"Build failed with exit code {e.returncode}", file=sys.stderr)
            sys.exit(1)
        except OSError as e:
            print(f"Build failed: could not run compiler '{command[0]}': {e}",
                  file=sys.stderr)
            sys.exit(1)

        print(f"Successfully built {output_path}")


# The .so is a plain ctypes library (no CPython ABI), so tag the wheel
# py3-none-<plat> instead of cp3x-cp3x. Degrade gracefully without bdist_wheel.
cmdclass = {"build_ext": BuildCtypesExt}
try:
    try:
        from setuptools.command.bdist_wheel import bdist_wheel as _bdist_wheel
    except ImportError:
        from wheel.bdist_wheel import bdist_wheel as _bdist_wheel

    class BdistWheel(_bdist_wheel):
        def get_tag(self):
            _, _, plat = super().get_tag()
            return "py3", "none", plat

    cmdclass["bdist_wheel"] = BdistWheel
except ImportError:
    pass

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

    ext_modules=[Extension("pcmflux.audio_capture_module",
                           sources=["pcmflux/audio_capture_module.cpp"])],

    cmdclass=cmdclass,

    package_data={"pcmflux": ["audio_capture_module.so"]},

    classifiers=[
        "Programming Language :: Python :: 3",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.6",
)
