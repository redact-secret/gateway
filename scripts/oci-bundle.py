#!/usr/bin/env python3
"""Assemble tested docker-save variants into an unpublished OCI image layout.

No registry or compiler. Reuses exact config and uncompressed layer bytes; records
OCI manifest digests and Docker config identities separately. Never extracts tar
member paths. Input archives are trusted candidate build outputs.
"""
import hashlib
import json
from pathlib import Path
import sys
import tarfile


def encoded(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def assemble(output, archives):
    output.mkdir(parents=True, exist_ok=False)
    blobs = output / "blobs" / "sha256"
    blobs.mkdir(parents=True)

    def put(data, media):
        digest = hashlib.sha256(data).hexdigest()
        (blobs / digest).write_bytes(data)
        return {"mediaType": media, "digest": f"sha256:{digest}", "size": len(data)}

    manifests = []
    seen = set()
    for archive in archives:
        with tarfile.open(archive) as source:
            def read(name):
                member = source.getmember(name)
                if not member.isfile():
                    raise ValueError("candidate member must be a regular file")
                with source.extractfile(member) as stream:
                    return stream.read()

            entries = json.loads(read("manifest.json"))
            if len(entries) != 1:
                raise ValueError("one tested variant per candidate archive required")
            entry = entries[0]
            config_bytes = read(entry["Config"])
            config = json.loads(config_bytes)
            arch = config["architecture"]
            if config["os"] != "linux" or arch not in ("amd64", "arm64") or arch in seen:
                raise ValueError("unexpected or duplicate platform")
            seen.add(arch)
            layers = []
            for name, diff_id in zip(entry["Layers"], config["rootfs"]["diff_ids"], strict=True):
                descriptor = put(read(name), "application/vnd.oci.image.layer.v1.tar")
                if descriptor["digest"] != diff_id:
                    raise ValueError("layer bytes do not match tested rootfs diff_id")
                layers.append(descriptor)
            manifest = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "config": put(config_bytes, "application/vnd.oci.image.config.v1+json"), "layers": layers}
            descriptor = put(encoded(manifest), manifest["mediaType"])
            descriptor["platform"] = {"os": "linux", "architecture": arch}
            manifests.append(descriptor)
    if seen != {"amd64", "arm64"}:
        raise ValueError("both tested Linux architectures required")
    index = {"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json",
             "manifests": sorted(manifests, key=lambda item: item["platform"]["architecture"])}
    index_bytes = encoded(index)
    index_descriptor = put(index_bytes, index["mediaType"])
    (output / "index.json").write_bytes(index_bytes)
    (output / "oci-layout").write_bytes(encoded({"imageLayoutVersion": "1.0.0"}))
    return {"index": index_descriptor, "variants": index["manifests"], "published": False}


if __name__ == "__main__":
    if len(sys.argv) != 4:
        raise SystemExit("usage: oci-bundle.py <new-layout-dir> <amd64-docker-save> <arm64-docker-save>")
    print(json.dumps(assemble(Path(sys.argv[1]), sys.argv[2:]), sort_keys=True, indent=2))
