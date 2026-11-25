#!/usr/bin/env python3
"""Build the native Voiceless release candidates, npm packages and the desktop package on the matching Ubuntu 22.04 architecture."""

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
from pathlib import Path, PurePosixPath


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
SHERPA_LIBRARIES = ("libsherpa-onnx-c-api.so", "libonnxruntime.so")
VULKAN_LIBRARY = "libvulkan.so.1"
VULKAN_SDK = "https://packages.lunarg.com/vulkan/1.4.313"
VULKAN_PACKAGES = (
    (
        "v/vulkan-loader/libvulkan1_1.4.313.0~rc1-1lunarg22.04-1_amd64.deb",
        "0fc9b0587059230743433cdbfd5723dd16b2c13b4a05f397ae45047033f4ddd5",
    ),
    (
        "v/vulkan-headers/vulkan-headers_1.4.313.0~rc1-1lunarg22.04-1_all.deb",
        "587b2d8e79416b394170ab61557c98765570cd153730f819a917866d78f45e1a",
    ),
    (
        "s/spirv-headers/spirv-headers_1.6.1+1.4.313.0~rc1-1lunarg22.04-1_all.deb",
        "9c72f6ccca5125cfb91bb0161d8841cc54059303cdb6c022d1f464c36029df7a",
    ),
    (
        "s/shaderc/shaderc_2025.2~rc1-1lunarg22.04-1_amd64.deb",
        "53a024e2939e13e86caa49930450f38fdfbc528e129d8ecffc142d7e5ff27d5c",
    ),
)
VULKAN_LICENSES = ("libvulkan1", "vulkan-headers", "spirv-headers")
PLATFORMS = {
    "x86_64": {
        "npm_cpu": "x64",
        "deb_arch": "amd64",
        "machine": "Advanced Micro Devices X86-64",
        "loader": "/lib64/ld-linux-x86-64.so.2",
        "rust_cpu": "x86-64",
        "c_arch": "x86-64",
        "ort_sha": "2cacf5084737b30f61a5c8a60efa0408b26f3e8ca6cc0dc312fe4d52d07724f9",
        "libraries": (*SHERPA_LIBRARIES, VULKAN_LIBRARY),
    },
    "aarch64": {
        "npm_cpu": "arm64",
        "deb_arch": "arm64",
        "machine": "AArch64",
        "loader": "/lib/ld-linux-aarch64.so.1",
        "rust_cpu": "generic",
        "c_arch": "armv8-a",
        "ort_sha": "c6a1971a8bfc8eaa07d48478d796245a747e75f510859574c9253dc7f1e9dc9a",
        "libraries": SHERPA_LIBRARIES,
    },
}
# System libraries any candidate may load beyond its own bundled ones.
SYSTEM_LIBRARIES = {"libc.so.6", "libm.so.6", "libpthread.so.0", "libdl.so.2", "librt.so.1",
                    "libstdc++.so.6", "libgcc_s.so.1", "libutil.so.1"}
# The desktop host's binary, Debian package, launcher and icon share one name:
# it is also the program name GTK reports, by which the launcher finds its
# window. The package keeps the host beside the runtime it was built with,
# which is the runtime the host starts.
DESKTOP = "voiceless-desktop"
DESKTOP_PREFIX = Path("usr/lib") / DESKTOP
# The system runtime of the glibc baseline, which the package does not bundle.
DESKTOP_BASE_DEPENDS = ("libc6 (>= 2.35)", "libgcc-s1", "libstdc++6")
# What the host must load from the system, with the packages declared for them.
DESKTOP_LINKED = {"libwebkit2gtk-4.1.so.0": "libwebkit2gtk-4.1-0", "libgtk-3.so.0": "libgtk-3-0"}
# The tray loads its indicator library only where a desktop has a tray host,
# so it is in no DT_NEEDED: the dependency is declared by hand, and the host
# names the libraries it tries.
DESKTOP_TRAY_DEPENDS = "libayatana-appindicator3-1 | libappindicator3-1"
DESKTOP_TRAY_PACKAGE = "libayatana-appindicator3-1"
DESKTOP_TRAY_LIBRARY = "libayatana-appindicator3.so.1"
DESKTOP_TRAY_NAMES = (b"libayatana-appindicator3.so.1", b"libappindicator3.so.1")


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


