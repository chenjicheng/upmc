"""Release authorization must use canonical runs, never an unrelated green badge."""
import copy
import importlib.util
import json
from pathlib import Path
import sys
import subprocess
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import release_test_gate as gate
import release_publication as publisher

SHA = "a" * 40
REPO = "chenjicheng/upmc"
POLICIES = [
    (".github/workflows/validate-updater.yml", "push", ["UPMC tests"]),
    (".github/workflows/release-slint.yml", "push", ["build"]),
    ("dynamic/github-code-scanning/codeql", "dynamic", ["Analyze (actions)", "Analyze (rust)", "Analyze (python)"]),
]


class ApiFixture:
    def __init__(self):
        self.calls = []
        self.workflows = []
        self.runs = {}
        self.jobs = {}
        for index, (path, event, names) in enumerate(POLICIES, 1):
            self.workflows.append(dict(id=index, path=path, state="active"))
            run = dict(id=100 + index, workflow_id=index, path=path, head_branch="main",
                       head_sha=SHA, event=event, status="completed", conclusion="success",
                       run_attempt=1, run_number=1, repository={"full_name": REPO}, head_repository={"full_name": REPO})
            self.runs[index] = [run]
            self.jobs[run["id"]] = [dict(id=1000 + index * 10 + n, name=name, run_id=run["id"],
                run_attempt=1, head_sha=SHA, status="completed", conclusion="success") for n, name in enumerate(names)]

    def __call__(self, endpoint):
        self.calls.append(endpoint)
        from urllib.parse import parse_qs, urlsplit
        parsed = urlsplit(endpoint)
        path = parsed.path
        page = int(parse_qs(parsed.query).get("page", [1])[0])
        def paged(key, items):
            return {"total_count": len(items), key: copy.deepcopy(items[(page - 1)*100:page*100])}
        if path.endswith("/actions/workflows"):
            return paged("workflows", self.workflows)
        if "/workflows/" in path:
            workflow = int(path.split("/workflows/")[1].split("/")[0])
            return paged("workflow_runs", self.runs[workflow])
        run_id = int(path.split("/runs/")[1].split("/")[0])
        if path.endswith("/jobs"):
            return paged("jobs", self.jobs[run_id])
        return copy.deepcopy(next(r for runs in self.runs.values() for r in runs if r["id"] == run_id))


