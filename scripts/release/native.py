#!/usr/bin/env python3
"""Build native Voiceless release candidates on the matching Ubuntu 22.04 architecture."""

import argparse
import gzip
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
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
RUST = "1.99.1"
GLIBC = "2.35"
SHERPA = "1.13.3"
ORT = "1.24.4"
SHERPA_GITHUB = "https://github.com/k2-fsa/sherpa-onnx"
SHERPA_RELEASE = f"{SHERPA_GITHUB}/releases/download/v{SHERPA}"
ORT_RELEASE = f"https://github.com/csukuangfj/onnxruntime-libs/releases/download/v{ORT}"
ORT_SOURCE = "https://github.com/microsoft/onnxruntime/tree/2d924974ef147392ced8409d36bd6d2e7fcc8a74"
SHERPA_SOURCE = f"https://codeload.github.com/k2-fsa/sherpa-onnx/tar.gz/refs/tags/v{SHERPA}"
SHERPA_SOURCE_SHA = "01db47e87078d5f8fd0e163f9168436ad1e2ad783d61af51da44f117cae3e554"
SHERPA_X64 = f"sherpa-onnx-v{SHERPA}-linux-x64-shared-no-tts-lib.tar.bz2"
SHERPA_X64_SHA = "36e2d6be206b60e352aa3f495c3f37d91081fe3c803630411d7bc99d4d3c8480"
LIBRARIES = ("libsherpa-onnx-c-api.so", "libonnxruntime.so")
PLATFORMS = {
    "x86_64": {
        "npm_cpu": "x64",
        "machine": "Advanced Micro Devices X86-64",
        "loader": "/lib64/ld-linux-x86-64.so.2",
        "rust_cpu": "x86-64",
        "c_arch": "x86-64",
        "ort_sha": "2cacf5084737b30f61a5c8a60efa0408b26f3e8ca6cc0dc312fe4d52d07724f9",
    },
    "aarch64": {
        "npm_cpu": "arm64",
        "machine": "AArch64",
        "loader": "/lib/ld-linux-aarch64.so.1",
        "rust_cpu": "generic",
        "c_arch": "armv8-a",
        "ort_sha": "c6a1971a8bfc8eaa07d48478d796245a747e75f510859574c9253dc7f1e9dc9a",
    },
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def output(*args, **kwargs):
    return subprocess.check_output(args, text=True, **kwargs).strip()


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def download(url, path, expected):
    subprocess.run(
        ["curl", "--fail", "--location", "--silent", "--show-error", "--retry", "3",
         "--proto", "=https", "--proto-redir", "=https", "--output", path, url],
        check=True,
    )
    require(digest(path) == expected, f"SHA-256 mismatch for {url}")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def inventory(tree):
    return {
        path.relative_to(tree).as_posix(): digest(path)
        for path in sorted(tree.rglob("*"))
        if path.is_file() and path != tree / "release.json"
    }


def archive(tree, destination, prefix=""):
    # USTAR emits ordinary regular file headers. All payload paths are relative.
    with destination.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tar:
            for path in sorted(tree.rglob("*")):
                require(not path.is_symlink(), f"unexpected symlink: {path}")
                if path.is_dir():
                    continue
                require(path.is_file(), f"unexpected file type: {path}")
                name = prefix + path.relative_to(tree).as_posix()
                entry = tarfile.TarInfo(name)
                entry.mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
                entry.size = path.stat().st_size
                with path.open("rb") as source:
                    tar.addfile(entry, source)


def target(arch):
    return f"linux-{arch}-gnu"


def leaf_name(arch):
    return f"voiceless-{target(arch)}"


def package_json(version, arch=None):
    package = {
        "name": "voiceless" if arch is None else leaf_name(arch),
        "version": version,
        "description": "Fast realtime voice input for Linux",
        "license": "MIT",
        "repository": {"type": "git", "url": "git+https://github.com/Vitus213/voiceless.git"},
        "os": ["linux"],
        "libc": ["glibc"],
    }
    if arch is None:
        package["bin"] = {"voiceless": "bin/voiceless"}
        package["optionalDependencies"] = {leaf_name(item): version for item in PLATFORMS}
    else:
        package["cpu"] = [PLATFORMS[arch]["npm_cpu"]]
    return package


def cargo(*args, **kwargs):
    return output("rustup", "run", RUST, "cargo", *args, cwd=ROOT / "rust", **kwargs)


def app_package(metadata):
    return next(pkg for pkg in metadata["packages"] if Path(pkg["manifest_path"]) == ROOT / "rust/Cargo.toml")


def app_version():
    metadata = json.loads(cargo("metadata", "--locked", "--no-deps", "--format-version", "1"))
    version = app_package(metadata)["version"]
    require(re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){2}", version),
            f"release candidates require a stable Cargo version, got {version}")
    return version


