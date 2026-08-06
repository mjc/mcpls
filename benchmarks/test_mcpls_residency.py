#!/usr/bin/env python3
"""Tests for the MCPLS resident-budget benchmark."""

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

from mcpls_residency import decode_sse, load_manifest


class ManifestTests(unittest.TestCase):
    def make_manifest(self, root_count=5, group_count=4):
        temporary = tempfile.TemporaryDirectory()
        roots = []
        for group in range(group_count):
            repository = Path(temporary.name) / f"repo-{group}"
            repository.mkdir()
            subprocess.run(
                ["git", "init", "--quiet", "--initial-branch=main", str(repository)],
                check=True,
            )
            (repository / "Cargo.toml").write_text(
                "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n"
            )
            subprocess.run(["git", "-C", str(repository), "add", "Cargo.toml"], check=True)
            subprocess.run(
                [
                    "git",
                    "-C",
                    str(repository),
                    "-c",
                    "user.name=benchmark",
                    "-c",
                    "user.email=benchmark@example.invalid",
                    "commit",
                    "--quiet",
                    "-m",
                    "initial",
                ],
                check=True,
            )
            group_roots = [repository]
            for root in range(1, root_count):
                path = Path(temporary.name) / f"repo-{group}-worktree-{root}"
                subprocess.run(
                    [
                        "git",
                        "-C",
                        str(repository),
                        "worktree",
                        "add",
                        "--quiet",
                        "--detach",
                        str(path),
                        "HEAD",
                    ],
                    check=True,
                )
                group_roots.append(path)
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

    def test_rejects_roots_that_are_not_git_worktrees(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        projects = []
        for group in range(4):
            roots = []
            for root in range(5):
                path = Path(temporary.name) / f"plain-{group}-{root}"
                path.mkdir()
                (path / "Cargo.toml").write_text(
                    "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\n"
                )
                roots.append(str(path))
            projects.append({"project_id": f"plain-{group}", "roots": roots})

        with self.assertRaisesRegex(ValueError, "Git worktree"):
            load_manifest({"projects": projects})


class SseTests(unittest.TestCase):
    def test_decodes_last_json_data_event(self):
        payload = {"jsonrpc": "2.0", "id": 7, "result": {"ok": True}}
        body = "data: stale\n\ndata: " + json.dumps(payload) + "\n\n"

        self.assertEqual(decode_sse(body), payload)


if __name__ == "__main__":
    unittest.main()
