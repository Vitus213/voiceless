"""Build two Apple Silicon DMGs. --sign requires Developer ID and notarization."""
import argparse
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def dependencies(binary):
    lines = subprocess.check_output(['otool', '-L', binary], text=True).splitlines()[1:]
    return [line.strip().split(' (compatibility version')[0] for line in lines]


def bundle_libraries(binary, source, libs):
    # Follow only the transitive Mach-O dependencies each binary actually uses.
    # Normalize build paths and @rpath references before signing any payload.
    work = [binary]
    seen = set()
    while work:
        image = work.pop()
        if image in seen:
            continue
        seen.add(image)
        run('lipo', image, '-verify_arch', 'arm64')
        is_library = image.parent == libs
        entries = dependencies(image)
        if is_library:
            run('install_name_tool', '-id', '@rpath/' + image.name, image)
            entries = entries[1:]  # LC_ID_DYLIB
        for dep in entries:
            if dep.startswith(('/usr/lib/', '/System/Library/')):
                continue
            name = Path(dep).name
            lib = libs / name
            if not lib.exists():
                shutil.copyfile(source / name, lib)
                lib.chmod(0o755)
            work.append(lib)
            dest = '@loader_path/' + ('' if is_library else '../' + libs.name + '/') + name
            if dep != dest:
                run('install_name_tool', '-change', dep, dest, image)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    parser.add_argument('--sign', action='store_true')
    args = parser.parse_args()
    if platform.system() != 'Darwin' or platform.machine() != 'arm64':
        parser.error('an Apple Silicon macOS host is required')
    identity = os.environ['APPLE_SIGNING_IDENTITY'] if args.sign else '-'
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    config = json.loads((ROOT / 'desktop/tauri.conf.json').read_text())
    version = config['version']
    if os.environ.get('GITHUB_REF_TYPE') == 'tag' and os.environ['GITHUB_REF_NAME'] != 'v' + version:
        parser.error('release tag and desktop version differ')
    with tempfile.TemporaryDirectory() as temp:
        tmp = Path(temp)
        target = Path(os.environ.get('CARGO_TARGET_DIR', tmp / 'target')).resolve()
        env = dict(os.environ, CARGO_TARGET_DIR=str(target), MACOSX_DEPLOYMENT_TARGET='11.0')
        if env.get('VOICELESS_CONSOLE_DIST'):
            env['VOICELESS_CONSOLE_DIST'] = str(Path(env['VOICELESS_CONSOLE_DIST']).resolve())
        else:
            run('npm', 'ci', cwd=ROOT / 'frontend', env=env)
            run('npm', 'run', 'build', cwd=ROOT / 'frontend', env=env)
        for crate in ('rust', 'desktop'):
            features = ['--features', 'tauri/custom-protocol'] if crate == 'desktop' else []
            run('cargo', 'build', '--locked', '--release', *features, cwd=ROOT / crate, env=env)
        release = target / 'release'
        # Use the archive's lib directory, which includes versioned dylib aliases.
        archives = list((target / 'sherpa-onnx-prebuilt').glob('*-osx-arm64-shared-lib/lib'))
        if len(archives) != 1:
            raise RuntimeError(f'expected one arm64 sherpa library directory, found {archives}')
        for kind in ('cli', 'desktop'):
            volume = tmp / kind
            volume.mkdir()
            if kind == 'desktop':
                app = volume / 'Voiceless.app'
                contents = app / 'Contents'
                binaries = contents / 'MacOS'
                libs = contents / 'Frameworks'
                info = plistlib.loads((ROOT / 'desktop/Info.plist').read_bytes())
                info.update(CFBundleExecutable='voiceless-desktop', CFBundleIdentifier=config['identifier'],
                            CFBundlePackageType='APPL', CFBundleName='Voiceless', CFBundleVersion=version,
                            # The bundled arm64 ONNX Runtime 1.24.4 has LC_BUILD_VERSION minos 15.5.
                            CFBundleShortVersionString=version, LSMinimumSystemVersion='15.5', NSHighResolutionCapable=True)
                contents.mkdir(parents=True)
                (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
                (volume / 'Applications').symlink_to('/Applications')
                programs = ['voiceless', 'voiceless-desktop']
            else:
                binaries = volume / 'voiceless-cli/bin'
                libs = volume / 'voiceless-cli/lib'
                shutil.copy2(HERE / 'install-cli.command', volume)
                (volume / 'install-cli.command').chmod(0o755)
                programs = ['voiceless']
            binaries.mkdir(parents=True)
            libs.mkdir(parents=True)
            for name in programs:
                binary = binaries / name
                shutil.copy2(release / name, binary)
                bundle_libraries(binary, archives[0], libs)
            sign = ['codesign', '--force', '--sign', identity]
            if args.sign:
                sign.extend(['--options', 'runtime', '--timestamp'])
            for lib in libs.iterdir():
                run(*sign, lib)
            for name in programs:
                run(*sign, '--entitlements', HERE / 'entitlements.plist', binaries / name)
            if kind == 'desktop':
                run(*sign, '--entitlements', HERE / 'entitlements.plist', app)
                run('codesign', '--verify', '--strict', '--deep', app)
            cli_version = subprocess.check_output([binaries / 'voiceless', '--version'], text=True).strip()
            if cli_version.split()[-1] != version:
                raise RuntimeError(f'CLI version {cli_version} does not match desktop {version}')
            image = output / f'voiceless-{kind}-v{version}-macos-arm64.dmg'
            run('hdiutil', 'create', '-format', 'UDZO', '-volname', 'Voiceless ' + kind, '-srcfolder', volume, image)
            if args.sign:
                run(*sign, image)
                run('xcrun', 'notarytool', 'submit', image, '--keychain-profile', 'voiceless-notary', '--wait')
                run('xcrun', 'stapler', 'staple', image)
                run('xcrun', 'stapler', 'validate', image)


if __name__ == '__main__':
    main()
