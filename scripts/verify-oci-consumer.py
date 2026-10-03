#!/usr/bin/env python3
"""Verify native-tested variants through an independent, local OCI consumer."""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys


layout, report_path = map(Path, sys.argv[1:])
report = json.loads(report_path.read_text())
selected = []
for variant in report['variants']:
    arch = variant['platform']['architecture']
    if arch not in ('amd64', 'arm64'):
        raise SystemExit('unexpected candidate architecture')

    def blob(descriptor):
        digest = descriptor['digest']
        if not re.fullmatch(r'sha256:[0-9a-f]{64}', digest):
            raise SystemExit('invalid candidate digest')
        data = (layout / 'blobs' / 'sha256' / digest[7:]).read_bytes()
        if hashlib.sha256(data).hexdigest() != digest[7:]:
            raise SystemExit('candidate blob digest mismatch')
        return json.loads(data)

    manifest = blob(variant)
    expected = blob(manifest['config'])
    raw = subprocess.check_output(['skopeo', '--override-os', 'linux', '--override-arch', arch,
                                   'inspect', '--config', '--raw', 'oci:' + str(layout) + ':candidate'])
    actual = json.loads(raw)
    if actual != expected or actual['architecture'] != arch or actual['os'] != 'linux':
        raise SystemExit('OCI consumer selected an unexpected native-tested variant')
    selected.append({'architecture': arch, 'config_digest': manifest['config']['digest'], 'matched': True})
report['consumer_verification'] = {
    'tool': subprocess.check_output(['skopeo', '--version'], text=True).strip(),
    'package_version': subprocess.check_output(['dpkg-query', '-W', '-f=${Version}', 'skopeo'], text=True).strip(),
    'layout_reference': 'candidate', 'selected': selected, 'registry_access': False,
    'execution_scope': 'independent OCI selection; native binary/image execution is recorded by the platform jobs',
}
report_path.write_text(json.dumps(report, sort_keys=True, indent=2) + '\n')
