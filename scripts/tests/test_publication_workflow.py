"""CI boundary behavior plus supplemental workflow wiring checks."""
from pathlib import Path
import unittest

from test_legacy_publication import pub


class WorkflowTests(unittest.TestCase):
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