def cargo(*args, directory=ROOT / "rust", **kwargs):
    return output("rustup", "run", RUST, "cargo", *args, cwd=directory, **kwargs)


def app_package(metadata):
    return next(pkg for pkg in metadata["packages"] if Path(pkg["manifest_path"]) == ROOT / "rust/Cargo.toml")


def desktop_package(metadata):
    return next(pkg for pkg in metadata["packages"] if Path(pkg["manifest_path"]) == ROOT / "desktop/Cargo.toml")


def app_version():
    metadata = json.loads(cargo("metadata", "--locked", "--no-deps", "--format-version", "1"))
    version = app_package(metadata)["version"]
    require(re.fullmatch(r"(?:0|[1-9][0-9]*)(?:\.(?:0|[1-9][0-9]*)){2}", version),
            f"release candidates require a stable Cargo version, got {version}")
    return version


def build_console(work):
    source = ROOT / "frontend"
    frontend = work / "frontend"
    shutil.copytree(source / "src", frontend / "src")
    shutil.copytree(source / "scripts", frontend / "scripts")
    for name in ("index.html", "package.json", "package-lock.json", "tsconfig.json", "vite.config.ts"):
        shutil.copyfile(source / name, frontend / name)
    subprocess.run(["npm", "ci", "--ignore-scripts", "--no-audit", "--no-fund"], cwd=frontend, check=True)
    subprocess.run(["npm", "run", "build"], cwd=frontend, check=True)
    return frontend / "dist"


def build_environment(work, arch):
    env = os.environ.copy()
    for name in list(env):
        if name.startswith(("CARGO_TARGET_", "CARGO_PROFILE_", "CARGO_BUILD_", "RUSTC", "RUSTFLAGS",
                            "CARGO_ENCODED_RUSTFLAGS", "CC_", "CXX_", "AR_", "CFLAGS", "CPPFLAGS",
                            "CXXFLAGS", "LDFLAGS", "PKG_CONFIG", "LD_", "CPATH", "C_INCLUDE_PATH",
                            "CPLUS_INCLUDE_PATH", "LIBRARY_PATH", "SHERPA_ONNX", "SHERPA_ONNXRUNTIME",
                            "TRANSCRIBE_", "CMAKE_", "VULKAN_SDK", "DOCS_RS")):
            del env[name]
    abi = PLATFORMS[arch]
    env.update(
        LC_ALL="C", CARGO_TARGET_DIR=str(work / "cargo-target"),
        VOICELESS_CONSOLE_DIST=str(build_console(work)),
        CC="/usr/bin/gcc-11", CXX="/usr/bin/g++-11", AR="/usr/bin/ar",
        PKG_CONFIG="/usr/bin/pkg-config", CFLAGS=f"-march={abi['c_arch']}",
        CXXFLAGS=f"-march={abi['c_arch']}", LIBSQLITE3_SYS_USE_PKG_CONFIG="0", BZIP2_NO_PKG_CONFIG="1",
        RUSTFLAGS=f"-C target-cpu={abi['rust_cpu']} -C linker=/usr/bin/gcc-11 -C link-arg=-fuse-ld=bfd",
    )
    return env


