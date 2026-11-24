#!/usr/bin/env python3
"""Validate native and npm candidates on the matching build architecture."""

import argparse
import base64
import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath

from native import (
    GLIBC, PLATFORMS, SHERPA_GITHUB, VULKAN_LICENSES, check_machine, digest, download,
    inventory, leaf_name, output, package_json, require, target,
)


MODEL = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09"
MODEL_SHA = "7305f7905bfcf77fa0b39388a313f3da35c68d971661a65475b56fb2162c8e63"
BUN_VERSION = "1.4.5"
BUN_INPUTS = {
    "x86_64": ("bun-linux-x64-baseline", "dcb23a18a1c2fff12d57bcef696dbbb42eb67fde29f43816ecb595459a24c463"),
    "aarch64": ("bun-linux-aarch64", "18656a3bf0ea5641f5796ae4213b75255579a70a0fded2f337933785568cdf84"),
}
SYSTEM_LIBRARIES = {"libc.so.6", "libm.so.6", "libpthread.so.0", "libdl.so.2", "librt.so.1",
                    "libstdc++.so.6", "libgcc_s.so.1", "libutil.so.1"}
# Ubuntu 22.04 ships libstdc++ from GCC 12, so GLIBCXX_3.4.30 belongs to the
# baseline although the build runs under g++-11.
SYMBOL_CEILINGS = {"GLIBC": (2, 35), "GLIBCXX": (3, 4, 30), "CXXABI": (1, 3, 13)}


def unpack(archive, root, prefix=""):
    seen = set()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar:
            name = member.name
            require(name.startswith(prefix), f"invalid archive prefix: {name}")
            name = name[len(prefix):]
            path = PurePosixPath(name)
            require(bool(name) and name == path.as_posix() and not path.is_absolute() and
                    not {".", ".."}.intersection(path.parts) and name not in seen,
                    f"invalid or repeated archive path: {name}")
            require(member.isfile() and member.mode in (0o644, 0o755), f"invalid file or mode: {name}")
            seen.add(name)
            dest = root / name
            dest.parent.mkdir(parents=True, exist_ok=True)
            with tar.extractfile(member) as source, dest.open("xb") as result:
                shutil.copyfileobj(source, result)
            dest.chmod(member.mode)


def verify_elf(tree, arch, env):
    binary = tree / "bin/voiceless"
    loader = PLATFORMS[arch]["loader"]
    libraries = set(PLATFORMS[arch]["libraries"])
    paths = {binary: "$ORIGIN/../lib"}
    paths.update({tree / "lib" / name: "$ORIGIN" for name in sorted(libraries)})
    linked = set()
    for path, runpath in paths.items():
        check_machine(path, arch)
        program = output("readelf", "--program-headers", "--wide", path)
        expected = [loader] if path == binary else []
        require(re.findall(r"Requesting program interpreter: ([^\]]+)\]", program) == expected,
                f"unexpected ELF interpreter in {path}")
        dynamic = output("readelf", "--dynamic", "--wide", path)
        require(re.findall(r"\((?:RPATH|RUNPATH)\).*\[([^\]]+)\]", dynamic) == [runpath],
                f"unexpected runtime library paths in {path}")
        needed = set(re.findall(r"\(NEEDED\).*\[([^\]]+)\]", dynamic))
        require(needed <= SYSTEM_LIBRARIES | libraries | {Path(loader).name},
                f"unexpected DT_NEEDED in {path}: {sorted(needed)}")
        linked.update(needed & libraries)
        if path != binary:
            require(re.findall(r"\(SONAME\).*\[([^\]]+)\]", dynamic) == [path.name],
                    f"unexpected SONAME in {path}")
        version_info = output("readelf", "--version-info", "--wide", path)
        needs = version_info.partition("Version needs section")[2]
        for family, ceiling in SYMBOL_CEILINGS.items():
            for symbol in re.findall(rf"\bName: ({family}_[\w.]+)", needs):
                version = symbol.split("_", 1)[1]
                if not (re.fullmatch(r"[0-9]+(?:\.[0-9]+)*", version) and
                        tuple(map(int, version.split("."))) <= ceiling):
                    bound = re.findall(rf"\sUND\s+(\S+)@{re.escape(symbol)}\b",
                                       output("readelf", "--dyn-syms", "--wide", path))
                    raise ValueError(f"{path.name} requires unsupported symbol version {symbol}: {bound}")
        notes = output("readelf", "--notes", "--wide", path)
        require(not re.search(r"x86 ISA needed:.*x86-64-v[234]", notes),
                f"{path.name} declares an x86-64 ISA above the baseline")
    require(linked == libraries, "not all bundled libraries are required")
    resolved = output(loader, "--list", binary, env=env)
    print(resolved, flush=True)
    for name in sorted(libraries):
        match = re.search(rf"^\s*{re.escape(name)} => (.+) \(0x[0-9a-f]+\)$", resolved, re.M)
        require(match and Path(match.group(1)).resolve() == tree / "lib" / name,
                f"the system loader did not resolve the bundled {name}")


