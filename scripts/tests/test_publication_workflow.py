"""CI boundary behavior plus supplemental workflow wiring checks."""
from pathlib import Path
import os
import shutil
import subprocess
import sys
import unittest

from test_legacy_publication import pub


class WorkflowTests(unittest.TestCase):
    def test_ci_context_cli_reads_the_actual_event_environment(self):
        helper = Path(__file__).resolve().parents[1] / "legacy_publication.py"
        for event, expected in (("push", "true"), ("workflow_dispatch", "false")):
            env = dict(os.environ, GITHUB_EVENT_NAME=event, GITHUB_REF="refs/tags/v0.4.8",
                       GITHUB_REPOSITORY="chenjicheng/upmc")
            result = subprocess.run([sys.executable, str(helper), "ci-context"],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "publish=" + expected)

    def test_actual_pages_activation_step_reports_failure_and_does_not_continue(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/build-updater.yml").read_text(encoding="utf-8")
        commands = [line.strip().removeprefix("run: ") for line in workflow.splitlines()
                    if line.strip().startswith("run: gh api ") and "/pages/builds" in line]
        self.assertEqual(len(commands), 1)
        self.assertIn("pages: write", workflow)
        bash = str(Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/bin/bash.exe") if os.name == "nt" else shutil.which("bash")
        self.assertTrue(bash and Path(bash).is_file(), "Git Bash or bash is required for the activation step contract test")
        for exit_code in (0, 23):
            # Run the actual workflow command under Actions' bash error mode.
            # Only the external gh effect is replaced with a shell fixture.
            script = ("gh() { printf '%s\\n' \"$@\"; return " + str(exit_code) + "; }\n"
                      + commands[0] + "\nprintf 'ACTIVATION_STEP_COMPLETED\\n'\n")
            result = subprocess.run([bash, "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
                                    capture_output=True, text=True)
            self.assertEqual(result.returncode, exit_code, result.stderr)
            self.assertIn("POST", result.stdout)
            self.assertIn("repos/chenjicheng/upmc/pages/builds", result.stdout)
            self.assertEqual("ACTIVATION_STEP_COMPLETED" in result.stdout, exit_code == 0)

    def test_only_official_repository_exact_tag_push_can_publish(self):
        self.assertTrue(pub.publication_allowed("push", "refs/tags/v0.4.8", "chenjicheng/upmc"))
        for event, ref, repository in [
            ("push", "refs/heads/dev", "chenjicheng/upmc"),
            ("push", "refs/heads/main", "chenjicheng/upmc"),
            ("workflow_dispatch", "refs/tags/v0.4.8", "chenjicheng/upmc"),
            ("pull_request", "refs/tags/v0.4.8", "chenjicheng/upmc"),
            ("push", "refs/tags/v0.6.0", "chenjicheng/upmc"),
            ("push", "refs/tags/v0.4.8-dev", "chenjicheng/upmc"),
            ("push", "refs/tags/v0.4.8", "someone/upmc"),
            ("", "", ""),
        ]:
            with self.subTest(event=event, ref=ref, repository=repository):
                self.assertFalse(pub.publication_allowed(event, ref, repository))

    def test_workflow_uses_actual_artifact_and_guarded_publisher(self):
        workflow = (Path(__file__).resolve().parents[2] / ".github/workflows/build-updater.yml").read_text(encoding="utf-8")
        self.assertIn("legacy_publication.py ci-context", workflow)
        self.assertIn("legacy_publication.py publish", workflow)
        self.assertIn("needs.build.outputs.publish == 'true'", workflow)
        self.assertIn("github.event_name == 'push'", workflow)
        self.assertIn("github.ref == 'refs/tags/v0.4.8'", workflow)
        self.assertIn("actions/download-artifact@v4", workflow)
        self.assertIn("--artifact _artifacts/updater.exe", workflow)
        self.assertIn("if-no-files-found: error", workflow)
        self.assertIn("group: upmc-pages-publication", workflow)
        self.assertIn("contents: read", workflow)
        self.assertNotIn("dev-latest", workflow)
        self.assertNotIn("--clobber", workflow)
        self.assertNotIn("TUF", workflow)
        self.assertNotIn("continue-on-error", workflow)
        for line in workflow.splitlines():
            if "cargo build " in line or "cargo test " in line:
                self.assertIn("--locked", line)


if __name__ == "__main__":
    unittest.main()
