"""Install each Windows candidate and transcribe a real clip with its bundled CPU runtime."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from validate import recognition_fixture, verify_recognition


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('candidates', type=Path)
    args = parser.parse_args()
    version = json.loads((Path(__file__).resolve().parents[3] / 'desktop/tauri.conf.json').read_text())['version']
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'), XDG_CACHE_HOME=str(root / 'cache'))
        speech = recognition_fixture(root, env)
        for kind in ['core', 'desktop']:
            setup = args.candidates / f'voiceless-{kind}-v{version}-windows-x64.exe'
            destination = root / kind
            flags = ['/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART']
            subprocess.run([setup, *flags, f'/DIR={destination}'], check=True, timeout=120)
            try:
                cli = destination / 'voiceless.exe'
                reported = subprocess.check_output([cli, '--version'], text=True, env=env).strip()
                if reported != f'voiceless {version}':
                    raise ValueError(f'unexpected installed CLI version: {reported}')
                verify_recognition(cli, speech, env)
                if kind == 'desktop':
                    executable = destination / 'voiceless-desktop.exe'
                    subprocess.run(['pwsh', '-NoProfile', '-File', Path(__file__).with_name('window.ps1'), executable],
                                   env=env, check=True, timeout=180)
            finally:
                subprocess.run([destination / 'unins000.exe', *flags], check=True, timeout=90)


if __name__ == '__main__':
    main()
