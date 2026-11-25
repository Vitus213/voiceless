"""Build unsigned x64 MSVC core and desktop installer candidates with Inno Setup 6."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[3]
TARGET = 'x86_64-pc-windows-msvc'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    if os.name != 'nt':
        parser.error('run on Windows x64 with the MSVC toolchain')
    compiler = shutil.which('iscc') or Path(os.environ['ProgramFiles(x86)']) / 'Inno Setup 6/ISCC.exe'
    version = json.loads((ROOT / 'desktop/tauri.conf.json').read_text())['version']
    for crate in ('rust', 'desktop'):
        with (ROOT / crate / 'Cargo.toml').open('rb') as manifest:
            if tomllib.load(manifest)['package']['version'] != version:
                raise ValueError(f'{crate} version differs from desktop configuration')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    npm = shutil.which('npm.cmd')
    subprocess.run([npm, 'ci'], cwd=ROOT / 'frontend', check=True)
    subprocess.run([npm, 'run', 'build'], cwd=ROOT / 'frontend', check=True)
    with tempfile.TemporaryDirectory() as directory:
        target = Path(directory) / 'target'
        env = dict(os.environ, CARGO_TARGET_DIR=str(target))
        payload = target / TARGET / 'release'
        for crate, kind in [('rust', 'core'), ('desktop', 'desktop')]:
            features = ['--features', 'tauri/custom-protocol'] if kind == 'desktop' else []
            subprocess.run(['cargo', 'build', '--release', '--locked', '--target', TARGET, *features],
                           cwd=ROOT / crate, env=env, check=True)
            for dll in ['onnxruntime.dll', 'sherpa-onnx-c-api.dll']:
                if not (payload / dll).is_file():
                    raise FileNotFoundError(payload / dll)
            defines = ['/DDesktop'] if kind == 'desktop' else []
            subprocess.run([compiler, f'/O{output}', f'/DVersion={version}', f'/DPayload={payload}',
                            *defines, ROOT / 'desktop/packaging/windows.iss'], check=True)


if __name__ == '__main__':
    main()
