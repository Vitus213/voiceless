#!/usr/bin/env python3
"""Validate native, npm and desktop candidates on the matching build architecture."""

import argparse
import base64
import gzip
import hashlib
import io
import json
import os
import platform
import re
import select
import shutil
import signal
import sqlite3
import struct
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import zipfile
from contextlib import ExitStack
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath
from urllib.error import HTTPError
from urllib.request import ProxyHandler, Request, build_opener

from native import (
    DESKTOP, DESKTOP_LINKED, DESKTOP_PREFIX, DESKTOP_TRAY_PACKAGE, GLIBC, PLATFORMS, SHERPA_GITHUB,
    SYSTEM_LIBRARIES, VULKAN_LICENSES, check_machine, check_tray, desktop_copyright, desktop_config,
    desktop_depends, desktop_description, desktop_icons, desktop_input, desktop_name, digest, download,
    inventory, leaf_name, output, package_json, require, target,
)

MODEL = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2025-09-09"
MODEL_SHA = "7305f7905bfcf77fa0b39388a313f3da35c68d971661a65475b56fb2162c8e63"
BUN_VERSION = "1.4.5"
BUN_INPUTS = {
    "x86_64": ("bun-linux-x64-baseline", "dcb23a18a1c2fff12d57bcef696dbbb42eb67fde29f43816ecb595459a24c463"),
    "aarch64": ("bun-linux-aarch64", "18656a3bf0ea5641f5796ae4213b75255579a70a0fded2f337933785568cdf84"),
}
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


def check_symbols(path):
    """No glibc, libstdc++ or C++ ABI symbol newer than the baseline, and no
    x86-64 instruction set above it."""
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
        check_symbols(path)
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
        "XDG_STATE_HOME": str(home / "state"), "DBUS_SESSION_BUS_ADDRESS": f"unix:path={home}/bus",
        "DO_NOT_TRACK": "1", "CI": "1",
    }


def recognition_fixture(work, env):
    """The SenseVoice model with a short English clip, and a config that uses
    the model."""
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
    return work / "speech.wav"


def verify_recognition(binary, speech, env):
    """`binary` transcribes the clip with the local model."""
    result = json.loads(output(binary, "transcribe", speech, "--backend", "local", "--json",
                               env=env, timeout=90))
    require(result["type"] == "transcript" and result["backend"] == "local" and result["text"].strip(),
            f"SenseVoice produced no speech transcript: {result}")
    print(f"SenseVoice fixture transcribed by {binary}", flush=True)


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


def verify_previous(work, candidates, arch, previous, checksum):
    """Upgrade a real 0.1.3 installation offline and reopen its data with the old CLI."""
    require(digest(previous) == checksum, "previous archive SHA-256 mismatch")
    work.mkdir()
    env = clean_environment(work)
    archive, = candidates.glob(f"voiceless-v*-{target(arch)}.tar.gz")
    candidate = work.parent / "native/bin/voiceless"
    output(candidate, "install", previous, "--sha256", checksum, env=env, timeout=120)
    command = Path(env["HOME"]) / ".local/bin/voiceless"
    old = command.resolve(strict=True)
    require(output(command, "--version", env=env) == "voiceless 0.1.3", "expected the 0.1.3 baseline")
    config = Path(env["XDG_CONFIG_HOME"]) / "voiceless/config.toml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text("# baseline settings\n", encoding="utf-8")
    output(command, "history", "list", env=env)
    database = Path(env["XDG_DATA_HOME"]) / "voiceless/history.sqlite3"
    with sqlite3.connect(database) as db:
        db.execute("INSERT INTO recognition_history(id, created_at, raw_text, final_text, status, character_count) "
                   "VALUES ('baseline', '2026-01-01T00:00:00Z', 'original', 'original', 'completed', 8)")
    db.close()
    before = json.loads(output(command, "history", "list", env=env))
    output(candidate, "install", archive, "--sha256", digest(archive), env=env, timeout=120)
    new = command.resolve(strict=True)
    require(new != old, "expected an upgrade from 0.1.3 to a newer stable candidate")
    records = json.loads(output(command, "history", "list", env=env))["records"]
    require(records[0]["final_text"] == "original", "candidate failed to migrate the baseline row")
    config.write_text("# candidate settings\n", encoding="utf-8")
    output(command, "history", "delete", "--all", env=env)
    notice = old.parent.parent / "THIRD_PARTY_NOTICES.txt"
    original = notice.read_bytes()
    notice.write_bytes(original + b"damaged")
    failed = subprocess.run([command, "update", "--rollback"], env=env, capture_output=True, text=True)
    require(failed.returncode == 1 and "SHA-256 mismatch" in failed.stderr and command.resolve() == new,
            f"damaged previous payload changed the activation: {failed.stderr}")
    notice.write_bytes(original)
    report = output(command, "update", "--rollback", env=env, timeout=120)
    require(command.resolve() == old, "rollback did not reactivate the previous release")
    snapshot = re.search(r"update --restore ([a-zA-Z0-9-]+)` returns to the snapshot", report)
    require(snapshot, f"rollback omitted the pre-upgrade snapshot: {report}")
    output(new, "update", "--restore", snapshot[1], env=env, timeout=120)
    require(config.read_text() == "# baseline settings\n" and
            json.loads(output(command, "history", "list", env=env)) == before,
            "0.1.3 could not reopen its original settings and history after rollback")
    print("Verified offline upgrade, damaged rollback rejection and restoration to 0.1.3", flush=True)


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