def build_environment(work, arch):
    env = os.environ.copy()
    for name in list(env):
        if name.startswith(("CARGO_TARGET_", "CARGO_PROFILE_", "CARGO_BUILD_", "RUSTC", "RUSTFLAGS",
                            "CARGO_ENCODED_RUSTFLAGS", "CC_", "CXX_", "AR_", "CFLAGS", "CPPFLAGS",
                            "CXXFLAGS", "LDFLAGS", "PKG_CONFIG", "LD_", "CPATH", "C_INCLUDE_PATH",
                            "CPLUS_INCLUDE_PATH", "LIBRARY_PATH", "SHERPA_ONNX", "SHERPA_ONNXRUNTIME",
                            "DOCS_RS")):
            del env[name]
    abi = PLATFORMS[arch]
    env.update(
        LC_ALL="C", CARGO_TARGET_DIR=str(work / "cargo-target"),
        CC="/usr/bin/gcc-11", CXX="/usr/bin/g++-11", AR="/usr/bin/ar",
        PKG_CONFIG="/usr/bin/pkg-config", CFLAGS=f"-march={abi['c_arch']}",
        CXXFLAGS=f"-march={abi['c_arch']}", LIBSQLITE3_SYS_USE_PKG_CONFIG="0", BZIP2_NO_PKG_CONFIG="1",
        RUSTFLAGS=f"-C target-cpu={abi['rust_cpu']} -C linker=/usr/bin/gcc-11 -C link-arg=-fuse-ld=bfd",
    )
    return env


def prepare_onnx(work, arch, licenses):
    machine = "x64" if arch == "x86_64" else "aarch64"
    stem = f"onnxruntime-linux-{machine}-glibc2_17-Release-{ORT}"
    package = work / f"{stem}.zip"
    download(f"{ORT_RELEASE}/{package.name}", package, PLATFORMS[arch]["ort_sha"])
    with zipfile.ZipFile(package) as upstream:
        # The exact upstream bundle supplies both the C headers and its license texts.
        upstream.extractall(work)
    runtime = work / stem
    require((runtime / "GIT_COMMIT_ID").read_text().strip() == ORT_SOURCE.rsplit("/", 1)[1],
            "ONNX Runtime source commit does not match the release pin")
    shutil.copyfile(runtime / "LICENSE", licenses / "ONNX-Runtime-LICENSE.txt")
    shutil.copyfile(runtime / "ThirdPartyNotices.txt", licenses / "ONNX-Runtime-ThirdPartyNotices.txt")
    return runtime


def check_machine(path, arch):
    header = output("readelf", "--file-header", "--wide", path)
    require(re.search(r"^\s*Class:\s*ELF64\s*$", header, re.M) and
            re.search(r"^\s*Data:.*little endian\s*$", header, re.M) and
            re.search(rf"^\s*Machine:\s*{PLATFORMS[arch]['machine']}\s*$", header, re.M),
            f"incorrect ELF architecture for {path}: expected {arch} little-endian ELF64")


