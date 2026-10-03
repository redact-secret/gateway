#!/usr/bin/env python3
"""Validate OCI platform selection, exact blobs and refusal of mislabeled roots."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("oci_bundle", Path(__file__).with_name("oci-bundle.py"))
bundle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bundle)


class BundleTests(unittest.TestCase):
    def archive(self, directory, architecture, bad_hash=False):
        layer = b"SYNTHETIC-LAYER-NOT-AN-IMAGE"
        digest = "sha256:" + hashlib.sha256(layer).hexdigest()
        config = json.dumps({"os": "linux", "architecture": architecture,
                             "rootfs": {"diff_ids": ["sha256:wrong" if bad_hash else digest]}}).encode()
        archive = directory / f"{architecture}-{bad_hash}.tar"
        with tarfile.open(archive, "w") as target:
            for name, data in {
                "manifest.json": json.dumps([{"Config": "config.json", "Layers": ["layer.tar"]}]).encode(),
                "config.json": config, "layer.tar": layer,
            }.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                target.addfile(member, io.BytesIO(data))
        return archive

    def test_index_selects_platform_and_preserves_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            archives = [self.archive(root, arch) for arch in ("amd64", "arm64")]
            report = bundle.assemble(root / "layout", archives)
            self.assertFalse(report["published"])
            self.assertEqual([v["platform"]["architecture"] for v in report["variants"]], ["amd64", "arm64"])
            for blob in (root / "layout" / "blobs" / "sha256").iterdir():
                self.assertEqual(hashlib.sha256(blob.read_bytes()).hexdigest(), blob.name)
            layout = json.loads((root / "layout" / "index.json").read_bytes())
            self.assertEqual(len(layout["manifests"]), 1)
            named = layout["manifests"][0]
            self.assertEqual(named["annotations"]["org.opencontainers.image.ref.name"], "candidate")
            self.assertEqual(named["digest"], report["index"]["digest"])
            multi = json.loads((root / "layout" / "blobs" / "sha256" / named["digest"].split(":")[1]).read_bytes())
            self.assertEqual(multi["manifests"], report["variants"])

    def test_duplicate_platform_and_wrong_root_hash_are_refused(self):
        for duplicate in (True, False):
            with tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                first = self.archive(root, "amd64")
                second = first if duplicate else self.archive(root, "arm64", bad_hash=True)
                with self.assertRaises(ValueError):
                    bundle.assemble(root / "layout", [first, second])


if __name__ == "__main__":
    unittest.main()
