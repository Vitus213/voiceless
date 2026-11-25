"""Install the Windows candidates as a standard user and check paste, tray and the integrity refusal."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from validate import recognition_fixture


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidates', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        parser.error('requires the hosted Windows guest')
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        env = dict(os.environ, XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'), XDG_CACHE_HOME=str(root / 'cache'))
        speech = recognition_fixture(root, env)
        harness = Path(__file__).with_name('standard-user.ps1')
        subprocess.run(['pwsh', '-NoProfile', '-STA', '-File', str(harness), '-Candidates', str(args.candidates),
                        '-Root', str(root), '-Speech', str(speech), '-Evidence', str(args.evidence)],
                       env=env, check=True, timeout=2400)


if __name__ == '__main__':
    main()