def prepare_vulkan(work, payload, env):
    # Jammy omits glslc. Extract the pinned SDK without changing system packages.
    sdk = work / "vulkan-sdk"
    for relative, sha in VULKAN_PACKAGES:
        package = work / Path(relative).name
        download(f"{VULKAN_SDK}/pool/main/{relative}", package, sha)
        subprocess.run(["dpkg-deb", "--extract", package, sdk], check=True)

    include = sdk / "usr/include"
    lib = sdk / "usr/lib/x86_64-linux-gnu"
    linker_name = lib / "libvulkan.so"
    linker_name.symlink_to(VULKAN_LIBRARY)
    env["TRANSCRIBE_CMAKE_ARGS"] = " ".join([
        "-DTRANSCRIBE_X86_CONSERVATIVE=ON",
        # Keep the decoder from linking to optional libraries on the runner.
        "-DTRANSCRIBE_USE_SYSTEM_BLAS=OFF",
        f"-DVulkan_INCLUDE_DIR={include}",
        f"-DVulkan_LIBRARY={linker_name}",
        f"-DVulkan_GLSLC_EXECUTABLE={sdk / 'usr/bin/glslc'}",
        f"-DSPIRV-Headers_DIR={sdk / 'usr/share/cmake/SPIRV-Headers'}",
    ])
    env["CXXFLAGS"] += f" -isystem {include}"
    env["RUSTFLAGS"] += f" -L native={lib}"
    shutil.copyfile(lib / VULKAN_LIBRARY, payload / "lib" / VULKAN_LIBRARY)
    for component in VULKAN_LICENSES:
        shutil.copyfile(sdk / "usr/share/doc" / component / "copyright",
                        payload / "licenses" / f"{component}-copyright.txt")


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
            for name in SHERPA_LIBRARIES:
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

    for name in SHERPA_LIBRARIES:
        check_machine(libs / name, arch)
    return libs


def copy_cargo_licenses(metadata, built_ids, licenses, prefix=""):
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
        if package["name"] == "transcribe-cpp-sys":
            texts.add(root / "ggml/LICENSE")
            texts.update(root.glob("src/third_party/*/LICENSE"))
        files = []
        for source in sorted(texts):
            dest = licenses / label / source.relative_to(root)
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, dest)
            files.append(prefix + dest.relative_to(licenses.parent).as_posix())
        url = f"https://crates.io/crates/{package['name']}/{package['version']}"
        notices.append(f"{label} ({package['license'] or 'see source license'})\n{url}\n" + "\n".join(files))
    return "\n\n".join(notices)


def write_notices(payload, metadata, built_ids, arch):
    licenses = payload / "licenses"
    lines = ["Third-party notices for the Voiceless Linux release", "",
             f"Sherpa-ONNX {SHERPA} (Apache-2.0): {SHERPA_GITHUB}/tree/v{SHERPA}",
             f"ONNX Runtime {ORT} (MIT): {ORT_SOURCE}",
             f"ONNX Runtime binary distribution: {ORT_RELEASE}",
             "ONNX Runtime license and upstream notices are in licenses/.", "",
             f"C API origin: {SHERPA_RELEASE}/{SHERPA_X64}" if arch == "x86_64"
             else f"C API origin: {SHERPA_SOURCE} (CPU C API build with TTS disabled)",
             "Sherpa-ONNX component and Rust toolchain license texts and sources:", ""]
    if arch == "x86_64":
        lines.extend([f"Vulkan SDK packages: {VULKAN_SDK}",
                      *(f"licenses/{name}-copyright.txt" for name in VULKAN_LICENSES), ""])
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
    for name in SHERPA_LIBRARIES:
        shutil.copyfile(sherpa_libs / name, payload / "lib" / name)
    features = ["--no-default-features"]
    if arch == "x86_64":
        prepare_vulkan(work, payload, env)
        features.extend(["--features", "vulkan"])
    for name in PLATFORMS[arch]["libraries"]:
        path = payload / "lib" / name
        if output("patchelf", "--print-rpath", path) != "$ORIGIN":
            subprocess.run(["patchelf", "--set-rpath", "$ORIGIN", path], check=True)

    with (work / "cargo.jsonl").open("w+") as messages:
        subprocess.run(["rustup", "run", RUST, "cargo", "build", "--release", "--locked",
                        *features, "--bin", "voiceless", "--target", rust_target,
                        "--message-format=json-render-diagnostics"],
                       cwd=ROOT / "rust", env=env, stdout=messages, check=True)
        messages.seek(0)
        built_ids = {item["package_id"] for item in map(json.loads, messages)
                     if item["reason"] == "compiler-artifact"}
    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1",
                                "--filter-platform", rust_target, *features, env=env))
    require(app_package(metadata)["id"] in built_ids, "Cargo did not report the Voiceless build")

    binary = payload / "bin/voiceless"
    shutil.copyfile(Path(env["CARGO_TARGET_DIR"]) / rust_target / "release/voiceless", binary)
    binary.chmod(0o755)
    subprocess.run(["patchelf", "--set-rpath", "$ORIGIN/../lib", binary], check=True)
    subprocess.run(["strip", "--strip-debug", binary], check=True)
    check_machine(binary, arch)
    shutil.copyfile(ROOT / "LICENSE", payload / "LICENSE")
    write_notices(payload, metadata, built_ids, arch)
    # Existing standalone updaters require this manifest variant for the
    # CPU-compatible release lane, including optional Vulkan acceleration.
    write_json(payload / "release.json", {"schema": 1, "version": version, "target": target(arch),
                                         "variant": "cpu", "files": inventory(payload)})
    archive(payload, destination / f"voiceless-v{version}-{target(arch)}.tar.gz")
    write_json(payload / "package.json", package_json(version, arch))
    archive(payload, destination / f"{leaf_name(arch)}-{version}.tgz", "package/")
    return payload, env