def prepare_sherpa(work, arch, runtime, env):
    if arch == "x86_64":
        package = work / SHERPA_X64
        download(f"{SHERPA_RELEASE}/{package.name}", package, SHERPA_X64_SHA)
        libs = work / package.name.removesuffix(".tar.bz2") / "lib"
        libs.mkdir(parents=True)
        with tarfile.open(package, "r:bz2") as upstream:
            for name in LIBRARIES:
                member = upstream.getmember(f"{libs.parent.name}/lib/{name}")
                require(member.isfile(), f"expected a regular upstream member: {member.name}")
                with upstream.extractfile(member) as source, (libs / name).open("wb") as dest:
                    shutil.copyfileobj(source, dest)
        require(digest(libs / "libonnxruntime.so") == digest(runtime / "lib/libonnxruntime.so"),
                "the Sherpa archive's ONNX Runtime differs from the pinned license bundle")
    else:
        # The generic upstream aarch64 shared archive enables TTS and links eSpeak NG.
        # Build the same ASR API from the pinned sources with TTS disabled.
        package = work / "sherpa-onnx.tar.gz"
        download(SHERPA_SOURCE, package, SHERPA_SOURCE_SHA)
        subprocess.run(["tar", "-xzf", package, "-C", work], check=True)
        build_dir = work / "sherpa-build"
        subprocess.run(
            ["cmake", "-S", work / f"sherpa-onnx-{SHERPA}", "-B", build_dir,
             "-DCMAKE_BUILD_TYPE=Release", "-DBUILD_SHARED_LIBS=ON",
             "-DSHERPA_ONNX_ENABLE_C_API=ON", "-DSHERPA_ONNX_ENABLE_TTS=OFF",
             "-DSHERPA_ONNX_ENABLE_GPU=OFF", "-DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF",
             "-DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF", "-DSHERPA_ONNX_ENABLE_BINARY=OFF",
             "-DSHERPA_ONNX_BUILD_C_API_EXAMPLES=OFF",
             "-DSHERPA_ONNX_USE_PRE_INSTALLED_ONNXRUNTIME_IF_AVAILABLE=OFF",
             f"-DFETCHCONTENT_SOURCE_DIR_ONNXRUNTIME={runtime}"],
            env=env, check=True,
        )
        subprocess.run(["cmake", "--build", build_dir, "--target", "sherpa-onnx-c-api", "--parallel", "2"],
                       env=env, check=True)
        libs = build_dir / "lib"
        shutil.copyfile(runtime / "lib/libonnxruntime.so", libs / "libonnxruntime.so")

    for name in LIBRARIES:
        check_machine(libs / name, arch)
    return libs


def copy_cargo_licenses(metadata, built_ids, licenses):
    notices = []
    license_name = re.compile(r"^(?:licen[cs]e|copying|copyright|notice|third[-_ ]?party)", re.I)
    for package in sorted(metadata["packages"], key=lambda pkg: (pkg["name"], pkg["version"])):
        if package["id"] not in built_ids or package["source"] is None:
            continue
        label = f"{package['name']}-{package['version']}"
        root = Path(package["manifest_path"]).parent
        texts = {item for item in root.iterdir() if item.is_file() and license_name.match(item.name)}
        if package["license_file"]:
            texts.add(root / package["license_file"])
        if package["name"] == "bzip2-sys":
            texts.update(root.glob("bzip2-*/LICENSE"))
        if package["name"] == "ring" and (root / "third_party").is_dir():
            texts.update(item for item in (root / "third_party").rglob("*")
                         if item.is_file() and license_name.match(item.name))
        files = []
        for source in sorted(texts):
            dest = licenses / label / source.relative_to(root)
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, dest)
            files.append(dest.relative_to(licenses.parent).as_posix())
        url = f"https://crates.io/crates/{package['name']}/{package['version']}"
        notices.append(f"{label} ({package['license'] or 'see source license'})\n{url}\n" + "\n".join(files))
    return "\n\n".join(notices)


def write_notices(payload, metadata, built_ids, arch):
    licenses = payload / "licenses"
    lines = ["Third-party notices for the Voiceless Linux CPU release", "",
             f"Sherpa-ONNX {SHERPA} (Apache-2.0): {SHERPA_GITHUB}/tree/v{SHERPA}",
             f"ONNX Runtime {ORT} (MIT): {ORT_SOURCE}",
             f"ONNX Runtime binary distribution: {ORT_RELEASE}",
             "ONNX Runtime license and upstream notices are in licenses/.", "",
             f"C API origin: {SHERPA_RELEASE}/{SHERPA_X64}" if arch == "x86_64"
             else f"C API origin: {SHERPA_SOURCE} (CPU C API build with TTS disabled)",
             "Sherpa-ONNX component and Rust toolchain license texts and sources:", ""]
    for row in (ROOT / "scripts/release/source-licenses.tsv").read_text().splitlines():
        if not row or row.startswith("#"):
            continue
        name, sha, url, source = row.split("\t")
        download(url, licenses / name, sha)
        lines.append(f"licenses/{name}\n{source}")
    lines.extend(["", "Bundled SQLite is public-domain software. https://sqlite.org/copyright.html",
                  "", "Cargo dependencies compiled for this candidate (including build dependencies):", "",
                  copy_cargo_licenses(metadata, built_ids, licenses)])
    (payload / "THIRD_PARTY_NOTICES.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")