def start(command, env, *arguments):
    """A process without a terminal, its output lines collected while it runs."""
    process = subprocess.Popen([command, *arguments], env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    lines = {"stdout": [], "stderr": []}
    for stream in lines:
        threading.Thread(target=lines[stream].extend,
                         args=(iter(getattr(process, stream).readline, ""),), daemon=True).start()
    return process, lines


def announce(process, lines, pattern, timeout):
    """What the first output line matching `pattern` says, or why nothing does."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and process.poll() is None:
        for line in lines["stdout"] + lines["stderr"]:
            match = pattern.search(line)
            if match:
                return match[1]
        time.sleep(0.1)
    raise ValueError(f"{Path(process.args[0]).name} reported no console within {timeout} s:\n"
                     + "".join((lines["stdout"] + lines["stderr"])[-12:]))


def stop(process):
    """End a process that was started to be looked at."""
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def read_line(file, timeout):
    """The next line of a pipe, or an empty string when none comes in time."""
    ready, _, _ = select.select([file], [], [], timeout)
    return file.readline() if ready else ""


def running(executable):
    """The process ids running `executable`, by the kernel's own links."""
    target, pids = executable.resolve(), []
    for entry in Path("/proc").iterdir():
        if entry.name.isdigit():
            try:
                if Path(os.readlink(entry / "exe")) == target:
                    pids.append(int(entry.name))
            except OSError:
                continue
    return pids


def stop_running(executable):
    for pid in running(executable):
        os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    while running(executable) and time.monotonic() < deadline:
        time.sleep(0.1)
    for pid in running(executable):
        os.kill(pid, signal.SIGKILL)


def http_get(url, headers=None):
    """`(status, headers, body)` of a loopback GET: no proxy, no redirects."""
    try:
        response = build_opener(ProxyHandler({})).open(Request(url, headers=headers or {}), timeout=10)
    except HTTPError as error:
        response = error
    with response:
        require(response.url == url, f"the console at {url} redirected to {response.url}")
        return response.status, response.headers, response.read()


def check_console(url):
    """The embedded console at `url`: its page with the token, both assets,
    and an API answer that only the token gets."""
    status, headers, body = http_get(url)
    require(status == 200 and headers["Content-Type"].startswith("text/html")
            and headers["Cache-Control"] == "no-store", f"the console page is not served: {status}")
    page = body.decode("utf-8")
    token = re.search(r'<meta name="voiceless-token" content="([0-9a-f]{48})">', page)
    require(token, "the console page does not carry its token")
    for asset, kind in (("app.js", "text/javascript"), ("style.css", "text/css")):
        require(f'"/{asset}"' in page, f"the console page does not reference {asset}")
        status, headers, body = http_get(f"{url}{asset}")
        require(status == 200 and headers["Content-Type"].startswith(f"{kind}; charset=utf-8")
                and len(body) > 1000, f"the embedded {asset} is not served: {status}")
    status, _, _ = http_get(f"{url}api/totals")
    require(status == 403, f"the console API answered without the token: {status}")
    status, _, body = http_get(f"{url}api/totals", {"X-Voiceless-Token": token[1]})
    totals = json.loads(body)
    require(status == 200 and isinstance(totals.get("records"), int)
            and isinstance(totals.get("characters"), int), f"the console API did not answer: {status}")


def verify_web(binary, env):
    """`voiceless web` from the release tree serves the embedded console where
    there is no Node, Bun or display."""
    require(not any(shutil.which(name, path=env["PATH"]) for name in ("node", "bun"))
            and not {"DISPLAY", "WAYLAND_DISPLAY"} & env.keys(),
            "the validation environment has a JavaScript runtime or a display")
    process, lines = start(binary, env, "web", "--port", "0", "--no-open")
    try:
        url = announce(process, lines, re.compile(r"Voiceless console: (http://127\.0\.0\.1:\d+/)"), 30)
        check_console(url)
        print(f"The embedded console is served at {url}", flush=True)
    finally:
        stop(process)


def parse_control(text):
    """The fields of a Debian control file, continuation lines joined."""
    fields, order = {}, []
    for line in text.splitlines():
        if line[:1] in (" ", "\t"):
            require(order and line == line.rstrip(), f"malformed control line: {line!r}")
            fields[order[-1]] += "\n" + line[1:]
            continue
        key, separator, value = line.partition(": ")
        require(separator and value == value.strip() and key not in fields,
                f"malformed control line: {line!r}")
        fields[key] = value
        order.append(key)
    return fields


def package_members(archive):
    """The files under DEBIAN/ of a package, read with dpkg's own extractor."""
    listing = subprocess.check_output(["dpkg-deb", "--ctrl-tarfile", archive])
    with tarfile.open(fileobj=io.BytesIO(listing), mode="r:*") as tar:
        # dpkg-deb writes the archive's root directory beside the files.
        members = [member for member in tar.getmembers() if not member.isdir()]
        require(all(member.isfile() for member in members), "a control member is not a regular file")
        return {PurePosixPath(member.name).as_posix(): tar.extractfile(member).read().decode("utf-8")
                for member in members}


def package_inventory(root):
    """An extracted package tree: the digests of its files and where its links point."""
    files, links = {}, {}
    for path in sorted(root.rglob("*")):
        if path.is_dir() and not path.is_symlink():
            continue
        name = path.relative_to(root).as_posix()
        if path.is_symlink():
            links[name] = os.readlink(path)
        else:
            require(path.is_file(), f"unexpected file type in the desktop package: {name}")
            files[name] = digest(path)
    return files, links


def desktop_entry(text):
    """The fields of the launcher's one group, `[Desktop Entry]`."""
    lines = text.splitlines()
    require(lines[:1] == ["[Desktop Entry]"] and not any(line.startswith("[") for line in lines[1:]),
            "the launcher is not one [Desktop Entry] group")
    return dict(line.split("=", 1) for line in lines[1:] if line.strip())


def resolves_bundled(binary, runtime, arch, env, libraries):
    """The loader gives `binary` each of `libraries` from the runtime beside it."""
    resolved = output(PLATFORMS[arch]["loader"], "--list", binary, env=env)
    for name in sorted(libraries):
        match = re.search(rf"^\s*{re.escape(name)} => (.+) \(0x[0-9a-f]+\)$", resolved, re.M)
        require(match and Path(match.group(1)).resolve() == (runtime / "lib" / name).resolve(),
                f"the system loader did not give {binary.name} the bundled {name}")


def verify_host(host, runtime, arch, env):
    """The desktop host's ELF: built for the baseline like the runtime, linking
    WebKitGTK and GTK, and finding the bundled libraries beside itself."""
    loader = PLATFORMS[arch]["loader"]
    check_machine(host, arch)
    check_symbols(host)
    require(re.findall(r"Requesting program interpreter: ([^\]]+)\]",
                       output("readelf", "--program-headers", "--wide", host)) == [loader],
            "unexpected ELF interpreter in the desktop host")
    dynamic = output("readelf", "--dynamic", "--wide", host)
    require(re.findall(r"\((?:RPATH|RUNPATH)\).*\[([^\]]+)\]", dynamic) == ["$ORIGIN/../lib"],
            "the desktop host does not look for libraries beside the runtime")
    needed = set(re.findall(r"\(NEEDED\).*\[([^\]]+)\]", dynamic))
    require(set(DESKTOP_LINKED) <= needed, "the desktop host does not link WebKitGTK and GTK")
    print(output(loader, "--list", host, env=env), flush=True)
    resolves_bundled(host, runtime, arch, env, needed & set(PLATFORMS[arch]["libraries"]))


def windows(display, env):
    """The X windows titled Voiceless."""
    return re.findall(r'^\s*(0x[0-9a-f]+) "Voiceless": ',
                      output("xwininfo", "-display", display, "-root", "-tree", env=env), re.M)


def launch_desktop(root, work, entry, runtime):
    """Start the installed host the way the launcher does, on a private X server
    and session bus: it opens one window over the console of the runtime it
    starts beside itself, and a second start hands that window back."""
    (work / "launch").mkdir()
    env = clean_environment(work / "launch")
    (work / "launch/runtime").mkdir(mode=0o700)
    config = Path(env["XDG_CONFIG_HOME"]) / "voiceless/config.toml"
    config.parent.mkdir(parents=True)
    config.write_text("[console]\nport = 0\n", encoding="utf-8")
    config.chmod(0o600)
    command = root / "usr/bin" / entry["Exec"]
    with ExitStack() as cleanup:
        read_end, write_end = os.pipe()
        server = subprocess.Popen(["Xvfb", "-displayfd", str(write_end), "-screen", "0", "1600x1200x24",
                                   "-nolisten", "tcp", "-ac"], pass_fds=(write_end,),
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        os.close(write_end)
        cleanup.callback(stop, server)
        with os.fdopen(read_end) as pipe:
            number = read_line(pipe, 30).strip()
        require(number.isdigit(), "Xvfb did not start")
        env.update(DISPLAY=f":{number}", XDG_RUNTIME_DIR=str(work / "launch/runtime"),
                   PATH=f"{command.parent}:{env['PATH']}")
        bus = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address=1"],
                               env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        cleanup.callback(stop, bus)
        address = read_line(bus.stdout, 30).strip()
        require(address.startswith("unix:"), "dbus-daemon did not start")
        env["DBUS_SESSION_BUS_ADDRESS"] = address
        cleanup.callback(stop_running, runtime / "bin/voiceless")
        host, lines = start(command, env)
        cleanup.callback(stop, host)
        url = announce(host, lines, re.compile(r"Voiceless desktop: console (http://127\.0\.0\.1:\d+/)"), 90)
        opened, deadline = [], time.monotonic() + 30
        while len(opened) != 1 and time.monotonic() < deadline:
            opened = windows(env["DISPLAY"], env)
            time.sleep(0.1)
        require(len(opened) == 1 and host.poll() is None, "the desktop host did not open one window:\n"
                + "".join(lines["stderr"][-12:]))
        classes = output("xprop", "-display", env["DISPLAY"], "-id", opened[0], "WM_CLASS", env=env)
        require(f'"{entry["StartupWMClass"]}",' in classes, f"the window is of class {classes.strip()}, "
                f"not of the launcher's {entry['StartupWMClass']}")
        require(running(runtime / "bin/voiceless"), "the host did not start the runtime beside it")
        check_console(url)
        again = subprocess.run([command], env=env, capture_output=True, text=True, timeout=30)
        require(again.returncode == 0 and host.poll() is None and windows(env["DISPLAY"], env) == opened,
                "a second start did not hand the window to the running host")
        print(f"The desktop host opened one window on {env['DISPLAY']} over the console of the runtime "
              "it started beside it", flush=True)


def verify_desktop(work, candidates, arch, version, core, env):
    """The desktop package, read with dpkg's own tools: its runtime is the
    validated core payload byte for byte; its host, launcher, icon and
    dependencies are what they declare; and apt can install it here."""
    archives = list(candidates.glob(f"{DESKTOP}-v*-{target(arch)}.deb"))
    require(len(archives) == 1 and archives[0].name == desktop_name(version, arch),
            f"expected exactly one desktop package named {desktop_name(version, arch)}")
    archive = archives[0]
    print(output("dpkg-deb", "--info", archive), flush=True)
    config = desktop_config(version)
    bundle, deb = config["bundle"], config["bundle"]["linux"]["deb"]
    members = package_members(archive)
    require(set(members) == {"control", "md5sums"}, f"unexpected control members: {sorted(members)}")
    control = parse_control(members["control"])
    root = work / "desktop"
    subprocess.run(["dpkg-deb", "--extract", archive, root], check=True, stdout=subprocess.DEVNULL)
    runtime, doc = root / DESKTOP_PREFIX, f"usr/share/doc/{DESKTOP}"
    files, links = package_inventory(root)

    # What is known apart from the build says what the package must hold.
    launcher = desktop_input(config, "desktopTemplate")
    manifest = json.loads((core / "release.json").read_text())
    prefix = DESKTOP_PREFIX.as_posix()
    expected = {f"{prefix}/{name}": value for name, value in manifest["files"].items()}
    expected.update({f"{prefix}/release.json": digest(core / "release.json"),
                     f"usr/share/applications/{DESKTOP}.desktop": digest(launcher),
                     f"{doc}/copyright": hashlib.sha256(desktop_copyright().encode("utf-8")).hexdigest(),
                     **{f"usr/share/icons/hicolor/{size}/apps/{DESKTOP}.png": digest(icon)
                        for size, icon in desktop_icons(config).items()}})
    built = {f"{prefix}/bin/{DESKTOP}", f"{doc}/changelog.gz", f"{doc}/THIRD_PARTY_NOTICES.txt"}
    require(set(expected) <= files.keys() and all(files[name] == value for name, value in expected.items())
            and all(name in built or name.startswith(f"{doc}/licenses/")
                    for name in files.keys() - expected.keys()),
            "the desktop package does not hold the runtime, the launcher, the icon and the notices it declares")
    require(links == {"usr/bin/voiceless-desktop": f"../lib/{DESKTOP}/bin/{DESKTOP}",
                      "usr/bin/voiceless": f"../lib/{DESKTOP}/bin/voiceless"},
            f"unexpected links in the desktop package: {links}")
    with gzip.open(root / doc / "changelog.gz", "rb") as changelog:
        require(changelog.read() == desktop_input(config, "changelog").read_bytes(),
                "the package's changelog is not the project's")
    notices = (root / doc / "THIRD_PARTY_NOTICES.txt").read_text(encoding="utf-8")
    texts = {name for name in files if name.startswith(f"{doc}/licenses/")}
    require(texts and set(re.findall(rf"^{re.escape(doc)}/licenses/(.+)$", notices, re.M))
            == {name.removeprefix(f"{doc}/licenses/") for name in texts},
            "the host's third-party notices do not list the license texts in the package")
    listed = [line.split("  ", 1) for line in members["md5sums"].splitlines()]
    require([name for _, name in listed] == sorted(files) and all(
        checksum == hashlib.md5((root / name).read_bytes(), usedforsecurity=False).hexdigest()
        for checksum, name in listed), "md5sums does not describe the package")

    # The control file: what the host loads, and what the bundle declares.
    host = runtime / "bin" / DESKTOP
    depends = desktop_depends(host, arch, deb["depends"])
    check_tray(host)
    entry = desktop_entry(launcher.read_text(encoding="utf-8"))
    installed = sum((root / name).stat().st_size for name in files)
    directories = sum(1 for path in root.rglob("*") if path.is_dir()) + 1
    require(set(control) == {"Package", "Version", "Architecture", "Maintainer", "Installed-Size",
                             "Depends", "Section", "Priority", "Homepage", "Description"}
            and control["Package"] == DESKTOP and control["Version"] == version
            and control["Architecture"] == PLATFORMS[arch]["deb_arch"] == output("dpkg", "--print-architecture")
            and control["Depends"] == ", ".join(depends) and control["Maintainer"] == bundle["publisher"]
            and (control["Section"], control["Priority"], control["Homepage"])
            == (deb["section"], deb["priority"], bundle["homepage"])
            and control["Description"] == desktop_description(config)
            and installed // 1024 <= int(control["Installed-Size"])
            <= (installed + 4096 * directories + 1023) // 1024,
            f"the package control file is not what it declares:\n{members['control']}")
    for target_package in (f"./{archive.name}", DESKTOP_TRAY_PACKAGE):
        plan = subprocess.run(["apt-get", "--simulate", "install", target_package], cwd=candidates,
                              capture_output=True, text=True)
        require(plan.returncode == 0, f"apt cannot resolve {target_package} on this baseline:\n"
                f"{plan.stdout}{plan.stderr}")

    # The launcher: valid, and it starts the host by the name of its window and icon.
    require(entry == {"Type": "Application", "Name": config["productName"],
                      "Comment": bundle["shortDescription"], "Exec": DESKTOP, "Icon": DESKTOP,
                      "Terminal": "false", "Categories": f"{bundle['category']};", "StartupWMClass": DESKTOP},
            "the launcher must start the host by name, and give its window and icon that name")
    validated = subprocess.run(["desktop-file-validate", str(root / f"usr/share/applications/{DESKTOP}.desktop")],
                               capture_output=True, text=True)
    require(validated.returncode == 0 and not validated.stdout + validated.stderr,
            f"desktop-file-validate rejects the launcher:\n{validated.stdout}{validated.stderr}")
    for size in desktop_icons(config):
        width, height = map(int, size.split("x"))
        png = (root / f"usr/share/icons/hicolor/{size}/apps/{DESKTOP}.png").read_bytes()
        require(struct.unpack(">II", png[16:24]) == (width, height), f"the {size} icon is not a {size} PNG")

    verify_elf(runtime, arch, env)
    verify_host(host, runtime, arch, env)
    # The voiceless link runs the bundled core through its own `$ORIGIN/../lib`.
    linked = subprocess.run([root / "usr/bin/voiceless", "--version"], env=env, capture_output=True,
                            text=True, timeout=30)
    require(linked.returncode == 0 and linked.stdout.strip() == f"voiceless {version}",
            f"the package's voiceless command does not start: {linked.stderr}")
    launch_desktop(root, work, entry, runtime)


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
    speech = recognition_fixture(work, env)
    bun_command = verify_bun(work, version, arch, root_package, leaf_package, env)
    # The core by itself, for the command, the embedded console and recognition:
    # no Bun, Node or desktop. Then the same recognition through the Bun command.
    core_env = dict(env, PATH=str(work / "empty-bin"))
    Path(core_env["PATH"]).mkdir()
    verify_recognition(tree / "bin/voiceless", speech, core_env)
    verify_web(tree / "bin/voiceless", core_env)
    verify_recognition(bun_command, speech, env)
    verify_desktop(work, candidates, arch, version, tree, env)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidates", type=Path, help="directory containing candidate archives")
    parser.add_argument("--previous-archive", type=Path, help="also exercise rollback to this native 0.1.3 archive")
    parser.add_argument("--previous-sha256", help="trusted SHA-256 of the previous archive")
    args = parser.parse_args()
    if bool(args.previous_archive) != bool(args.previous_sha256):
        parser.error("--previous-archive and --previous-sha256 must be supplied together")
    os.environ["LC_ALL"] = "C"
    arch = platform.machine()
    require(platform.system() == "Linux" and arch in PLATFORMS, "requires native x86_64 or aarch64 Linux")
    require(output("getconf", "GNU_LIBC_VERSION") == f"glibc {GLIBC}",
            f"validate on the glibc {GLIBC} baseline (Ubuntu 22.04)")
    with tempfile.TemporaryDirectory(prefix="voiceless-release-validation-") as directory:
        validate(Path(directory), args.candidates.resolve(), arch)
        if args.previous_archive:
            verify_previous(Path(directory) / "upgrade", args.candidates.resolve(), arch,
                            args.previous_archive.resolve(), args.previous_sha256)
    print(f"Validated native, npm and desktop release candidates for {target(arch)}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"release validation: {error}")