def desktop_name(version, arch):
    return f"{DESKTOP}-v{version}-{target(arch)}.deb"


def desktop_config(version):
    """`desktop/tauri.conf.json`, the metadata the desktop package is built
    from, at the release version."""
    config = json.loads((ROOT / "desktop/tauri.conf.json").read_text(encoding="utf-8"))
    require(config["version"] == version, "desktop/tauri.conf.json and rust/Cargo.toml must declare one version")
    bundle, deb = config["bundle"], config["bundle"]["linux"]["deb"]
    require(all(isinstance(bundle.get(field), str) and bundle[field].strip()
                for field in ("publisher", "homepage", "category", "shortDescription", "longDescription")),
            "desktop/tauri.conf.json needs the publisher, homepage, category and descriptions of the bundle")
    require(all(isinstance(deb.get(field), str) and deb[field].strip()
                for field in ("section", "priority", "changelog", "desktopTemplate"))
            and isinstance(deb.get("depends"), list),
            "desktop/tauri.conf.json needs the depends, section, priority, changelog "
            "and desktopTemplate of the deb bundle")
    return config


def desktop_input(config, field):
    """A file the Debian bundle configuration names, relative to desktop/."""
    path = (ROOT / "desktop" / config["bundle"]["linux"]["deb"][field]).resolve()
    require(path.is_relative_to(ROOT) and path.is_file(), f"missing desktop bundle {field}: {path}")
    return path


def desktop_icons(config):
    """The bundle's icons by the hicolor size they are installed under."""
    icons = {}
    for icon in config["bundle"]["icon"]:
        match = re.fullmatch(r"icons/(\d+x\d+)\.png", icon)
        require(match and (ROOT / "desktop" / icon).is_file(), f"unsupported desktop bundle icon: {icon}")
        icons[match[1]] = ROOT / "desktop" / icon
    require(icons, "desktop/tauri.conf.json declares no icon")
    return icons


def loaded(path, arch):
    """The shared objects the loader gives an executable, as `(soname, file)`."""
    entries = re.findall(r"^\s*(\S+) => (\S+) \(0x[0-9a-f]+\)$",
                         output(PLATFORMS[arch]["loader"], "--list", path, env={"LC_ALL": "C"}), re.M)
    require(entries and all(Path(file).is_absolute() for _, file in entries),
            f"unresolved libraries in {path}: {entries}")
    return entries


