"""Behavioral publication tests: real temporary bytes/Git; only gh is simulated."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "publication", Path(__file__).resolve().parents[1] / "legacy_publication.py"
)
pub = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(pub)
RUN = subprocess.run
VERSION = "0.4.8"
TAG = "v0.4.8"
URL = "https://github.com/chenjicheng/upmc/releases/download/v0.4.8/updater.exe"
PATHS = ("version.json", "dev/version.json", "bridge/version.json", "bridge/dev/version.json")


def git(cwd, *args, **kwargs):
    return RUN(["git", "-C", str(cwd), *args], check=True, capture_output=True,
               text=True, **kwargs).stdout.strip()


def init_repo(path):
    path.mkdir()
    git(path, "init", "-b", "gh-pages")
    git(path, "config", "user.name", "Publication test")
    git(path, "config", "user.email", "publication@example.invalid")


class Fixtures(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.source = self.root / "source"
        init_repo(self.source)
        (self.source / "upmc").mkdir()
        (self.source / "upmc/Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "0.4.8"\n')
        git(self.source, "add", ".")
        git(self.source, "commit", "-m", "Source fixture")
        self.sha = git(self.source, "rev-parse", "HEAD")
        git(self.source, "tag", TAG)
        self.artifact = self.root / "updater.exe"
        # Byte fixture is intentionally not a runnable updater.
        self.payload = b"MZ\x00CI artifact fixture\x00" + bytes(range(256))
        self.artifact.write_bytes(self.payload)
        self.digest = hashlib.sha256(self.payload).hexdigest()

    def descriptor(self):
        return {"version": VERSION, "build_id": self.sha, "download_url": URL,
                "sha256": self.digest, "size": len(self.payload)}


class DescriptorTests(Fixtures):
    def test_metadata_is_derived_from_actual_bytes(self):
        self.assertEqual(pub.make_descriptor(self.artifact, VERSION, TAG, self.sha, URL), self.descriptor())
        self.artifact.write_bytes(self.payload + b"signed bytes")
        result = pub.make_descriptor(self.artifact, VERSION, TAG, self.sha, URL)
        self.assertEqual(result["size"], self.artifact.stat().st_size)
        self.assertEqual(result["sha256"], hashlib.sha256(self.artifact.read_bytes()).hexdigest())

    def test_missing_empty_or_directory_artifacts_fail(self):
        self.artifact.write_bytes(b"")
        for artifact in (self.artifact, self.root / "missing", self.root):
            with self.subTest(artifact=artifact), self.assertRaises(pub.PublicationError):
                pub.make_descriptor(artifact, VERSION, TAG, self.sha, URL)

    def test_exact_identity_and_url_are_required(self):
        cases = [("", TAG, self.sha, URL), ("0.4.08", TAG, self.sha, URL),
                 ("0.6.0", "v0.6.0", self.sha, URL), (VERSION, "vv0.4.8", self.sha, URL),
                 (VERSION, "v0.4.7", self.sha, URL), (VERSION, TAG, "abcdef0", URL),
                 (VERSION, TAG, "g" * 40, URL)]
        urls = ["", "null", URL.replace("https:", "http:"), URL + "?token=x", URL + "#x",
                URL.replace("chenjicheng/upmc/", "attacker/upmc/"),
                URL.replace("v0.4.8", "v0.4.7"), URL.replace("updater.exe", "other.exe"),
                "https://github.com@attacker.invalid/" + URL,
                "https://gh.chenjicheng.cn/https://attacker.invalid/updater.exe"]
        cases += [(VERSION, TAG, self.sha, url) for url in urls]
        for args in cases:
            with self.subTest(args=args), self.assertRaises(pub.PublicationError):
                pub.make_descriptor(self.artifact, *args)

    def test_existing_approved_proxy_is_accepted(self):
        url = "https://gh.chenjicheng.cn/" + URL
        self.assertEqual(pub.make_descriptor(self.artifact, VERSION, TAG, self.sha, url)["download_url"], url)

    def test_source_commit_tag_and_manifest_must_match(self):
        self.assertEqual(pub.validate_source(self.source, TAG, self.sha, "refs/tags/" + TAG, "stable"), VERSION)
        for tag, build, ref, channel in [(TAG, self.sha[:7], "refs/tags/" + TAG, "stable"),
                                        (TAG, self.sha, "refs/heads/dev", "dev"),
                                        ("v0.6.0", self.sha, "refs/tags/v0.6.0", "stable"),
                                        (TAG, self.sha, "refs/tags/" + TAG, "dev")]:
            with self.subTest(tag=tag, build=build, ref=ref, channel=channel), self.assertRaises(pub.PublicationError):
                pub.validate_source(self.source, tag, build, ref, channel)
        (self.source / "upmc/Cargo.toml").write_text('[package]\nversion = "0.4.7"\n')
        with self.assertRaises(pub.PublicationError):
            pub.validate_source(self.source, TAG, self.sha, "refs/tags/" + TAG, "stable")

    def test_source_tag_must_resolve_to_checked_out_commit(self):
        (self.source / "new-file").write_text("new commit")
        git(self.source, "add", ".")
        git(self.source, "commit", "-m", "Later source")
        new_sha = git(self.source, "rev-parse", "HEAD")
        with self.assertRaises(pub.PublicationError):
            pub.validate_source(self.source, TAG, new_sha, "refs/tags/" + TAG, "stable")


class ReleaseTests(Fixtures):
    def setUp(self):
        super().setUp()
        self.calls = []
        self.exists = True
        self.status = 200
        self.release = {"tag_name": TAG, "draft": False, "prerelease": False,
                        "assets": [{"name": "updater.exe", "state": "uploaded", "size": len(self.payload),
                                    "browser_download_url": URL, "digest": "sha256:" + self.digest}]}
        self.download = self.payload
        self.remote_sha = self.sha
        self.fail_create = False
        self.fail_download = False

    def gh(self, args, **kwargs):
        if args[0] != "gh":
            return RUN(args, **kwargs)
        self.calls.append(args)
        if args[1] == "api":
            endpoint = next(a for a in args[2:] if a.startswith("repos/"))
            if "/git/ref/" in endpoint:
                body, status = {"object": {"type": "commit", "sha": self.remote_sha}}, 200
            else:
                status = self.status if self.exists else 404
                body = self.release if status == 200 else {"message": "API failed"}
            output = (f"HTTP/2.0 {status} STATUS\r\nContent-Type: application/json\r\n\r\n" + json.dumps(body)) if status else ""
            return subprocess.CompletedProcess(args, 0 if status == 200 else 1, output, "simulated API error" if status != 200 else "")
        if args[1:3] == ["release", "create"]:
            if self.fail_create:
                return subprocess.CompletedProcess(args, 1, "", "creation failed")
            self.exists = True
            return subprocess.CompletedProcess(args, 0, "created", "")
        if args[1:3] == ["release", "download"]:
            if self.fail_download:
                return subprocess.CompletedProcess(args, 1, "", "download failed")
            destination = Path(args[args.index("--dir") + 1])
            (destination / "updater.exe").write_bytes(self.download)
            return subprocess.CompletedProcess(args, 0, "", "")
        self.fail("Unexpected gh call: " + repr(args))

    def test_only_confirmed_404_is_missing(self):
        with patch.object(subprocess, "run", self.gh):
            self.assertEqual(pub.lookup_release(TAG), self.release)
            self.exists = False
            self.assertIsNone(pub.lookup_release(TAG))

    def test_auth_transport_rate_limit_and_server_failures_never_create_release(self):
        with patch.object(subprocess, "run", self.gh):
            for status in (0, 401, 403, 429, 500, 502):
                self.status = status
                with self.subTest(status=status), self.assertRaises(pub.PublicationError):
                    pub.ensure_release(self.artifact, VERSION, TAG, self.sha)
        self.assertFalse(any(call[1:3] == ["release", "create"] for call in self.calls))

    def test_existing_release_bytes_are_verified_against_ci_artifact(self):
        with patch.object(subprocess, "run", self.gh):
            self.assertEqual(pub.ensure_release(self.artifact, VERSION, TAG, self.sha), self.descriptor())
        self.assertFalse(any(call[1:3] == ["release", "create"] for call in self.calls))

    def test_missing_release_is_created_then_downloaded_and_verified(self):
        self.exists = False
        with patch.object(subprocess, "run", self.gh):
            self.assertEqual(pub.ensure_release(self.artifact, VERSION, TAG, self.sha), self.descriptor())
        creation = next(c for c in self.calls if c[1:3] == ["release", "create"])
        self.assertIn("--verify-tag", creation)
        self.assertNotIn("--clobber", creation)

    def test_release_creation_and_download_failures_are_fatal(self):
        for failure in ("fail_create", "fail_download"):
            with self.subTest(failure=failure):
                self.fail_create = self.fail_download = False
                self.exists = failure != "fail_create"
                setattr(self, failure, True)
                with patch.object(subprocess, "run", self.gh), self.assertRaises(pub.PublicationError):
                    pub.ensure_release(self.artifact, VERSION, TAG, self.sha)

    def test_wrong_bytes_truncated_bytes_and_bad_release_fields_are_rejected(self):
        cases = [("download", b"wrong"), ("download", b""), ("download", self.payload[:-1]),
                 ("remote_sha", "0" * 40)]
        for attr, value in cases:
            original = getattr(self, attr)
            setattr(self, attr, value)
            with self.subTest(attr=attr, value=value), patch.object(subprocess, "run", self.gh), self.assertRaises(pub.PublicationError):
                pub.ensure_release(self.artifact, VERSION, TAG, self.sha)
            setattr(self, attr, original)
        changes = [{"tag_name": "v0.4.7"}, {"draft": True}, {"prerelease": True}, {"assets": []},
                   {"assets": self.release["assets"] * 2}]
        original = json.loads(json.dumps(self.release))
        for changeset in changes:
            self.release = dict(original, **changeset)
            with self.subTest(changes=changeset), patch.object(subprocess, "run", self.gh), self.assertRaises(pub.PublicationError):
                pub.ensure_release(self.artifact, VERSION, TAG, self.sha)
        for field, value in [("size", 0), ("size", True), ("size", len(self.payload) + 1),
                             ("browser_download_url", ""), ("digest", "sha256:" + "0" * 64),
                             ("state", "new")]:
            self.release = json.loads(json.dumps(original))
            self.release["assets"][0][field] = value
            with self.subTest(field=field, value=value), patch.object(subprocess, "run", self.gh), self.assertRaises(pub.PublicationError):
                pub.ensure_release(self.artifact, VERSION, TAG, self.sha)


class PagesTests(Fixtures):
    def setUp(self):
        super().setUp()
        self.pages = self.root / "pages"
        init_repo(self.pages)
        self.cname = b"upmc.chenjicheng.cn\r\n"
        (self.pages / "CNAME").write_bytes(self.cname)
        (self.pages / "unrelated.txt").write_bytes(b"preserve unrelated content\n")
        (self.pages / "version.json").write_text('{"version":"0.4.7"}\n')
        git(self.pages, "-c", "core.autocrlf=false", "add", ".")
        git(self.pages, "commit", "-m", "Existing Pages")
        self.head = git(self.pages, "rev-parse", "HEAD")
        self.remote = self.root / "remote.git"
        RUN(["git", "init", "--bare", str(self.remote)], check=True, capture_output=True)
        git(self.pages, "remote", "add", "origin", str(self.remote))
        git(self.pages, "push", "origin", "gh-pages")

    def remote_head(self):
        return git(self.remote, "rev-parse", "refs/heads/gh-pages")

    def blob(self, path):
        return RUN(["git", "-C", str(self.remote), "show", "refs/heads/gh-pages:" + path],
                   check=True, capture_output=True).stdout

    def assert_untouched(self):
        self.assertEqual(self.remote_head(), self.head)
        self.assertEqual(git(self.pages, "rev-parse", "HEAD"), self.head)
        self.assertEqual((self.pages / "CNAME").read_bytes(), self.cname)
        self.assertFalse((self.pages / "dev").exists())
        self.assertEqual(git(self.pages, "status", "--porcelain"), "")

    def test_four_equal_descriptors_are_published_in_exactly_one_commit(self):
        new_head = pub.publish_pages(self.pages, self.descriptor(), self.head)
        self.assertEqual(new_head, self.remote_head())
        self.assertEqual(git(self.remote, "rev-parse", new_head + "^"), self.head)
        for path in PATHS:
            self.assertEqual(json.loads(self.blob(path)), self.descriptor())
            self.assertEqual(self.blob(path), self.blob(PATHS[0]))
        self.assertEqual(self.blob("CNAME"), self.cname)
        self.assertEqual(self.blob("unrelated.txt"), b"preserve unrelated content\n")
        self.assertEqual(git(self.pages, "status", "--porcelain"), "")

    def test_invalid_metadata_never_changes_pages(self):
        for field, value in [("sha256", ""), ("sha256", "0"), ("size", 0), ("size", -1),
                             ("size", True), ("size", "100"), ("download_url", ""),
                             ("version", "0.6.0"), ("build_id", "")]:
            with self.subTest(field=field, value=value), self.assertRaises(pub.PublicationError):
                pub.publish_pages(self.pages, dict(self.descriptor(), **{field: value}), self.head)
            self.assert_untouched()
        for field in self.descriptor():
            desc = self.descriptor()
            del desc[field]
            with self.subTest(missing=field), self.assertRaises(pub.PublicationError):
                pub.publish_pages(self.pages, desc, self.head)
            self.assert_untouched()

    def test_missing_or_wrong_cname_fails_without_publication(self):
        for cname in (None, "other.example.invalid"):
            target = self.pages / "CNAME"
            if cname is None:
                target.unlink()
            else:
                target.write_text(cname)
            git(self.pages, "add", "-A")
            git(self.pages, "commit", "-m", "CNAME fixture")
            git(self.pages, "push", "origin", "gh-pages")
            self.head = self.remote_head()
            with self.subTest(cname=cname), self.assertRaises(pub.PublicationError):
                pub.publish_pages(self.pages, self.descriptor(), self.head)
            self.assertEqual(self.remote_head(), self.head)
            self.assertFalse((self.pages / "dev").exists())

    def test_failed_push_does_not_leave_partial_files_or_move_any_branch(self):
        hook = self.remote / "hooks/pre-receive"
        hook.write_text("#!/bin/sh\nexit 1\n", newline="\n")
        hook.chmod(0o755)
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(self.pages, self.descriptor(), self.head)
        self.assert_untouched()

    def test_stale_expected_head_and_concurrent_remote_update_are_rejected(self):
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(self.pages, self.descriptor(), "0" * 40)
        self.assert_untouched()
        def race(args, **kwargs):
            if "push" in args:
                other = git(self.remote, "-c", "user.name=Race", "-c", "user.email=race@example.invalid",
                            "commit-tree", self.head + "^{tree}", "-p", self.head, "-m", "Concurrent publisher")
                git(self.remote, "update-ref", "refs/heads/gh-pages", other, self.head)
            return RUN(args, **kwargs)
        with patch.object(subprocess, "run", race), self.assertRaises(pub.PublicationError):
            pub.publish_pages(self.pages, self.descriptor(), self.head)
        self.assertEqual(json.loads(self.blob("version.json")), {"version": "0.4.7"})
        self.assertEqual(git(self.pages, "rev-parse", "HEAD"), self.head)
        self.assertEqual(git(self.pages, "status", "--porcelain"), "")

    def test_rerun_is_idempotent_and_later_bridge_promotion_cannot_be_rewound(self):
        published = pub.publish_pages(self.pages, self.descriptor(), self.head)
        git(self.pages, "fetch", "origin", "gh-pages")
        git(self.pages, "reset", "--hard", "FETCH_HEAD")
        self.assertEqual(pub.publish_pages(self.pages, self.descriptor(), published), published)
        promoted = dict(self.descriptor(), version="0.6.0")
        (self.pages / "bridge/version.json").write_text(json.dumps(promoted))
        git(self.pages, "add", ".")
        git(self.pages, "commit", "-m", "Native promotion fixture")
        git(self.pages, "push", "origin", "gh-pages")
        promotion_head = self.remote_head()
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(self.pages, self.descriptor(), promotion_head)
        self.assertEqual(self.remote_head(), promotion_head)
        self.assertEqual(json.loads(self.blob("bridge/version.json")), promoted)


if __name__ == "__main__":
    unittest.main()
