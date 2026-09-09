"""Release policy is read from the source manifest, not publisher literals."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


class PolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        (root / 'scripts').mkdir()
        (root / 'upmc').mkdir()
        self.helper = root / 'scripts/release_publication.py'
        self.helper.write_bytes((Path(__file__).resolve().parents[1] / 'release_publication.py').read_bytes())
        self.manifest = root / 'upmc/Cargo.toml'

    def policy(self, current='9.2.0', predecessor='8.9.4'):
        self.manifest.write_text('[package]\nversion = ' + json.dumps(current) +
                                 '\n[package.metadata.upmc-release]\npredecessor = ' +
                                 json.dumps(predecessor) + '\n', encoding='utf-8')

    def context(self, ref='refs/tags/v9.2.0', event='push', repo='chenjicheng/upmc', channel='stable'):
        env = dict(os.environ, GITHUB_EVENT_NAME=event, GITHUB_REF=ref,
                   GITHUB_REPOSITORY=repo, UPMC_CHANNEL=channel)
        if channel is None:
            env.pop('UPMC_CHANNEL')
        return subprocess.run([sys.executable, '-X', 'utf8', str(self.helper), 'ci-context'],
                              env=env, capture_output=True, text=True)

    def test_manifest_change_controls_ci_tag_and_all_release_identities(self):
        self.policy()
        result = self.context()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), ['publish=true', 'release_tag=v9.2.0'])
        inspect = 'import json,runpy,sys; p=runpy.run_path(sys.argv[1]); print(json.dumps([p[k] for k in ("VERSION","TAG","DOWNLOAD_URL","MARKER","PREDECESSOR_VERSION","PREDECESSOR_MARKER")]))'
        result = subprocess.run([sys.executable, '-X', 'utf8', '-c', inspect, str(self.helper)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), ['9.2.0', 'v9.2.0',
                         'https://github.com/chenjicheng/upmc/releases/download/v9.2.0/updater.exe',
                         '.upmc-release-9.2.0.json', '8.9.4', '.upmc-release-8.9.4.json'])

    def test_context_rejects_other_tags_manual_branches_repositories_and_channels(self):
        self.policy()
        cases = [{'ref': 'refs/tags/v0.5.3'}, {'ref': 'refs/tags/v9.2.1'}, {'ref': 'refs/tags/v8.9.4'},
                 {'ref': 'refs/tags/v9.2.0-beta'}, {'ref': 'refs/heads/main'},
                 {'event': 'workflow_dispatch'}, {'repo': 'other/upmc'}, {'channel': 'dev'}, {'channel': ''}, {'channel': None}]
        for case in cases:
            with self.subTest(case=case):
                result = self.context(**case)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.splitlines(), ['publish=false', 'release_tag=v9.2.0'])

    def test_missing_policy_and_manifest_fail_closed_even_for_old_exact_tag(self):
        for contents in (None, '[package]\nversion="0.5.3"\n',
                         '[package.metadata.upmc-release]\npredecessor="0.5.2"\n', 'invalid TOML!'):
            with self.subTest(contents=contents):
                if contents is None:
                    self.manifest.unlink(missing_ok=True)
                else:
                    self.manifest.write_text(contents, encoding='utf-8')
                result = self.context(ref='refs/tags/v0.5.3')
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('publish=true', result.stdout)

    def test_policy_rejects_nonstable_versions_and_nonolder_predecessors(self):
        invalid = ['', '1.2', '01.2.3', '1.2.3-beta', '1.2.3+build', '1.2.3\n', '1.2.3\nrelease_tag=evil', True, 123]
        for current, predecessor in ([(v, '0.5.2') for v in invalid] +
                                     [('0.5.3', v) for v in invalid] +
                                     [('0.5.3', '0.5.3'), ('0.5.3', '0.6.0'), ('2.9.0', '2.10.0')]):
            with self.subTest(current=current, predecessor=predecessor):
                self.policy(current, predecessor)
                result = self.context(ref='refs/tags/v0.5.3')
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn('publish=true', result.stdout)