def package_owner(path):
    """The installed package that owns a file, by dpkg's database, which may
    name it through the other of the merged /lib and /usr/lib."""
    # The loader may report a merged-/usr alias or a symlink maintained by
    # ldconfig. Query the owning package of the actual versioned file too.
    resolved = os.path.realpath(path)
    for candidate in dict.fromkeys([path, resolved, resolved.removeprefix("/usr"),
                                    "/usr" + resolved.removeprefix("/usr")]):
        result = subprocess.run(["dpkg-query", "--search", candidate], capture_output=True, text=True)
        if result.returncode == 1:
            continue
        result.check_returncode()
        owners = {match[1] for line in result.stdout.splitlines()
                  if (match := re.fullmatch(r"([a-z0-9][a-z0-9+.-]*)(?::[a-z0-9-]+)?: "
                                           + re.escape(candidate), line))}
        if len(owners) == 1:
            return owners.pop()
    raise ValueError(f"cannot identify the installed package owning {path}")


def desktop_depends(host, arch, declared):
    """`Depends` of the desktop package: the runtime of the glibc baseline, the
    dependencies declared in tauri.conf.json (among them the tray's), and the
    package of every other system library the host loads directly. The
    libraries bundled beside the host are the runtime's own."""
    linked = dict(loaded(host, arch))
    direct = set(re.findall(r"\(NEEDED\).*\[([^\]]+)\]", output("readelf", "--dynamic", "--wide", host)))
    # glibc's loader lists itself by absolute path, without a SONAME => path row.
    # libc6 supplies the interpreter even when it also appears in DT_NEEDED.
    direct.discard(Path(PLATFORMS[arch]["loader"]).name)
    require(direct <= linked.keys(), f"unresolved direct desktop libraries: {sorted(direct - linked.keys())}")
    names = {name.strip().split(" ", 1)[0] for entry in declared for name in entry.split("|")}
    for soname, package in DESKTOP_LINKED.items():
        require(soname in direct and package in names and package_owner(linked[soname]) == package,
                f"the desktop host must link {soname} from the declared package {package}")
    require(DESKTOP_TRAY_DEPENDS in declared, f"the desktop package must depend on {DESKTOP_TRAY_DEPENDS}")
    bundled = set(PLATFORMS[arch]["libraries"])
    owners = {package_owner(linked[name]) for name in direct - bundled - SYSTEM_LIBRARIES}
    return [*DESKTOP_BASE_DEPENDS, *declared, *sorted(owners - names)]


def check_tray(host):
    """The host names the indicator libraries it tries, and the package the
    dependency names first provides the one tried first."""
    content = host.read_bytes()
    require(all(name in content for name in DESKTOP_TRAY_NAMES), "the desktop host does not contain the tray")
    listing = subprocess.run(["dpkg-query", "--listfiles", DESKTOP_TRAY_PACKAGE], check=True,
                             capture_output=True, text=True).stdout
    require(DESKTOP_TRAY_LIBRARY in {PurePosixPath(line).name for line in listing.splitlines()},
            f"{DESKTOP_TRAY_PACKAGE} does not provide {DESKTOP_TRAY_LIBRARY}")


def desktop_description(config):
    """The package's description: the bundle's synopsis and its long text."""
    bundle = config["bundle"]
    return f"{bundle['shortDescription']}\n{bundle['longDescription']}"