def clean_environment(work):
    home = work / "home"
    home.mkdir()
    return {
        "HOME": str(home), "PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8",
        "XDG_CONFIG_HOME": str(home / "config"), "XDG_DATA_HOME": str(home / "data"),
        "XDG_CACHE_HOME": str(home / "cache"), "LD_BIND_NOW": "1",
        "DO_NOT_TRACK": "1", "CI": "1",
    }


def verify_recognition(binary, work, env):
    package = work / f"{MODEL}.tar.bz2"
    download(f"{SHERPA_GITHUB}/releases/download/asr-models/{package.name}", package, MODEL_SHA)
    model = work / "model"
    model.mkdir()
    with tarfile.open(package, "r:bz2") as fixture:
        members = {"model.int8.onnx": model / "model.int8.onnx", "tokens.txt": model / "tokens.txt",
                   "test_wavs/en.wav": work / "speech.wav"}
        for name, dest in members.items():
            with fixture.extractfile(f"{MODEL}/{name}") as source, dest.open("xb") as result:
                shutil.copyfileobj(source, result)
    config = Path(env["XDG_CONFIG_HOME"]) / "voiceless/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text(f"[asr]\nbatch_model_dir = {json.dumps(str(model))}\n", encoding="utf-8")
    config.chmod(0o600)
    result = json.loads(output(binary, "transcribe", work / "speech.wav", "--backend", "local", "--json",
                               env=env, timeout=90))
    require(result["type"] == "transcript" and result["backend"] == "local" and result["text"].strip(),
            f"SenseVoice produced no speech transcript: {result}")
    print("SenseVoice fixture transcribed successfully", flush=True)


def verify_standalone(binary, archive, version, env):
    subprocess.run([binary, "install", archive, "--sha256", digest(archive)],
                   env=env, check=True, timeout=90)
    installed = Path(env["HOME"]) / ".local/bin/voiceless"
    require(installed.is_symlink() and output(installed, "--version", env=env, timeout=30) == f"voiceless {version}",
            "standalone user command does not start the selected release")
    rollback = subprocess.run([installed, "update", "--rollback"], env=env,
                              capture_output=True, text=True, timeout=30)
    require(rollback.returncode == 1 and "no previous standalone release" in rollback.stderr,
            f"first standalone install should have no rollback target: {rollback.stderr}")


def verify_bun(work, version, arch, root_package, leaf_package, env):
    bun_asset, sha = BUN_INPUTS[arch]
    bundle = work / f"{bun_asset}.zip"
    download(f"https://github.com/oven-sh/bun/releases/download/bun-v{BUN_VERSION}/{bundle.name}", bundle, sha)
    bun = work / "bun"
    with zipfile.ZipFile(bundle) as upstream:
        with upstream.open(f"{bun_asset}/bun") as source, bun.open("wb") as result:
            shutil.copyfileobj(source, result)
    bun.chmod(0o755)

    packages = {"voiceless": (package_json(version), root_package),
                leaf_name(arch): (package_json(version, arch), leaf_package)}
    tarballs = {f"/{name}/-/{archive.name}": archive for name, (_, archive) in packages.items()}
    manifests = {}

    class Registry(BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path in manifests:
                body = manifests[self.path]
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            elif self.path in tarballs:
                path = tarballs[self.path]
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(path.stat().st_size))
                self.end_headers()
                with path.open("rb") as archive:
                    shutil.copyfileobj(archive, self.wfile)
            else:
                self.send_error(404)

    bindir = work / "bun-bin"
    global_dir = work / "bun-global"
    with ThreadingHTTPServer(("127.0.0.1", 0), Registry) as server:
        registry = f"http://127.0.0.1:{server.server_port}"
        for name, (manifest, archive) in packages.items():
            with archive.open("rb") as tarball:
                integrity = hashlib.sha512()
                for chunk in iter(lambda: tarball.read(1024 * 1024), b""):
                    integrity.update(chunk)
            dist = {"tarball": f"{registry}/{name}/-/{archive.name}",
                    "integrity": "sha512-" + base64.b64encode(integrity.digest()).decode("ascii")}
            manifests[f"/{name}"] = json.dumps({"name": name, "dist-tags": {"latest": version},
                                                "versions": {version: dict(manifest, dist=dist)}}).encode()
            manifests[f"/{name}/latest"] = json.dumps(dict(manifest, dist=dist)).encode()
        config = work / "bunfig.toml"
        config.write_text(f'[install]\nregistry = "{registry}"\n', encoding="utf-8")
        bun_env = dict(env, BUN_INSTALL_GLOBAL_DIR=str(global_dir), BUN_INSTALL_BIN=str(bindir),
                       PATH=f"{work}:/usr/bin:/bin", npm_config_registry=registry)
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            subprocess.run([bun, "add", "--global", "--exact", "--ignore-scripts",
                            f"--config={config}", f"voiceless@{version}"], cwd=work,
                           env=bun_env, check=True, timeout=120)
            command = bindir / "voiceless"
            result = output(command, "update", env=bun_env, cwd=work, timeout=60)
            require(f"Voiceless {version} is current" in result,
                    "the installed package did not recognize its Bun update manager")
        finally:
            server.shutdown()
            worker.join()

    launcher = global_dir / "node_modules/voiceless/bin/voiceless"
    require(command.is_symlink() and command.resolve() == launcher.resolve(),
            "Bun global command does not resolve to the root package launcher")
    require(output(command, "--version", env=env, timeout=30) == f"voiceless {version}",
            "Bun-installed command reported an unexpected version")
    # Exercise a tree with only the nested dependency and an extra symlink hop.
    root = work / "nested" / "voiceless"
    unpack(root_package, root, "package/")
    unpack(leaf_package, root / "node_modules" / leaf_name(arch), "package/")
    link = work / "voiceless-link"
    link.symlink_to(root / "bin/voiceless")
    require(output(link, "--version", env=env, timeout=30) == f"voiceless {version}",
            "Bun launcher failed with the nested platform package")
    return command


