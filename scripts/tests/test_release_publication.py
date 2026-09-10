"""0.5 promotion must preserve frozen legacy first-hop descriptors."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tomllib
import unittest
import subprocess
from unittest.mock import patch
from test_legacy_publication import Fixtures, git, init_repo

class ReleaseTests(Fixtures):
    fixture_current = '9.2.0'
    fixture_predecessor = '8.9.4'

    def setUp(self):
        super().setUp()
        policy_root = self.root / 'publisher-policy'
        (policy_root / 'scripts').mkdir(parents=True)
        (policy_root / 'upmc').mkdir()
        helper = policy_root / 'scripts/release_publication.py'
        helper.write_bytes((Path(__file__).resolve().parents[1] / 'release_publication.py').read_bytes())
        self.write_policy(policy_root / 'upmc/Cargo.toml', self.fixture_predecessor)
        isolated = importlib.util.spec_from_file_location('isolated_release_pub', helper)
        self.pub = importlib.util.module_from_spec(isolated)
        isolated.loader.exec_module(self.pub)
        # Expected identities come from independent fixture inputs, never from
        # the publisher's actual constants or the live project version.
        self.current_tag = 'v' + self.fixture_current
        self.current_ref = 'refs/tags/' + self.current_tag
        self.current_url = f'https://github.com/chenjicheng/upmc/releases/download/{self.current_tag}/updater.exe'
        self.current_marker = f'.upmc-release-{self.fixture_current}.json'
        self.predecessor_tag = 'v' + self.fixture_predecessor
        self.predecessor_ref = 'refs/tags/' + self.predecessor_tag
        self.predecessor_url = f'https://github.com/chenjicheng/upmc/releases/download/{self.predecessor_tag}/updater.exe'
        self.predecessor_marker = f'.upmc-release-{self.fixture_predecessor}.json'

    def write_policy(self, path, predecessor):
        path.write_text(f'[package]\nname = "fixture"\nversion = "{self.fixture_current}"\n'
                        f'[package.metadata.upmc-release]\npredecessor = "{predecessor}"\n', encoding='utf-8')

    def assert_frozen_documents_rejected(self, mutations, valid_size):
        pages, remote, _ = self.pages_fixture()
        names = ('version.json', 'dev/version.json', '.upmc-legacy-transition.json', *self.pub.PATHS)
        for name in names:
            value = dict(self.descriptor(), size=valid_size)
            value.update(mutations.get(name, {}))
            (pages / name).write_text(json.dumps(value))
        head = self.commit_pages(pages)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)
        self.assertEqual(git(pages, 'status', '--porcelain'), '')

    def test_frozen_documents_reject_float_sizes_before_comparison(self):
        original_root = self.root
        groups = [('bridge/version.json', 'bridge/dev/version.json'),
                  ('version.json',), ('dev/version.json',), ('.upmc-legacy-transition.json',)]
        for index, names in enumerate(groups):
            with self.subTest(paths=names):
                self.root = original_root / str(index); self.root.mkdir()
                self.assert_frozen_documents_rejected({name: {'size': float(len(self.payload))} for name in names}, len(self.payload))

    def test_frozen_documents_reject_boolean_sizes_before_comparison(self):
        original_root = self.root
        groups = [('bridge/version.json', 'bridge/dev/version.json'),
                  ('version.json',), ('dev/version.json',), ('.upmc-legacy-transition.json',)]
        for index, names in enumerate(groups):
            with self.subTest(paths=names):
                self.root = original_root / str(index); self.root.mkdir()
                self.assert_frozen_documents_rejected({name: {'size': True} for name in names}, 1)

    def test_frozen_marker_schema_is_validated_even_when_all_documents_match(self):
        original_root = self.root
        for index, invalid in enumerate(({'build_id': 'short'}, {'sha256': 'bad'},
                                        {'download_url': self.current_url}, {'extra': 'invalid'})):
            with self.subTest(invalid=invalid):
                self.root = original_root / str(index); self.root.mkdir()
                names = ('version.json', 'dev/version.json', '.upmc-legacy-transition.json', *self.pub.PATHS)
                self.assert_frozen_documents_rejected({name: invalid for name in names}, len(self.payload))

    def predecessor(self):
        return dict(self.descriptor(), version=self.fixture_predecessor,
                    download_url=self.predecessor_url)

    def commit_pages(self, pages):
        git(pages, 'add', '-A'); git(pages, 'commit', '-m', 'publication fixture')
        git(pages, 'push', 'origin', 'gh-pages')
        return git(pages, 'rev-parse', 'HEAD')

    def predecessor_fixture(self):
        pages, remote, _ = self.pages_fixture()
        historical = dict(self.descriptor(), version='0.5.1',
                          download_url='https://github.com/chenjicheng/upmc/releases/download/v0.5.1/updater.exe')
        (pages / '.upmc-release-0.5.1.json').write_text(json.dumps(historical))
        for name in (*self.pub.PATHS, self.predecessor_marker):
            (pages / name).write_text(json.dumps(self.predecessor()))
        return pages, remote, self.commit_pages(pages)

    def test_known_predecessor_promotes_atomically_and_preserves_all_history(self):
        pages, remote, head = self.predecessor_fixture()
        result = self.pub.publish_pages(pages, self.new_descriptor(), head)
        changed = git(remote, 'diff-tree', '--no-commit-id', '--name-only', '-r', result).splitlines()
        self.assertEqual(set(changed), {*self.pub.PATHS, self.current_marker})
        for name in ('CNAME', 'version.json', 'dev/version.json', '.upmc-legacy-transition.json',
                     '.upmc-release-0.5.1.json', self.predecessor_marker, 'unrelated.txt'):
            self.assertEqual(git(remote, 'show', f'{result}:{name}'), git(pages, 'show', f'{head}:{name}'))
        for name in (*self.pub.PATHS, self.current_marker):
            self.assertEqual(json.loads(git(remote, 'show', f'{result}:{name}')), self.new_descriptor())
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)
        self.assertEqual(git(pages, 'status', '--porcelain'), '')

    def test_predecessor_without_marker_is_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        (pages / self.predecessor_marker).unlink()
        head = self.commit_pages(pages)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_current_version_without_its_marker_is_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for name in self.pub.PATHS:
            (pages / name).write_text(json.dumps(self.new_descriptor()))
        head = self.commit_pages(pages)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_predecessor_feed_disagreement_and_marker_mismatch_are_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for name in ('bridge/dev/version.json', self.predecessor_marker):
            with self.subTest(path=name):
                (pages / name).write_text(json.dumps(dict(self.predecessor(), sha256='1' * 64)))
                head = self.commit_pages(pages)
                with self.assertRaises(self.pub.PublicationError):
                    self.pub.publish_pages(pages, self.new_descriptor(), head)
                self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
                (pages / name).write_text(json.dumps(self.predecessor()))
                self.commit_pages(pages)

    def test_matching_but_invalid_predecessor_descriptors_are_rejected(self):
        pages, remote, _ = self.predecessor_fixture()
        for change in ({'version': '99.0.0'}, {'version': '0.5.0'}, {'size': True},
                       {'size': 0}, {'build_id': 'short'}, {'sha256': 'bad'},
                       {'download_url': self.current_url}, {'extra': 'not allowed'}):
            with self.subTest(change=change):
                for name in (*self.pub.PATHS, self.predecessor_marker):
                    (pages / name).write_text(json.dumps(dict(self.predecessor(), **change)))
                head = self.commit_pages(pages)
                with self.assertRaises(self.pub.PublicationError):
                    self.pub.publish_pages(pages, self.new_descriptor(), head)
                self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_boolean_feed_size_cannot_match_integer_marker_size(self):
        pages, remote, _ = self.predecessor_fixture()
        (pages / self.predecessor_marker).write_text(json.dumps(dict(self.predecessor(), size=1)))
        for name in self.pub.PATHS:
            (pages / name).write_text(json.dumps(dict(self.predecessor(), size=True)))
        head = self.commit_pages(pages)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)
        current = dict(self.new_descriptor(), size=1)
        (pages / self.current_marker).write_text(json.dumps(current))
        for name in self.pub.PATHS:
            (pages / name).write_text(json.dumps(dict(current, size=True)))
        head = self.commit_pages(pages)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, current, head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_exact_fixture_version_and_download_identity(self):
        for version, tag, url in [(self.fixture_predecessor, self.predecessor_tag, self.predecessor()['download_url']),
                                  (self.fixture_current, self.current_tag, self.predecessor()['download_url']),
                                  (self.fixture_current, self.predecessor_tag, self.new_descriptor()['download_url'])]:
            with self.subTest(version=version, tag=tag, url=url), self.assertRaises(self.pub.PublicationError):
                self.pub.make_descriptor(self.artifact, version, tag, self.sha, url)

    def test_current_source_tag_commit_and_stable_channel_are_required(self):
        self.write_policy(self.source / 'upmc/Cargo.toml', self.fixture_predecessor)
        git(self.source, 'add', '.'); git(self.source, 'commit', '-m', 'current release')
        current = git(self.source, 'rev-parse', 'HEAD')
        git(self.source, 'tag', self.current_tag)
        self.assertEqual(self.pub.validate_source(self.source, self.current_tag, current, self.current_ref, 'stable'), self.fixture_current)
        for tag, sha, ref, channel in [(self.predecessor_tag, current, self.predecessor_ref, 'stable'),
                                       (self.current_tag, self.sha, self.current_ref, 'stable'),
                                       (self.current_tag, current, 'refs/heads/main', 'stable'),
                                       (self.current_tag, current, self.current_ref, 'dev')]:
            with self.subTest(tag=tag, sha=sha, ref=ref, channel=channel), self.assertRaises(self.pub.PublicationError):
                self.pub.validate_source(self.source, tag, sha, ref, channel)
    def test_source_predecessor_policy_must_match_the_publisher_checkout(self):
        self.write_policy(self.source / 'upmc/Cargo.toml', '0.5.1')
        git(self.source, 'add', '.'); git(self.source, 'commit', '-m', 'different policy')
        current = git(self.source, 'rev-parse', 'HEAD')
        git(self.source, 'tag', self.current_tag)
        with self.assertRaises(self.pub.PublicationError):
            self.pub.validate_source(self.source, self.current_tag, current, self.current_ref, 'stable')
    def test_historical_workflow_does_not_duplicate_branch_builds(self):
        root = Path(__file__).resolve().parents[2]
        historical = (root / '.github/workflows/build-updater.yml').read_text(encoding='utf-8')
        current = (root / '.github/workflows/release-slint.yml').read_text(encoding='utf-8')
        self.assertNotIn('branches: [main, dev]', historical)
        self.assertIn("tags: ['v0.4.8']", historical)
        self.assertIn('branches: [main, dev]', current)
    def new_descriptor(self):
        return dict(self.descriptor(), version=self.fixture_current, download_url=self.current_url)

    def test_exact_official_tag_only(self):
        self.assertTrue(self.pub.publication_allowed('push', self.current_ref, 'chenjicheng/upmc'))
        for event, ref, repo in [('workflow_dispatch',self.current_ref,'chenjicheng/upmc'), ('push','refs/heads/main','chenjicheng/upmc'), ('push','refs/tags/v0.4.8','chenjicheng/upmc'), ('push',self.predecessor_ref,'chenjicheng/upmc'), ('push',self.current_ref,'other/upmc')]:
            self.assertFalse(self.pub.publication_allowed(event, ref, repo))

    def test_release_workflow_uses_new_publisher_and_exact_tag(self):
        workflow = (Path(__file__).resolve().parents[2] / '.github/workflows/release-slint.yml').read_text(encoding='utf-8')
        self.assertIn("tags: ['v*']", workflow)
        self.assertIn('release_publication.py publish', workflow)
        self.assertIn("github.ref == format('refs/tags/{0}', needs.build.outputs.release_tag)", workflow)
        self.assertIn('release_tag: ${{ steps.context.outputs.release_tag }}', workflow)
        self.assertIn("needs.build.outputs.publish == 'true'", workflow)
        self.assertNotIn('slint/live-preview', workflow)
        self.assertNotIn('--clobber', workflow)

    def test_hash_and_size_come_from_artifact(self):
        expected = self.new_descriptor()
        self.assertEqual(self.pub.make_descriptor(self.artifact, self.fixture_current, self.current_tag, self.sha, expected['download_url']), expected)

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
        result = self.pub.publish_pages(pages, self.new_descriptor(), head)
        changed = git(remote, 'diff-tree', '--no-commit-id', '--name-only', '-r', result).splitlines()
        self.assertEqual(set(changed), {'bridge/version.json', 'bridge/dev/version.json', self.current_marker})
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
        with self.assertRaises(self.pub.PublicationError): self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_http_failure_does_not_create_release(self):
        with patch.object(self.pub, '_verify_remote_tag'), patch.object(self.pub, 'lookup_release', side_effect=self.pub.PublicationError('HTTP 403')), patch.object(self.pub, '_run') as run:
            with self.assertRaises(self.pub.PublicationError): self.pub.ensure_release(self.artifact, self.fixture_current, self.current_tag, self.sha)
            run.assert_not_called()

    def test_remote_release_contract_against_new_publisher(self):
        import test_legacy_publication as legacy
        with patch.multiple(legacy, pub=self.pub, VERSION=self.fixture_current, TAG=self.current_tag, URL=self.current_url):
            result = unittest.TestResult()
            unittest.defaultTestLoader.loadTestsFromTestCase(legacy.ReleaseTests).run(result)
        self.assertEqual(result.testsRun, 6)
        self.assertEqual(result.errors + result.failures, [])

    def test_rerun_is_idempotent_and_does_not_rewind_later_promotion(self):
        pages, remote, head = self.pages_fixture()
        published = self.pub.publish_pages(pages, self.new_descriptor(), head)
        git(pages, 'fetch', 'origin', 'gh-pages')
        git(pages, 'merge', '--ff-only', 'FETCH_HEAD')
        self.assertEqual(self.pub.publish_pages(pages, self.new_descriptor(), published), published)
        (pages / 'bridge/version.json').write_text(json.dumps(dict(self.new_descriptor(), version='99.0.0')))
        git(pages, 'add', '.'); git(pages, 'commit', '-m', 'later'); git(pages, 'push', 'origin', 'gh-pages')
        head = git(pages, 'rev-parse', 'HEAD')
        with self.assertRaises(self.pub.PublicationError): self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)

    def test_lease_race_leaves_competing_commit_intact(self):
        pages, remote, head = self.pages_fixture()
        run = subprocess.run
        def race(args, **kwargs):
            if 'push' in args:
                other = git(remote, '-c', 'user.name=Race', '-c', 'user.email=race@example.invalid', 'commit-tree', head + '^{tree}', '-p', head, '-m', 'race')
                git(remote, 'update-ref', 'refs/heads/gh-pages', other, head)
            return run(args, **kwargs)
        with patch.object(subprocess, 'run', race), self.assertRaises(self.pub.PublicationError):
            self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(json.loads(git(remote, 'show', 'gh-pages:bridge/version.json')), self.descriptor())
        self.assertEqual(git(pages, 'rev-parse', 'HEAD'), head)

    def test_missing_frozen_entry_prevents_publication(self):
        pages, remote, head = self.pages_fixture()
        (pages / 'dev/version.json').unlink()
        git(pages, 'add', '-A'); git(pages, 'commit', '-m', 'missing'); git(pages, 'push', 'origin', 'gh-pages')
        head = git(pages, 'rev-parse', 'HEAD')
        with self.assertRaises(self.pub.PublicationError): self.pub.publish_pages(pages, self.new_descriptor(), head)
        self.assertEqual(git(remote, 'rev-parse', 'gh-pages'), head)


class UnrelatedPolicyReleaseTests(ReleaseTests):
    """Run every release contract against another nonadjacent policy pair."""
    fixture_current = '17.6.2'
    fixture_predecessor = '3.8.11'


class ManifestConsistencyTests(unittest.TestCase):
    def test_lock_matches_manifest_and_real_ci_context_reads_it(self):
        root = Path(__file__).resolve().parents[2]
        with (root / 'upmc/Cargo.toml').open('rb') as stream:
            current = tomllib.load(stream)['package']['version']
        with (root / 'Cargo.lock').open('rb') as stream:
            packages = tomllib.load(stream)['package']
        self.assertEqual([p['version'] for p in packages if p['name'] == 'upmc'], [current])
        env = dict(os.environ, GITHUB_EVENT_NAME='push', GITHUB_REF='refs/tags/v' + current,
                   GITHUB_REPOSITORY='chenjicheng/upmc', UPMC_CHANNEL='stable')
        result = subprocess.run([sys.executable, '-X', 'utf8', str(root / 'scripts/release_publication.py'), 'ci-context'],
                                env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['publish=true', 'release_tag=v' + current])