class GateTests(unittest.TestCase):
    def test_exact_main_success_requires_validation_and_current_jobs(self):
        api = ApiFixture()
        evidence = gate.require_green(SHA, api)
        self.assertEqual([item["workflow"] for item in evidence], [POLICIES[0][0]])
        self.assertTrue(any("/attempts/1/jobs" in call for call in api.calls))

    def test_missing_pending_failed_cancelled_and_skipped_runs_never_authorize(self):
        for state in ["missing", "queued", "in_progress", "failure", "cancelled", "skipped", "neutral", "timed_out", "action_required"]:
            with self.subTest(state=state):
                api = ApiFixture()
                if state == "missing": api.runs[1] = []
                elif state in {"queued", "in_progress"}: api.runs[1][0].update(status=state, conclusion=None)
                else: api.runs[1][0]["conclusion"] = state
                with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)

    def test_wrong_event_branch_sha_repository_workflow_or_path_is_not_evidence(self):
        for change in [dict(event="pull_request"), dict(event="workflow_dispatch"), dict(head_branch="v9.0.0"),
                       dict(head_sha="b"*40), dict(head_repository={"full_name":"other/upmc"}),
                       dict(repository={"full_name":"other/upmc"}), dict(workflow_id=99), dict(path="other.yml")]:
            with self.subTest(change=change):
                api = ApiFixture(); api.runs[1][0].update(change)
                with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)

    def test_newer_failure_or_new_rerun_cannot_reuse_old_success(self):
        api = ApiFixture()
        newer = dict(api.runs[1][0], id=201, run_number=2, conclusion="failure")
        api.runs[1].append(newer)
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)
        api = ApiFixture(); api.runs[1][0]["run_attempt"] = 2
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)

    def test_missing_duplicate_failed_or_foreign_jobs_fail_closed(self):
        for change in ["missing", "duplicate", "failure", "skipped", "sha", "run", "attempt"]:
            with self.subTest(change=change):
                api = ApiFixture(); jobs = api.jobs[101]
                if change == "missing": jobs.clear()
                elif change == "duplicate": jobs.append(dict(jobs[0], id=999))
                elif change in {"failure", "skipped"}: jobs[0]["conclusion"] = change
                elif change == "sha": jobs[0]["head_sha"] = "b"*40
                elif change == "run": jobs[0]["run_id"] = 999
                else: jobs[0]["run_attempt"] = 2
                with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)

    def test_obsolete_main_build_and_advisory_scans_do_not_block_release(self):
        for state in ("missing", "in_progress", "failure"):
            with self.subTest(state=state):
                api = ApiFixture()
                for workflow in (2, 3):
                    if state == "missing":
                        api.runs[workflow] = []
                    elif state == "in_progress":
                        api.runs[workflow][0].update(status=state, conclusion=None)
                    else:
                        api.runs[workflow][0]["conclusion"] = state
                self.assertEqual(len(gate.require_green(SHA, api)), 1)
                self.assertFalse(any("/workflows/2/" in call or "/workflows/3/" in call
                                     for call in api.calls))

    def test_only_validation_workflow_needs_to_be_registered(self):
        api = ApiFixture()
        api.workflows = api.workflows[:1]
        self.assertEqual(len(gate.require_green(SHA, api)), 1)

    def test_validation_has_no_allowed_skipped_jobs(self):
        api = ApiFixture()
        api.jobs[101].append(dict(api.jobs[101][0], id=999, name="extra", conclusion="skipped"))
        with self.assertRaises(gate.ReleaseGateError):
            gate.require_green(SHA, api)

    def test_pagination_finds_later_failed_run_and_rejects_incomplete_responses(self):
        api = ApiFixture()
        api.runs[1] = [dict(api.runs[1][0], id=n, run_number=n) for n in range(1000, 1101)]
        api.runs[1][-1]["conclusion"] = "failure"
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)
        self.assertTrue(any("page=2" in call for call in api.calls))
        for response in [{}, {"total_count": 1, "workflows": []}, {"total_count": True, "workflows": []}]:
            with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, lambda _: response)

    def test_run_rechecked_after_jobs_rejects_rerun_started_during_gate(self):
        api = ApiFixture()
        def racing(endpoint):
            result = api(endpoint)
            if endpoint.endswith("/runs/101"):
                result.update(run_attempt=2, status="in_progress", conclusion=None)
            return result
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, racing)

    def test_latest_uses_workflow_run_number_not_opaque_database_id(self):
        api = ApiFixture()
        api.runs[1].append(dict(api.runs[1][0], id=50, run_number=2, conclusion="failure"))
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, api)

    def test_fresh_lookup_cannot_substitute_another_run(self):
        api = ApiFixture()
        def mismatched(endpoint):
            result = api(endpoint)
            if endpoint.endswith("/runs/101"): result["id"] = 999
            return result
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, mismatched)

    def test_api_error_and_invalid_sha_fail_without_fallback(self):
        for sha in ["", "main", "A"*40, "a"*39]:
            with self.assertRaises(gate.ReleaseGateError): gate.require_green(sha, ApiFixture())
        with self.assertRaises(gate.ReleaseGateError): gate.require_green(SHA, lambda _: (_ for _ in ()).throw(OSError("offline")))

    def test_publisher_cli_gate_failure_precedes_every_mutating_operation(self):
        args = ["release_publication.py", "publish", "--source", ".", "--tag", publisher.TAG,
                "--build-id", SHA, "--ref", "refs/tags/" + publisher.TAG, "--channel", "stable",
                "--artifact", "fixture.exe", "--pages", "fixture-pages", "--expected-pages-head", "b"*40]
        with patch.object(sys, "argv", args), patch.object(publisher, "validate_source", return_value=publisher.VERSION), \
             patch.object(gate, "require_green", side_effect=gate.ReleaseGateError("required tests are red")), \
             patch.object(publisher, "ensure_release", return_value={}) as release, \
             patch.object(publisher, "publish_pages", return_value="done") as pages:
            self.assertEqual(publisher.main(), 1)
            release.assert_not_called(); pages.assert_not_called()