def validate(work, candidates, arch):
    native_archives = list(candidates.glob(f"voiceless-v*-{target(arch)}.tar.gz"))
    require(len(native_archives) == 1, f"expected exactly one native archive for {arch}")
    native = native_archives[0]
    version = native.name.removeprefix("voiceless-v").removesuffix(f"-{target(arch)}.tar.gz")
    root_package = candidates / f"voiceless-{version}.tgz"
    leaf_package = candidates / f"{leaf_name(arch)}-{version}.tgz"
    tree, leaf, root = (work / directory for directory in ("native", "leaf", "root"))
    unpack(native, tree)
    unpack(leaf_package, leaf, "package/")
    unpack(root_package, root, "package/")
    expected = {"schema": 1, "version": version, "target": target(arch),
                "variant": "cpu", "files": inventory(tree)}
    manifest = tree / "release.json"
    require(json.loads(manifest.read_text()) == expected, "release.json does not match the native payload")
    libraries = set(PLATFORMS[arch]["libraries"])
    required = {"bin/voiceless", "LICENSE", "THIRD_PARTY_NOTICES.txt",
                "licenses/Sherpa-ONNX-LICENSE.txt", "licenses/ONNX-Runtime-LICENSE.txt",
                "licenses/ONNX-Runtime-ThirdPartyNotices.txt", *(f"lib/{name}" for name in libraries)}
    if arch == "x86_64":
        required.update(f"licenses/{name}-copyright.txt" for name in VULKAN_LICENSES)
    require(required <= expected["files"].keys(), "archive missing payload files or license texts")
    require({p.name for p in (tree / "lib").iterdir()} == libraries, "unexpected bundled library")
    require(os.access(tree / "bin/voiceless", os.X_OK), "native binary is not executable")
    require(json.loads((leaf / "package.json").read_text()) == package_json(version, arch),
            "incorrect npm platform package metadata")
    require(json.loads((root / "package.json").read_text()) == package_json(version),
            "incorrect npm root package metadata")
    leaf_files = inventory(leaf)
    leaf_files.pop("package.json")
    require(leaf_files == expected["files"] and (leaf / "release.json").read_bytes() == manifest.read_bytes()
            and os.access(leaf / "bin/voiceless", os.X_OK), "npm leaf differs from the native payload")
    require(inventory(root).keys() == {"bin/voiceless", "LICENSE", "package.json"}
            and os.access(root / "bin/voiceless", os.X_OK), "invalid npm root contents")
    env = clean_environment(work)
    verify_elf(tree, arch, env)
    require(output(tree / "bin/voiceless", "--version", env=env, timeout=30) == f"voiceless {version}",
            "native candidate reported an unexpected version")
    verify_standalone(tree / "bin/voiceless", native, version, env)
    bun_command = verify_bun(work, version, arch, root_package, leaf_package, env)
    verify_recognition(bun_command, work, env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidates", type=Path, help="directory containing candidate archives")
    args = parser.parse_args()
    os.environ["LC_ALL"] = "C"
    arch = platform.machine()
    require(platform.system() == "Linux" and arch in PLATFORMS, "requires native x86_64 or aarch64 Linux")
    require(output("getconf", "GNU_LIBC_VERSION") == f"glibc {GLIBC}",
            f"validate on the glibc {GLIBC} baseline (Ubuntu 22.04)")
    with tempfile.TemporaryDirectory(prefix="voiceless-release-validation-") as directory:
        validate(Path(directory), args.candidates.resolve(), arch)
    print(f"Validated native and npm release candidates for {target(arch)}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"release validation: {error}")