def desktop_control(config, package, arch, depends, size):
    """The package's control file. Its fields come from where Tauri's own
    bundler takes them: the crate, and `bundle` in tauri.conf.json."""
    bundle, deb = config["bundle"], config["bundle"]["linux"]["deb"]
    # Tauri's deb takes Cargo `authors` over `bundle.publisher`: unset, they cannot disagree.
    require(not package["authors"] and re.fullmatch(r"[^<>]+ <[^<>\s@]+@[^<>\s@]+>", bundle["publisher"]),
            "the Debian maintainer is bundle.publisher, as `Name <email>`; Cargo authors must stay unset")
    synopsis, _, description = desktop_description(config).partition("\n")
    return "".join([
        f"Package: {DESKTOP}\nVersion: {package['version']}\nArchitecture: {PLATFORMS[arch]['deb_arch']}\n",
        f"Maintainer: {bundle['publisher']}\nInstalled-Size: {(size + 1023) // 1024}\n",
        f"Depends: {', '.join(depends)}\n",
        f"Section: {deb['section']}\nPriority: {deb['priority']}\nHomepage: {bundle['homepage']}\n",
        f"Description: {synopsis}\n", *(f" {line}\n" for line in description.splitlines()),
    ])


def desktop_copyright():
    """The package's copyright notice: the project's license, and where the
    notices of its third-party components are."""
    runtime, doc = DESKTOP_PREFIX.as_posix(), f"usr/share/doc/{DESKTOP}"
    return "\n".join([
        "Voiceless desktop host and its bundled runtime", "",
        f"{runtime}/ holds the Voiceless runtime and the desktop host, both under the MIT License:", "",
        (ROOT / "LICENSE").read_text(encoding="utf-8").rstrip("\n"), "",
        "Third-party components keep their own licenses and notices:", "",
        f"  the bundled runtime: {runtime}/THIRD_PARTY_NOTICES.txt and {runtime}/licenses/",
        f"  the desktop host: {doc}/THIRD_PARTY_NOTICES.txt and {doc}/licenses/", "",
    ])


def desktop_notices(metadata, built_ids, licenses):
    """Third-party notices of the desktop host: the crates compiled into it,
    and where the bundled runtime carries its own notices."""
    runtime, doc = DESKTOP_PREFIX.as_posix(), f"usr/share/doc/{DESKTOP}"
    return "\n\n".join([
        "Third-party notices for the Voiceless desktop host (Linux release)",
        f"The bundled runtime under {runtime}/ carries its own notices for Sherpa-ONNX,\n"
        f"ONNX Runtime, the Vulkan loader and the Rust toolchain: {runtime}/THIRD_PARTY_NOTICES.txt\n"
        f"and {runtime}/licenses/.",
        "GTK, WebKitGTK and the tray indicator are the distribution's: the package depends\n"
        "on their packages and redistributes none of them.",
        "Rust crates compiled into the host for this candidate (including build dependencies).\n"
        f"Their license texts are under {doc}/licenses/:",
        copy_cargo_licenses(metadata, built_ids, licenses, f"{doc}/"),
    ]) + "\n"


