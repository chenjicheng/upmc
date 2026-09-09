"""0.5 promotion must preserve frozen legacy first-hop descriptors."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
from test_legacy_publication import Fixtures, git, init_repo

spec = importlib.util.spec_from_file_location('release_pub', Path(__file__).resolve().parents[1] / 'release_publication.py')
pub = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pub)

class ReleaseTests(Fixtures):
    def new_descriptor(self):
        return dict(self.descriptor(), version='0.5.0', download_url='https://github.com/chenjicheng/upmc/releases/download/v0.5.0/updater.exe')

    def test_exact_official_tag_only(self):
        self.assertTrue(pub.publication_allowed('push', 'refs/tags/v0.5.0', 'chenjicheng/upmc'))
        for event, ref, repo in [('workflow_dispatch','refs/tags/v0.5.0','chenjicheng/upmc'), ('push','refs/heads/main','chenjicheng/upmc'), ('push','refs/tags/v0.4.8','chenjicheng/upmc'), ('push','refs/tags/v0.5.0','other/upmc')]:
            self.assertFalse(pub.publication_allowed(event, ref, repo))

    def test_release_workflow_uses_new_publisher_and_exact_tag(self):
        workflow = (Path(__file__).resolve().parents[2] / '.github/workflows/release-slint.yml').read_text(encoding='utf-8')
        self.assertIn("tags: ['v0.5.0']", workflow)
        self.assertIn('release_publication.py publish', workflow)
        self.assertIn("github.ref == 'refs/tags/v0.5.0'", workflow)
        self.assertNotIn('slint/live-preview', workflow)
        self.assertNotIn('--clobber', workflow)

    def test_hash_and_size_come_from_artifact(self):
        expected = self.new_descriptor()
        self.assertEqual(pub.make_descriptor(self.artifact, '0.5.0', 'v0.5.0', self.sha, expected['download_url']), expected)

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
        self.assertEqual(set(changed), {'bridge/version.json', 'bridge/dev/version.json', '.upmc-release-0.5.0.json'})
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
            with self.assertRaises(pub.PublicationError): pub.ensure_release(self.artifact, '0.5.0', 'v0.5.0', self.sha)
            run.assert_not_called()
