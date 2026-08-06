#!/usr/bin/env python3
"""Tests for the MCPLS resident-budget benchmark."""

import json
import tempfile
import unittest
from pathlib import Path

from mcpls_residency import decode_sse, load_manifest


class ManifestTests(unittest.TestCase):
    def make_manifest(self, root_count=5, group_count=4):
        temporary = tempfile.TemporaryDirectory()
        roots = []
        for group in range(group_count):
            group_roots = []
            for root in range(root_count):
                path = Path(temporary.name) / f"repo-{group}-{root}"
                path.mkdir()
                (path / "Cargo.toml").write_text(
                    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n"
                )
                group_roots.append(str(path))
            roots.append({"project_id": f"repo-{group}", "roots": group_roots})
        return temporary, {"projects": roots}

    def test_requires_four_projects_with_five_roots_each(self):
        temporary, manifest = self.make_manifest()
        self.addCleanup(temporary.cleanup)

        groups = load_manifest(manifest)

        self.assertEqual(len(groups), 4)
        self.assertTrue(all(len(group.roots) == 5 for group in groups))

        too_few_temporary, too_few_projects = self.make_manifest(group_count=3)
        self.addCleanup(too_few_temporary.cleanup)
        with self.assertRaisesRegex(ValueError, "exactly four"):
            load_manifest(too_few_projects)

    def test_rejects_a_group_with_fewer_than_five_roots(self):
        temporary, manifest = self.make_manifest(root_count=4)
        self.addCleanup(temporary.cleanup)

        with self.assertRaisesRegex(ValueError, "exactly five"):
            load_manifest(manifest)


class SseTests(unittest.TestCase):
    def test_decodes_last_json_data_event(self):
        payload = {"jsonrpc": "2.0", "id": 7, "result": {"ok": True}}
        body = "data: stale\n\ndata: " + json.dumps(payload) + "\n\n"

        self.assertEqual(decode_sse(body), payload)


if __name__ == "__main__":
    unittest.main()
