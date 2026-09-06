"""Publish the one-time legacy 0.4.8 transition using verified Release bytes.

The CLI is deliberately a single operation: no external hash, size, descriptor,
repository, version or download URL can be supplied to the publisher.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib

REPOSITORY = "chenjicheng/upmc"
VERSION = "0.4.8"
TAG = "v" + VERSION
DOWNLOAD_URL = f"https://github.com/{REPOSITORY}/releases/download/{TAG}/updater.exe"
PROXY_PREFIX = "https://gh.chenjicheng.cn/"
PATHS = ("version.json", "dev/version.json", "bridge/version.json", "bridge/dev/version.json")
MARKER = ".upmc-legacy-transition.json"
FIELDS = {"version", "build_id", "download_url", "sha256", "size"}


class PublicationError(RuntimeError):
    pass


def _require(condition, message):
    if not condition:
        raise PublicationError(message)


def _sha(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{40}", value) is not None


def _identity(version, tag, build_id):
    _require(version == VERSION and tag == TAG,
             "This publisher only accepts exact version 0.4.8 and tag v0.4.8")
    _require(_sha(build_id), "build_id must be a full lowercase 40-digit commit SHA")


def _validate_descriptor(descriptor):
    _require(isinstance(descriptor, dict) and set(descriptor) == FIELDS,
             "Descriptor must contain exactly version, build_id, download_url, sha256 and size")
    _identity(descriptor["version"], TAG, descriptor["build_id"])
    _require(descriptor["download_url"] in (DOWNLOAD_URL, PROXY_PREFIX + DOWNLOAD_URL),
             "download_url must be the official v0.4.8 updater.exe HTTPS URL or its approved proxy")
    _require(isinstance(descriptor["sha256"], str)
             and re.fullmatch(r"[0-9a-f]{64}", descriptor["sha256"]) is not None,
             "sha256 must contain 64 lowercase hex digits")
    _require(type(descriptor["size"]) is int and descriptor["size"] > 0,
             "size must be a positive integer byte length")


def _run(args, **kwargs):
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=180, **kwargs)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PublicationError(f"Command could not complete ({args[0]}): {error}") from error
    if result.returncode:
        raise PublicationError(f"Command failed ({args[0]} {args[1]}): {result.stderr.strip() or result.stdout.strip()}")
    return result.stdout.strip()


def _git(repo, *args, **kwargs):
    return _run(["git", "-C", str(repo), *args], **kwargs)


def make_descriptor(artifact, version, tag, build_id, download_url):
    _identity(version, tag, build_id)
    artifact = Path(artifact)
    _require(artifact.is_file() and not artifact.is_symlink(), "CI artifact must be a regular file")
    try:
        with artifact.open("rb") as stream:
            digest = hashlib.sha256()
            size = 0
            while chunk := stream.read(1024 * 1024):
                size += len(chunk)
                digest.update(chunk)
    except OSError as error:
        raise PublicationError(f"Cannot read artifact: {error}") from error
    descriptor = {"version": version, "build_id": build_id, "download_url": download_url,
                  "sha256": digest.hexdigest(), "size": size}
    _validate_descriptor(descriptor)
    return descriptor


def validate_source(source, tag, build_id, ref, channel):
    try:
        with (Path(source) / "upmc/Cargo.toml").open("rb") as stream:
            version = tomllib.load(stream)["package"]["version"]
    except (OSError, ValueError, KeyError, TypeError) as error:
        raise PublicationError(f"Cannot read source package version: {error}") from error
    _identity(version, tag, build_id)
    _require(ref == "refs/tags/" + TAG and channel == "stable",
             "Legacy publication requires the stable v0.4.8 tag; branches and dev are build-only")
    _require(_git(source, "rev-parse", "HEAD") == build_id, "build_id differs from checked-out source commit")
    _require(_git(source, "rev-parse", f"refs/tags/{TAG}^{{commit}}") == build_id,
             "Release tag differs from checked-out source commit")
    _require(not _git(source, "status", "--porcelain", "--untracked-files=no"),
             "Tracked source changes must be committed before publication")
    return version


def _api(endpoint, allow_404=False):
    args = ["gh", "api", "--hostname", "github.com", "--include", endpoint]
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=180)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise PublicationError(f"Release API transport failed: {error}") from error
    response = result.stdout.replace("\r\n", "\n")
    headers, separator, body = response.partition("\n\n")
    status_match = re.match(r"HTTP/\S+ (\d{3})(?: |\n|$)", headers)
    _require(status_match is not None and separator,
             f"Release API did not return an HTTP response: {result.stderr.strip()}")
    status = int(status_match[1])
    if allow_404 and status == 404:
        return None
    _require(result.returncode == 0 and status == 200,
             f"Release API failed with HTTP {status}: {result.stderr.strip()}")
    try:
        data = json.loads(body)
    except ValueError as error:
        raise PublicationError("Release API returned invalid JSON") from error
    _require(isinstance(data, dict), "Release API returned an invalid object")
    return data


def lookup_release(tag):
    _require(tag == TAG, "Only the v0.4.8 transition Release is allowed")
    return _api(f"repos/{REPOSITORY}/releases/tags/{tag}", allow_404=True)


def _verify_remote_tag(tag, build_id):
    data = _api(f"repos/{REPOSITORY}/git/ref/tags/{tag}")
    obj = data.get("object", {})
    for _ in range(5):
        _require(isinstance(obj, dict) and _sha(obj.get("sha")), "Remote tag has an invalid object")
        if obj.get("type") == "commit":
            _require(obj["sha"] == build_id, "Remote release tag does not point to build_id")
            return
        _require(obj.get("type") == "tag", "Remote release tag does not resolve to a commit")
        obj = _api(f"repos/{REPOSITORY}/git/tags/{obj['sha']}").get("object", {})
    raise PublicationError("Remote annotated tag nesting exceeds the verification bound")


def ensure_release(artifact, version, tag, build_id):
    expected = make_descriptor(artifact, version, tag, build_id, DOWNLOAD_URL)
    _verify_remote_tag(tag, build_id)
    release = lookup_release(tag)
    if release is None:
        _run(["gh", "release", "create", tag, str(Path(artifact).resolve()),
              "--repo", REPOSITORY, "--verify-tag", "--target", build_id,
              "--title", tag, "--generate-notes"])
        release = lookup_release(tag)
    _require(isinstance(release, dict), "Release is still unavailable after creation")
    _require(release.get("tag_name") == tag and release.get("draft") is False
             and release.get("prerelease") is False, "Release must be the public stable v0.4.8 Release")
    assets = release.get("assets")
    _require(isinstance(assets, list) and all(isinstance(asset, dict) for asset in assets),
             "Release assets must be a list of objects")
    matches = [asset for asset in assets if asset.get("name") == "updater.exe"]
    _require(len(matches) == 1, "Release must have exactly one updater.exe asset")
    asset = matches[0]
    _require(asset.get("state") == "uploaded", "Release updater.exe is not fully uploaded")
    _require(asset.get("browser_download_url") == DOWNLOAD_URL, "Release asset has an unexpected download URL")
    _require(type(asset.get("size")) is int and asset["size"] == expected["size"],
             "Release asset size differs from the downloaded CI artifact")
    if asset.get("digest") is not None:
        _require(asset["digest"] == "sha256:" + expected["sha256"],
                 "Release asset digest differs from the downloaded CI artifact")
    with tempfile.TemporaryDirectory(prefix="upmc-release-verify-") as directory:
        _run(["gh", "release", "download", tag, "--repo", REPOSITORY,
              "--pattern", "updater.exe", "--dir", directory])
        actual = make_descriptor(Path(directory) / "updater.exe", version, tag, build_id, DOWNLOAD_URL)
    _require(actual == expected, "Downloaded Release bytes differ from the downloaded CI artifact")
    _verify_remote_tag(tag, build_id)
    return actual


def _page_files(pages, head):
    entries = {}
    for entry in _git(pages, "ls-tree", "-r", "--full-tree", head).splitlines():
        mode_type_oid, path = entry.split("\t", 1)
        mode, kind, oid = mode_type_oid.split()
        entries[path] = (mode, kind, oid)
    return entries


def _read_page(pages, entries, path):
    mode, kind, oid = entries[path]
    _require(mode == "100644" and kind == "blob", f"Pages path must be an ordinary file: {path}")
    return _git(pages, "cat-file", "blob", oid)


def _json_page(pages, entries, path):
    try:
        value = json.loads(_read_page(pages, entries, path))
    except ValueError as error:
        raise PublicationError(f"Pages document is invalid JSON: {path}") from error
    _require(isinstance(value, dict), f"Pages document must be an object: {path}")
    return value


def publish_pages(pages, descriptor, expected_head):
    _validate_descriptor(descriptor)
    _require(_sha(expected_head), "Expected Pages HEAD must be a full commit SHA")
    _require(_git(pages, "rev-parse", "HEAD") == expected_head, "Pages checkout is stale")
    _require(not _git(pages, "status", "--porcelain"), "Pages checkout must be clean")
    remote_head = _git(pages, "ls-remote", "--exit-code", "origin", "refs/heads/gh-pages").split()
    _require(remote_head == [expected_head, "refs/heads/gh-pages"], "Remote Pages HEAD has changed; revalidate before retrying")
    entries = _page_files(pages, expected_head)
    _require("CNAME" in entries, "Existing Pages CNAME is required; initialization is not permitted")
    _require(_read_page(pages, entries, "CNAME") == "upmc.chenjicheng.cn", "Existing Pages CNAME is unexpected")
    if MARKER in entries:
        _require(_json_page(pages, entries, MARKER) == descriptor,
                 "The legacy transition is already frozen to a different artifact")
        _require(all(path in entries and _json_page(pages, entries, path) == descriptor for path in PATHS),
                 "Transition already published and bridge metadata changed; refusing to rewind promotion")
        return expected_head
    for path in PATHS:
        if path in entries:
            existing = _json_page(pages, entries, path)
            if existing == descriptor:
                continue
            _require(not path.startswith("bridge/"), "Existing bridge metadata prevents initial transition publication")
            existing_version = existing.get("version")
            _require(existing_version in (None, "0.4.7"), "Unexpected legacy version; refusing to overwrite a newer or unknown release")
        # Prevent a file/tree collision from deleting unrelated Pages content.
        _require(not any(key.startswith(path + "/") for key in entries), f"Pages target is a directory: {path}")
        for parent in Path(path).parents:
            _require(parent.as_posix() not in entries, f"Pages parent path is a file: {parent}")
    payload = json.dumps(descriptor, indent=2, sort_keys=True) + "\n"
    # Build the full candidate tree with an isolated index. Never modify checkout
    # files, the real index, or local refs, even if object creation/push fails.
    with tempfile.TemporaryDirectory(prefix="upmc-pages-index-") as directory:
        env = dict(os.environ, GIT_INDEX_FILE=str(Path(directory) / "index"))
        _git(pages, "read-tree", expected_head, env=env)
        oid = _git(pages, "hash-object", "-w", "--stdin", input=payload)
        for path in (*PATHS, MARKER):
            _git(pages, "update-index", "--add", "--cacheinfo", f"100644,{oid},{path}", env=env)
        tree = _git(pages, "write-tree", env=env)
        commit = _git(pages, "commit-tree", tree, "-p", expected_head,
                      "-m", f"Publish frozen legacy {TAG} transition ({descriptor['build_id']})")
    # This new commit has expected_head as its parent. The lease rejects any
    # intervening writer, including a native publisher using another workflow.
    _git(pages, "push", f"--force-with-lease=refs/heads/gh-pages:{expected_head}",
         "origin", f"{commit}:refs/heads/gh-pages")
    return commit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("validate-source", "publish"))
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--build-id", required=True)
    parser.add_argument("--ref", required=True)
    parser.add_argument("--channel", required=True)
    parser.add_argument("--artifact", type=Path)
    parser.add_argument("--pages", type=Path)
    parser.add_argument("--expected-pages-head")
    args = parser.parse_args()
    try:
        version = validate_source(args.source, args.tag, args.build_id, args.ref, args.channel)
        if args.command == "validate-source":
            print(f"Validated legacy {version} source at {args.build_id}")
            return 0
        _require(args.artifact and args.pages and args.expected_pages_head,
                 "publish requires --artifact, --pages and --expected-pages-head")
        descriptor = ensure_release(args.artifact, version, args.tag, args.build_id)
        commit = publish_pages(args.pages, descriptor, args.expected_pages_head)
        print(json.dumps({"pages_commit": commit, "descriptor": descriptor}, indent=2))
        return 0
    except PublicationError as error:
        print(f"Legacy publication failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
