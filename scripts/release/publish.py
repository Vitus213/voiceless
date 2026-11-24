#!/usr/bin/env python3
"""Publish already validated Voiceless candidates for one stable Git tag."""

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tarfile
import time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

REPO = "Vitus213/voiceless"
REGISTRY = "https://registry.npmjs.org"
ARCHITECTURES = ("x86_64", "aarch64")
PLATFORMS = tuple(f"voiceless-linux-{arch}-gnu" for arch in ARCHITECTURES)
STABLE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def version_number(value):
    match = STABLE.fullmatch(value)
    if match is None:
        raise ValueError(f"expected a stable version, got {value!r}")
    return tuple(map(int, match.groups()))


def digest(path, algorithm):
    result = hashlib.new(algorithm)
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            result.update(chunk)
    return result


def npm_registry(name):
    request = Request(f"{REGISTRY}/{name}", headers={"Cache-Control": "no-cache"})
    try:
        with urlopen(request, timeout=30) as response:
            return json.load(response)
    except HTTPError as error:
        if error.code == 404:
            return None
        raise


def npm_ready(name, version, path, metadata):
    if metadata is None:
        if not os.environ.get("NODE_AUTH_TOKEN"):
            raise ValueError(f"first publish of {name} needs the NPM_BOOTSTRAP_TOKEN secret")
        return False
    # A staged version holds the placeholder tag `0.0.0-stage` until a
    # maintainer approves it; that is neither an older nor a published release.
    latest = metadata.get("dist-tags", {}).get("latest")
    if (latest is not None and STABLE.fullmatch(latest)
            and version_number(latest) > version_number(version)):
        raise ValueError(f"{name}@latest is already {latest}; refusing to release older {version}")
    published = metadata.get("versions", {}).get(version)
    if published is None:
        return False
    integrity = "sha512-" + base64.b64encode(digest(path, "sha512").digest()).decode("ascii")
    if published.get("dist", {}).get("integrity") != integrity:
        raise ValueError(f"{name}@{version} already exists with different bytes; use the original candidate artifacts")
    if latest != version:
        raise ValueError(f"{name}@{version} exists but latest is {latest!r}; check the registry tag before retrying")
    return True


def github(*args, capture=False):
    env = {key: value for key, value in os.environ.items() if key != "NODE_AUTH_TOKEN"}
    return subprocess.run(["gh", *args], env=env, check=True, text=True, capture_output=capture).stdout


def verify_assets(release, requested):
    observed = {asset["name"]: asset for asset in release["assets"]}
    missing = []
    for path, expected in requested.items():
        asset = observed.get(path.name)
        if asset is None:
            missing.append(path)
        elif (asset["state"] != "uploaded" or asset["size"] != path.stat().st_size
              or asset.get("digest") != expected):
            raise ValueError(f"GitHub asset {path.name} differs from the candidate; inspect the release before retrying")
    return missing


def prepare_github(tag, requested):
    endpoint = f"repos/{REPO}/releases"
    pages = json.loads(github("api", "--paginate", "--slurp", f"{endpoint}?per_page=100", capture=True))
    releases = [release for page in pages for release in page]
    version = version_number(tag[1:])
    for release in releases:
        other = release["tag_name"].removeprefix("v")
        if (not release["draft"] and not release["prerelease"] and STABLE.fullmatch(other)
                and version_number(other) > version):
            raise ValueError(f"GitHub already has a newer stable release {other}")
    matches = [release for release in releases if release["tag_name"] == tag]
    if len(matches) > 1:
        raise ValueError(f"more than one GitHub release matches {tag}; inspect before retrying")
    if matches:
        release = matches[0]
    else:
        github("release", "create", tag, "--repo", REPO, "--verify-tag", "--draft",
               "--generate-notes", "--title", tag)
        release_id = int(github("release", "view", tag, "--repo", REPO, "--json", "databaseId",
                                "--jq", ".databaseId", capture=True))
        release = json.loads(github("api", f"{endpoint}/{release_id}", capture=True))
    if release["tag_name"] != tag or release["prerelease"]:
        raise ValueError(f"GitHub returned a release other than stable {tag}")
    missing = verify_assets(release, requested)
    if missing:
        if not release["draft"]:
            raise ValueError(f"published GitHub release {tag} is missing required assets")
        github("release", "upload", tag, *map(str, missing), "--repo", REPO)
        release = json.loads(github("api", f"{endpoint}/{release['id']}", capture=True))
        if verify_assets(release, requested):
            raise ValueError(f"GitHub release {tag} did not retain every uploaded asset")
    return release["draft"]


def publish(candidates, tag):
    if not tag.startswith("v"):
        raise ValueError(f"expected a v-prefixed Git tag, got {tag!r}")
    version = tag[1:]
    version_number(version)
    packages = {name: candidates / f"{name}-{version}.tgz" for name in (*PLATFORMS, "voiceless")}
    archives = [candidates / f"voiceless-{tag}-linux-{arch}-gnu.tar.gz" for arch in ARCHITECTURES]
    for archive in [*archives, *packages.values()]:
        if not archive.is_file():
            raise ValueError(f"missing release candidate {archive.name}")
    for name, path in packages.items():
        with tarfile.open(path, "r:gz") as archive:
            manifest = json.load(archive.extractfile("package/package.json"))
        if (manifest["name"], manifest["version"]) != (name, version):
            raise ValueError(f"npm package identity does not match {path.name}")
        if name == "voiceless" and manifest.get("optionalDependencies") != dict.fromkeys(PLATFORMS, version):
            raise ValueError("npm main package must pin both platform packages to the release version")

    pending = [(name, path) for name, path in packages.items()
               if not npm_ready(name, version, path, npm_registry(name))]
    sums = candidates / "SHA256SUMS"
    hashes = {path: digest(path, "sha256").hexdigest() for path in archives}
    sums.write_text("".join(f"{value}  {path.name}\n" for path, value in hashes.items()), encoding="ascii")
    hashes[sums] = digest(sums, "sha256").hexdigest()
    finish_github = prepare_github(tag, {path: f"sha256:{value}" for path, value in hashes.items()})

    env = {key: value for key, value in os.environ.items() if key != "GH_TOKEN"}
    for name, path in pending:
        print(f"Publishing {name}@{version}", flush=True)
        # --provenance makes npm use the workflow's OIDC identity, so a
        # configured trusted publisher needs no token at all.
        subprocess.run(["npm", "publish", str(path.resolve()), "--access", "public", "--tag", "latest",
                        "--provenance", "--ignore-scripts", "--registry", REGISTRY], env=env, check=True)
        # A brand-new package is listed a few minutes after npm accepts it.
        for _ in range(30):
            if npm_ready(name, version, path, npm_registry(name)):
                break
            time.sleep(10)
        else:
            raise ValueError(f"npm registry has not confirmed {name}@{version} after 5 minutes; "
                             "approve a staged version on npmjs.com or rerun the publish job")
    if finish_github:
        github("release", "edit", tag, "--repo", REPO, "--draft=false", "--latest")
    print(f"Published {tag} to npm and GitHub Releases.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("candidates", type=Path)
    parser.add_argument("tag")
    args = parser.parse_args()
    try:
        publish(args.candidates, args.tag)
    except (OSError, ValueError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        sys.exit(f"publish: {error}")
