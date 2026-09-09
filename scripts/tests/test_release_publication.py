"""0.5 promotion must preserve frozen legacy first-hop descriptors."""
import importlib.util
import json
from pathlib import Path
import unittest
import subprocess
from unittest.mock import patch
from test_legacy_publication import Fixtures, git, init_repo

spec = importlib.util.spec_from_file_location('release_pub', Path(__file__).resolve().parents[1] / 'release_publication.py')
pub = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pub)

class ReleaseTests(Fixtures):
    def predecessor(self):
        return dict(self.descriptor(), version='0.5.1',
                    download_url='https://github.com/chenjicheng/upmc/releases/download/v0.5.1/updater.exe')

    def commit_pages(self, pages):
        git(pages, 'add', '-A'); git(pages, 'commit', '-m', 'publication fixture')
        git(pages, 'push', 'origin', 'gh-pages')
        return git(pages, 'rev-parse', 'HEAD')

    def predecessor_fixture(self):
        pages, remote, _ = self.pages_fixture()
        for name in (*pub.PATHS, '.upmc-release-0.5.1.json'):
            (pages / name).write_text(json.dumps(self.predecessor()))
        return pages, remote, self.commit_pages(pages)

    def test_known_predecessor_promotes_atomically_and_preserves_all_history(self):
        pages, remote, head = self.predecessor_fixture()
        result = pub.publish_pages(pages, self.new_descriptor(), head)
        changed = git(remote, 'diff-tree', '--no-commit-id', '--name-only', '-r', result).splitlines()
        self.assertEqual(set(changed), {*pub.PATHS, '.upmc-release-0.5.2.json'})
        for name in ('CNAME', 'version.json', 'dev/version.json', '.upmc-legacy-transition.json',
                     '.upmc-release-0.5.1.json', 'unrelated.txt'):
            self.assertEqual(git(remote, 'show', f'{result}:{name}'), git(pages, 'show', f'{head}:{name}'))
        for name in (*pub.PATHS, '.upmc-release-0.5.2.json'):
            self.assertEqual(json.loads(git(remote, 'show', f'{result}:{name}')), self.new_descriptor())
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)
        self.assertEqual(git(pages, 'status', '--porcelain'), '')

    def test_predecessor_without_marker_is_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        (pages / '.upmc-release-0.5.1.json').unlink()
        head = self.commit_pages(pages)
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_current_version_without_its_marker_is_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for name in pub.PATHS:
            (pages / name).write_text(json.dumps(self.new_descriptor()))
        head = self.commit_pages(pages)
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_predecessor_feed_disagreement_and_marker_mismatch_are_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for name in ('bridge/dev/version.json', '.upmc-release-0.5.1.json'):
            with self.subTest(path=name):
                (pages / name).write_text(json.dumps(dict(self.predecessor(), sha256='1' * 64)))
                head = self.commit_pages(pages)
                with self.assertRaises(pub.PublicationError):
                    pub.publish_pages(pages, self.new_descriptor(), head)
                self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
                (pages / name).write_text(json.dumps(self.predecessor()))
                self.commit_pages(pages)

    def test_matching_but_invalid_predecessor_descriptors_are_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for change in ({'version': '0.5.3'}, {'version': '0.5.0'}, {'size': True},
                       {'size': 0}, {'build_id': 'short'}, {'sha256': 'bad'},
                       {'download_url': pub.DOWNLOAD_URL}, {'extra': 'not allowed'}):
            with self.subTest(change=change):
                for name in (*pub.PATHS, '.upmc-release-0.5.1.json'):
                    (pages / name).write_text(json.dumps(dict(self.predecessor(), **change)))
                head = self.commit_pages(pages)
                with self.assertRaises(pub.PublicationError):
                    pub.publish_pages(pages, self.new_descriptor(), head)
                self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_boolean_feed_size_cannot_match_integer_marker_size(self):
        pages, remote, _ = self.predecessor_fixture()
        (pages / '.upmc-release-0.5.1.json').write_text(json.dumps(dict(self.predecessor(), size=1)))
        for name in pub.PATHS:
            (pages / name).write_text(json.dumps(dict(self.predecessor(), size=True)))
        head = self.commit_pages(pages)
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
        current = dict(self.new_descriptor(), size=1)
        (pages / pub.MARKER).write_text(json.dumps(current))
        for name in pub.PATHS:
            (pages / name).write_text(json.dumps(dict(current, size=True)))
        head = self.commit_pages(pages)
        with self.assertRaises(pub.PublicationError):
            pub.publish_pages(pages, current, head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_exact_current_version_manifest_and_download_identity(self):
        import tomllib
        root = Path(__file__).resolve().parents[2]
        with (root / 'upmc/Cargo.toml').open('rb') as stream:
            self.assertEqual(tomllib.load(stream)['package']['version'], '0.5.2')
        with (root / 'Cargo.lock').open('rb') as stream:
            packages = tomllib.load(stream)['package']
        self.assertEqual([p['version'] for p in packages if p['name'] == 'upmc'], ['0.5.2'])
        for version, tag, url in [('0.5.1', 'v0.5.1', self.predecessor()['download_url']),
                                  ('0.5.2', 'v0.5.2', self.predecessor()['download_url']),
                                  ('0.5.2', 'v0.5.1', self.new_descriptor()['download_url'])]:
            with self.subTest(version=version, tag=tag, url=url), self.assertRaises(pub.PublicationError):
                pub.make_descriptor(self.artifact, version, tag, self.sha, url)

    def test_current_source_tag_commit_and_stable_channel_are_required(self):
        (self.source / 'upmc/Cargo.toml').write_text('[package]\nname = "fixture"\nversion = "0.5.2"\n')
        git(self.source, 'add', '.'); git(self.source, 'commit', '-m', 'current release')
        current = git(self.source, 'rev-parse', 'HEAD')
        git(self.source, 'tag', 'v0.5.2')
        self.assertEqual(pub.validate_source(self.source, 'v0.5.2', current, 'refs/tags/v0.5.2', 'stable'), '0.5.2')
        for tag, sha, ref, channel in [('v0.5.1', current, 'refs/tags/v0.5.1', 'stable'),
                                       ('v0.5.2', self.sha, 'refs/tags/v0.5.2', 'stable'),
                                       ('v0.5.2', current, 'refs/heads/main', 'stable'),
                                       ('v0.5.2', current, 'refs/tags/v0.5.2', 'dev')]:
            with self.subTest(tag=tag, sha=sha, ref=ref, channel=channel), self.assertRaises(pub.PublicationError):
                pub.validate_source(self.source, tag, sha, ref, channel)
    def test_historical_workflow_does_not_duplicate_branch_builds(self):
        root = Path(__file__).resolve().parents[2]
        historical = (root / '.github/workflows/build-updater.yml').read_text(encoding='utf-8')
        current = (root / '.github/workflows/release-slint.yml').read_text(encoding='utf-8')
        self.assertNotIn('branches: [main, dev]', historical)
        self.assertIn("tags: ['v0.4.8']", historical)
        self.assertIn('branches: [main, dev]', current)
    def new_descriptor(self):
        return dict(self.descriptor(), version='0.5.2', download_url='https://github.com/chenjicheng/upmc/releases/download/v0.5.2/updater.exe')

    def test_exact_official_tag_only(self):
        self.assertTrue(pub.publication_allowed('push', 'refs/tags/v0.5.2', 'chenjicheng/upmc'))
        for event, ref, repo in [('workflow_dispatch','refs/tags/v0.5.2','chenjicheng/upmc'), ('push','refs/heads/main','chenjicheng/upmc'), ('push','refs/tags/v0.4.8','chenjicheng/upmc'), ('push','refs/tags/v0.5.1','chenjicheng/upmc'), ('push','refs/tags/v0.5.2','other/upmc')]:
            self.assertFalse(pub.publication_allowed(event, ref, repo))

    def test_release_workflow_uses_new_publisher_and_exact_tag(self):
        workflow = (Path(__file__).resolve().parents[2] / '.github/workflows/release-slint.yml').read_text(encoding='utf-8')
        self.assertIn("tags: ['v0.5.2']", workflow)
        self.assertIn('release_publication.py publish', workflow)
        self.assertIn("github.ref == 'refs/tags/v0.5.2'", workflow)
        self.assertNotIn('slint/live-preview', workflow)
        self.assertNotIn('--clobber', workflow)

    def test_hash_and_size_come_from_artifact(self):
        expected = self.new_descriptor()
        self.assertEqual(pub.make_descriptor(self.artifact, '0.5.2', 'v0.5.2', self.sha, expected['download_url']), expected)

    def pages_fixture(self):
        pages = self.root / 'pages'; init_repo(pages)
        (pages / 'CNAME').write_text('upmc.chenjicheng.cn\n')
        for name in ('version.json', 'dev/version.json', 'bridge/version.json', 'bridge/dev/version.json', '.upmc-legacy-transition.json'):
            path = pages / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text(json.dumps(self.descriptor()))
        (pages / 'unrelated.txt').write_text('preserve')
        git(pages, 'add', '.'); git(pages, 'commit', '-m', 'frozen transition')
        remote = self.root / 'remote.git'; git(self.root, 'init', '--bare', str(remote))
        git(pages, 'remote', 'add', 'origin', str(remote)); git(pages, 'push', 'origin', 'gh-pages')
        return pages, remote, git(pages, 'rev-parse', 'HEAD')

    def test_promotion_changes_only_bridge_and_release_marker(self):
        pages, remote, head = self.pages_fixture()
        result = pub.publish_pages(pages, self.new_descriptor(), head)
        changed = git(remote, 'diff-tree', '--no-commit-id', '--name-only', '-r', result).splitlines()
        self.assertEqual(set(changed), {'bridge/version.json', 'bridge/dev/version.json', '.upmc-release-0.5.2.json'})
        for name in ('version.json', 'dev/version.json', '.upmc-legacy-transition.json', 'unrelated.txt'):
            self.assertEqual(git(remote, 'show', f'{result}:{name}'), git(pages, 'show', f'{head}:{name}'))
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)
        self.assertEqual(git(pages, 'status', '--porcelain'), '')
        self.assertEqual(json.loads(git(remote, 'show', f'{result}:bridge/version.json')), self.new_descriptor())

    def test_unknown_bridge_and_missing_frozen_entry_fail_without_push(self):
        pages, remote, head = self.pages_fixture()
        (pages / 'bridge/version.json').write_text(json.dumps(dict(self.descriptor(), version='0.6.0')))
        git(pages, 'add', '.'); git(pages, 'commit', '-m', 'newer bridge'); git(pages, 'push', 'origin', 'gh-pages')
        head = git(pages, 'rev-parse', 'HEAD')
        with self.assertRaises(pub.PublicationError): pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_http_failure_does_not_create_release(self):
        with patch.object(pub, '_verify_remote_tag'), patch.object(pub, 'lookup_release', side_effect=pub.PublicationError('HTTP 403')), patch.object(pub, '_run') as run:
            with self.assertRaises(pub.PublicationError): pub.ensure_release(self.artifact, '0.5.2', 'v0.5.2', self.sha)
            run.assert_not_called()

    def test_remote_release_contract_against_new_publisher(self):
        import test_legacy_publication as legacy
        with patch.multiple(legacy, pub=pub, VERSION='0.5.2', TAG='v0.5.2', URL=pub.DOWNLOAD_URL):
            result = unittest.TestResult()
            unittest.defaultTestLoader.loadTestsFromTestCase(legacy.ReleaseTests).run(result)
        self.assertEqual(result.testsRun, 6)
        self.assertEqual(result.errors + result.failures, [])

    def test_rerun_is_idempotent_and_does_not_rewind_later_promotion(self):
        pages, remote, head = self.pages_fixture()
        published = pub.publish_pages(pages, self.new_descriptor(), head)
        git(pages, 'fetch', 'origin', 'gh-pages')
        git(pages, 'merge', '--ff-only', 'FETCH_HEAD')
        self.assertEqual(pub.publish_pages(pages, self.new_descriptor(), published), published)
        (pages / 'bridge/version.json').write_text(json.dumps(dict(self.new_descriptor(), version='0.6.0')))
        git(pages, 'add', '.'); git(pages, 'commit', '-m', 'later'); git(pages, 'push', 'origin', 'gh-pages')
        head = git(pages, 'rev-parse', 'HEAD')
        with self.assertRaises(pub.PublicationError): pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_lease_race_leaves_competing_commit_intact(self):
        pages, remote, head = self.pages_fixture()
        run = subprocess.run
        def race(args, **kwargs):
            if 'push' in args:
                other = git(remote, '-c', 'user.name=Race', '-c', 'user.email=race@example.invalid', 'commit-tree', head + '^{tree}', '-p', head, '-m', 'race')
                git(remote, 'update-ref', 'refs/heads/gh-pages', other, head)
            return run(args, **kwargs)
        with patch.object(subprocess, 'run', race), self.assertRaises(pub.PublicationError):
            pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(json.loads(git(remote, 'show', 'gh-pages:bridge/version.json')), self.descriptor())
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)

    def test_missing_frozen_entry_prevents_publication(self):
        pages, remote, head = self.pages_fixture()
        (pages / 'dev/version.json').unlink()
        git(pages, 'add', '-A'); git(pages, 'commit', '-m', 'missing'); git(pages, 'push', 'origin', 'gh-pages')
        head = git(pages, 'rev-parse', 'HEAD')
        with self.assertRaises(pub.PublicationError): pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