def build_native(work, destination, version, arch):
    rust_target = f"{arch}-unknown-linux-gnu"
    payload = work / "payload"
    for directory in ("bin", "lib", "licenses"):
        (payload / directory).mkdir(parents=True)
    env = build_environment(work, arch)
    runtime = prepare_onnx(work, arch, payload / "licenses")
    sherpa_libs = prepare_sherpa(work, arch, runtime, env)
    env["SHERPA_ONNX_LIB_DIR"] = str(sherpa_libs)
    for name in LIBRARIES:
        path = payload / "lib" / name
        shutil.copyfile(sherpa_libs / name, path)
        if output("patchelf", "--print-rpath", path) != "$ORIGIN":
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", path], check=True)

    with (work / "cargo.jsonl").open("w+") as messages:
        subprocess.run(["rustup", "run", RUST, "cargo", "build", "--release", "--locked",
                        "--no-default-features", "--bin", "voiceless", "--target", rust_target,
                        "--message-format=json-render-diagnostics"],
                       cwd=ROOT / "rust", env=env, stdout=messages, check=True)
        messages.seek(0)
        built_ids = {item["package_id"] for item in map(json.loads, messages)
                     if item["reason"] == "compiler-artifact"}
    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1",
                                "--filter-platform", rust_target, env=env))
    require(app_package(metadata)["id"] in built_ids, "Cargo did not report the Voiceless build")

    binary = payload / "bin/voiceless"
    shutil.copyfile(Path(env["CARGO_TARGET_DIR"]) / rust_target / "release/voiceless", binary)
    binary.chmod(0o755)
    subprocess.run(["patchelf", "--set-rpath", "$ORIGIN/../lib", binary], check=True)
    subprocess.run(["strip", "--strip-debug", binary], check=True)
    check_machine(binary, arch)
    shutil.copyfile(ROOT / "LICENSE", payload / "LICENSE")
    write_notices(payload, metadata, built_ids, arch)
    write_json(payload / "release.json", {"schema": 1, "version": version, "target": target(arch),
                                         "variant": "cpu", "files": inventory(payload)})
    archive(payload, destination / f"voiceless-v{version}-{target(arch)}.tar.gz")
    write_json(payload / "package.json", package_json(version, arch))
    archive(payload, destination / f"{leaf_name(arch)}-{version}.tgz", "package/")


def build_root(work, destination, version):
    root = work / "npm-root"
    (root / "bin").mkdir(parents=True)
    launcher = root / "bin/voiceless"
    shutil.copyfile(ROOT / "scripts/release/voiceless", launcher)
    launcher.chmod(0o755)
    shutil.copyfile(ROOT / "LICENSE", root / "LICENSE")
    write_json(root / "package.json", package_json(version))
    archive(root, destination / f"voiceless-{version}.tgz", "package/")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="directory for release and npm candidate archives")
    args = parser.parse_args()
    # Stable diagnostics for the readelf output parsed by the producer and validator.
    os.environ["LC_ALL"] = "C"
    arch = platform.machine()
    require(platform.system() == "Linux" and arch in PLATFORMS, "requires native x86_64 or aarch64 Linux")
    require(output("getconf", "GNU_LIBC_VERSION") == f"glibc {GLIBC}",
            f"build on the glibc {GLIBC} baseline (Ubuntu 22.04)")
    version = app_version()
    if os.environ.get("GITHUB_REF_TYPE") == "tag":
        require(os.environ.get("GITHUB_REF_NAME") == f"v{version}", "Git tag must match the Cargo version")
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="voiceless-release-") as directory:
        work = Path(directory)
        build_native(work, args.output, version, arch)
        build_root(work, args.output, version)
    print(f"Wrote release candidates for {target(arch)} to {args.output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"release: {error}")
