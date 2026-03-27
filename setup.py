import subprocess
import sys
from pathlib import Path
import setuptools
from setuptools import Extension, setup
from setuptools.command.build_ext import build_ext

class BuildCtypesExt(build_ext):
    def build_extensions(self):
        compiler = self.compiler.compiler_cxx[0]

        lib_dir = Path(self.build_lib)

        output_path = lib_dir / "pcmflux" / "audio_capture_module.so"

        output_path.parent.mkdir(parents=True, exist_ok=True)
        
        sources = ['pcmflux/audio_capture_module.cpp']
        include_dirs = []
        
        libraries = ['pulse', 'pulse-simple', 'opus', 'pthread']
        extra_compile_args = ['-std=c++17', '-Wno-unused-function', '-fPIC', '-O3', '-shared']
        
        command = [compiler]
        command.extend(extra_compile_args)
        command.append('-o')
        command.append(str(output_path))
        
        for include_dir in include_dirs:
            command.append(f'-I{include_dir}')
            
        command.extend(sources)
        
        for lib in libraries:
            command.append(f'-l{lib}')
        
        print("Running build command:")
        print(" ".join(command))
        try:
            subprocess.check_call(command)
        except subprocess.CalledProcessError as e:
            print(f"Build failed with exit code {e.returncode}", file=sys.stderr)
            sys.exit(1)

        print(f"Successfully built {output_path}")

with open("README.md", "r", encoding="utf-8") as fh:
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

    ext_modules=[Extension("pcmflux.audio_capture_module", sources=[])],

    cmdclass={"build_ext": BuildCtypesExt},

    package_data={"pcmflux": ["audio_capture_module.so"]},

    classifiers=[
        "Programming Language :: Python :: 3",
        "Operating System :: POSIX :: Linux",
    ],
    python_requires=">=3.6",
)