class WorkflowGateTests(unittest.TestCase):
    def test_required_validation_is_independent_of_tag_and_manual_publication(self):
        path = ROOT / ".github/workflows/validate-updater.yml"
        self.assertTrue(path.exists(), "required PR/main test workflow is missing")
        text = path.read_text(encoding="utf-8")
        self.assertIn("name: UPMC tests", text)
        self.assertIn("pull_request:", text)
        self.assertIn("branches: [main]", text)
        self.assertNotIn("workflow_dispatch", text); self.assertNotIn("tags:", text)
        self.assertIn("cargo test --locked --workspace", text)
        self.assertIn("python -m unittest discover -s scripts/tests -v", text)
        self.assertLess(text.index("cargo build --locked --release -p discord-proxy-dll -p force-proxy-dll"), text.index("cargo test --locked --workspace"))
        self.assertNotIn("continue-on-error", text); self.assertNotIn("secrets.", text)

    def test_publication_has_read_only_actions_permission_and_no_gate_cycle(self):
        text = (ROOT / ".github/workflows/release-slint.yml").read_text(encoding="utf-8")
        publish = text.split("\n  publish:", 1)[1]
        self.assertIn("actions: read", publish)
        self.assertIn("release_publication.py publish", publish)
        self.assertNotIn("release_test_gate.py", text.split("\n  publish:", 1)[0])
        self.assertNotIn("continue-on-error", text)

    def test_tags_build_once_without_repeating_canonical_tests(self):
        text = (ROOT / ".github/workflows/release-slint.yml").read_text(encoding="utf-8")
        self.assertNotIn("branches:", text)
        self.assertNotIn("cargo test", text)
        self.assertNotIn("unittest discover", text)
        self.assertNotIn("setup-java", text)
        self.assertIn("cargo build --locked --release -p upmc", text)
        self.assertIn("needs: build", text)
        self.assertIn("./scripts/sign-updater.ps1", text)
        self.assertIn("if-no-files-found: error", text)

    def test_only_stale_pr_validation_is_cancelled(self):
        text = (ROOT / ".github/workflows/validate-updater.yml").read_text(encoding="utf-8")
        self.assertIn("group: upmc-validation-${{ github.event.pull_request.number || github.sha }}", text)
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", text)


class GateWaitTests(unittest.TestCase):
    def setUp(self):
        self.now = 0.0
        self.sleeps = []

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        self.now += seconds

    def wait(self, api, timeout=30):
        with patch.object(gate.time, "monotonic", side_effect=lambda: self.now), \
             patch.object(gate.time, "sleep", side_effect=self.sleep):
            return gate.wait_for_green(SHA, api=api, timeout_seconds=timeout, poll_seconds=10)

    def test_tag_publication_waits_for_running_exact_main_tests(self):
        api = ApiFixture()
        api.runs[1][0].update(status="in_progress", conclusion=None)
        def progressing(endpoint):
            if self.now >= 20:
                api.runs[1][0].update(status="completed", conclusion="success")
            return api(endpoint)
        self.assertEqual(len(self.wait(progressing)), 1)
        self.assertEqual(self.sleeps, [10, 10])

    def test_main_run_can_appear_after_tag_build_starts(self):
        api = ApiFixture()
        run = api.runs[1].pop()
        def appearing(endpoint):
            if self.now >= 10 and not api.runs[1]:
                api.runs[1].append(run)
            return api(endpoint)
        self.assertEqual(len(self.wait(appearing)), 1)
        self.assertEqual(self.sleeps, [10])

    def test_missing_or_pending_validation_has_a_bounded_wait(self):
        for status in ("missing", "queued", "in_progress"):
            with self.subTest(status=status):
                self.setUp()
                api = ApiFixture()
                if status == "missing":
                    api.runs[1] = []
                else:
                    api.runs[1][0].update(status=status, conclusion=None)
                with self.assertRaisesRegex(gate.ReleaseGateError, "Timed out"):
                    self.wait(api, timeout=25)
                self.assertEqual(self.sleeps, [10, 10, 5])

    def test_failed_cancelled_or_malformed_validation_never_waits_or_authorizes(self):
        for change in (dict(conclusion="failure"), dict(conclusion="cancelled"),
                       dict(status="unknown", conclusion=None), dict(head_sha="b" * 40),
                       dict(status="queued", conclusion="success")):
            with self.subTest(change=change):
                api = ApiFixture()
                api.runs[1][0].update(change)
                with self.assertRaises(gate.ReleaseGateError):
                    self.wait(api)
                self.assertEqual(self.sleeps, [])

    def test_api_failure_does_not_turn_into_a_retry_or_old_success(self):
        def offline(_):
            raise OSError("offline")
        with self.assertRaises(gate.ReleaseGateError):
            self.wait(offline)
        self.assertEqual(self.sleeps, [])

    def test_api_response_after_deadline_cannot_authorize_release(self):
        api = ApiFixture()
        def slow(endpoint):
            self.now += 26
            return api(endpoint)
        with self.assertRaisesRegex(gate.ReleaseGateError, "Timed out"):
            self.wait(slow, timeout=25)
        self.assertEqual(len(api.calls), 1)
        self.assertEqual(self.sleeps, [])

    def test_live_api_timeout_is_capped_by_remaining_wait_budget(self):
        api = ApiFixture()
        timeouts = []
        def timed(endpoint, *, timeout):
            timeouts.append(timeout)
            self.assertEqual(timeout, 25 - self.now)
            self.now += 3
            return api(endpoint)
        with patch.object(gate, "_api", side_effect=timed):
            self.assertEqual(len(self.wait(None, timeout=25)), 1)
        self.assertTrue(timeouts)
        self.assertEqual(self.sleeps, [])

    def test_zero_wait_still_allows_one_fresh_validation_query(self):
        self.assertEqual(len(self.wait(ApiFixture(), timeout=0)), 1)
        self.assertEqual(self.sleeps, [])

    def test_rerun_while_waiting_must_have_matching_attempt_jobs(self):
        api = ApiFixture()
        api.runs[1][0].update(status="queued", conclusion=None)
        def rerunning(endpoint):
            if self.now >= 10:
                api.runs[1][0].update(status="completed", conclusion="success", run_attempt=2)
            return api(endpoint)
        with self.assertRaisesRegex(gate.ReleaseGateError, "Stale or foreign job"):
            self.wait(rerunning)
        self.assertEqual(self.sleeps, [10])

    def test_publisher_waits_before_any_release_or_pages_mutation(self):
        args = ["release_publication.py", "publish", "--source", ".", "--tag", publisher.TAG,
                "--build-id", SHA, "--ref", "refs/tags/" + publisher.TAG, "--channel", "stable",
                "--artifact", "fixture.exe", "--pages", "fixture-pages", "--expected-pages-head", "b" * 40]
        calls = []
        def waiting(sha):
            self.assertEqual(sha, SHA)
            calls.append("validation")
            return []
        with patch.object(sys, "argv", args), patch.object(publisher, "validate_source", return_value=publisher.VERSION), \
             patch.object(gate, "wait_for_green", side_effect=waiting), \
             patch.object(publisher, "ensure_release", side_effect=lambda *_: calls.append("release") or {}), \
             patch.object(publisher, "publish_pages", side_effect=lambda *_: calls.append("pages") or "done"):
            self.assertEqual(publisher.main(), 0)
        self.assertEqual(calls, ["validation", "release", "pages"])