def build_desktop(work, destination, version, arch, payload, env):
    """The desktop host and the runtime just built, as one Debian package.
    The host is built like the runtime: same baseline, same console bundle."""
    config = desktop_config(version)
    deb = config["bundle"]["linux"]["deb"]
    rust_target = f"{arch}-unknown-linux-gnu"
    with (work / "desktop-cargo.jsonl").open("w+") as messages:
        subprocess.run(["rustup", "run", RUST, "cargo", "build", "--release", "--locked",
                        "--bin", DESKTOP, "--target", rust_target, "--message-format=json-render-diagnostics"],
                       cwd=ROOT / "desktop", env=env, stdout=messages, check=True)
        messages.seek(0)
        built_ids = {item["package_id"] for item in map(json.loads, messages)
                     if item["reason"] == "compiler-artifact"}
    metadata = json.loads(cargo("metadata", "--locked", "--format-version", "1",
                                "--filter-platform", rust_target, directory=ROOT / "desktop", env=env))
    package = desktop_package(metadata)
    require(package["id"] in built_ids and package["name"] == DESKTOP and package["version"] == version,
            "Cargo must build the desktop host at the core release version")

    # The runtime tree is the core payload just built, unchanged: the host goes beside it.
    tree = work / "desktop-package"
    runtime = tree / DESKTOP_PREFIX
    manifest = json.loads((payload / "release.json").read_text())
    for name in sorted([*manifest["files"], "release.json"]):
        (runtime / name).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(payload / name, runtime / name)
        (runtime / name).chmod(0o755 if (payload / name).stat().st_mode & 0o111 else 0o644)
    host = runtime / "bin" / DESKTOP
    shutil.copyfile(Path(env["CARGO_TARGET_DIR"]) / rust_target / "release" / DESKTOP, host)
    host.chmod(0o755)
    subprocess.run(["patchelf", "--set-rpath", "$ORIGIN/../lib", host], check=True)
    subprocess.run(["strip", "--strip-debug", host], check=True)
    check_machine(host, arch)
    depends = desktop_depends(host, arch, deb["depends"])
    check_tray(host)

    def install(source, target):
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
        target.chmod(0o644)

    share = tree / "usr/share"
    install(desktop_input(config, "desktopTemplate"), share / "applications" / f"{DESKTOP}.desktop")
    for size, icon in desktop_icons(config).items():
        install(icon, share / "icons/hicolor" / size / "apps" / f"{DESKTOP}.png")
    doc = share / "doc" / DESKTOP
    doc.mkdir(parents=True)
    (doc / "copyright").write_text(desktop_copyright(), encoding="utf-8")
    (doc / "THIRD_PARTY_NOTICES.txt").write_text(
        desktop_notices(metadata, built_ids, doc / "licenses"), encoding="utf-8")
    with (doc / "changelog.gz").open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as gz:
            gz.write(desktop_input(config, "changelog").read_bytes())
    # Links, not copies: the core's `$ORIGIN/../lib` stays beside it, and the
    # host keeps its file name, which GTK reports as the program name.
    (tree / "usr/bin").mkdir(parents=True)
    (tree / "usr/bin" / DESKTOP).symlink_to(f"../lib/{DESKTOP}/bin/{DESKTOP}")
    (tree / "usr/bin" / "voiceless").symlink_to(f"../lib/{DESKTOP}/bin/voiceless")

    paths = [tree, *sorted(tree.rglob("*"))]
    for path in paths:
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644)
    files = sorted((path for path in paths if path.is_file() and not path.is_symlink()),
                   key=lambda path: path.relative_to(tree).as_posix())
    size = sum(path.stat().st_size for path in files) + 4096 * sum(1 for path in paths if path.is_dir())
    control = tree / "DEBIAN"
    control.mkdir()
    (control / "control").write_text(desktop_control(config, package, arch, depends, size), encoding="utf-8")
    (control / "md5sums").write_text(
        "".join(f"{hashlib.md5(path.read_bytes(), usedforsecurity=False).hexdigest()}  "
                f"{path.relative_to(tree).as_posix()}\n" for path in files), encoding="ascii")
    for path in control.iterdir():
        path.chmod(0o644)
    # dpkg-deb writes root ownership and, with SOURCE_DATE_EPOCH, one timestamp.
    subprocess.run(["dpkg-deb", "--build", "--root-owner-group", "-Zxz", "-z6", tree,
                    destination / desktop_name(version, arch)],
                   env=dict(os.environ, SOURCE_DATE_EPOCH="0"), check=True)


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
    desktop_config(version)
    require((ROOT / "desktop/Cargo.lock").is_file(),
            "desktop/Cargo.lock is missing: track it beside desktop/Cargo.toml, as the flake does")
    if os.environ.get("GITHUB_REF_TYPE") == "tag":
        require(os.environ.get("GITHUB_REF_NAME") == f"v{version}", "Git tag must match the Cargo version")
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="voiceless-release-") as directory:
        work = Path(directory)
        payload, env = build_native(work, args.output, version, arch)
        build_desktop(work, args.output, version, arch, payload, env)
        build_root(work, args.output, version)
    print(f"Wrote release candidates for {target(arch)} to {args.output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"release: {error}")