class ApiEncodingTests(unittest.TestCase):
    def fake_gh(self, stdout, stderr=b"", exit_code=0):
        real_run = subprocess.run
        def run(_args, **kwargs):
            # A real child writes exactly the bytes gh would emit. Force a
            # legacy default decoder in the parent, independently of its host.
            program = ("import sys; sys.stdout.buffer.write(bytes.fromhex(" + repr(stdout.hex())
                       + ")); sys.stderr.buffer.write(bytes.fromhex(" + repr(stderr.hex())
                       + ")); sys.exit(" + str(exit_code) + ")")
            return real_run([sys.executable, "-c", program], **kwargs)
        return run

    def test_utf8_json_survives_legacy_parent_locale(self):
        expected = {"message": "修复发布检查 — 更新器 ✅"}
        payload = json.dumps(expected, ensure_ascii=False).encode("utf-8")
        with patch.object(subprocess, "_text_encoding", return_value="cp1252"), \
             patch.object(gate.subprocess, "run", side_effect=self.fake_gh(payload)):
            self.assertEqual(gate._api("repos/example/test"), expected)

    def test_invalid_utf8_response_is_a_typed_blocking_error(self):
        with patch.object(subprocess, "_text_encoding", return_value="cp1252"), \
             patch.object(gate.subprocess, "run", side_effect=self.fake_gh(b'{"message":"\xff"}')):
            with self.assertRaisesRegex(gate.ReleaseGateError, "UTF-8"):
                gate._api("repos/example/test")

    def test_utf8_error_diagnostic_is_preserved(self):
        with patch.object(subprocess, "_text_encoding", return_value="cp1252"), \
             patch.object(gate.subprocess, "run", side_effect=self.fake_gh(b"", "访问被拒绝".encode("utf-8"), 1)):
            with self.assertRaisesRegex(gate.ReleaseGateError, "访问被拒绝"):
                gate._api("repos/example/test")

    def test_transport_timeout_remains_bounded_and_typed(self):
        def timeout(args, **kwargs):
            self.assertEqual(kwargs["timeout"], 45)
            raise subprocess.TimeoutExpired(args, kwargs["timeout"])
        with patch.object(gate.subprocess, "run", side_effect=timeout):
            with self.assertRaises(gate.ReleaseGateError):
                gate.require_green(SHA)


if __name__ == "__main__":
    unittest.main()
