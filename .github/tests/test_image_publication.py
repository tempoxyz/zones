"""Exercise the image verification trust boundary (requires PyYAML and jq)."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml

JOBS = yaml.safe_load((Path(__file__).parents[1] / 'workflows/docker.yml').read_text())['jobs']

def shell(job, name):
    return next(s['run'] for s in JOBS[job]['steps'] if s.get('name') == name)

class ImagePublicationTests(unittest.TestCase):
    def test_candidate_has_no_production_credentials(self):
        job = JOBS['build-and-push']
        self.assertEqual(job['permissions']['packages'], 'read')
        text = json.dumps(job)
        for forbidden in ('DOCKER_HUB_TOKEN', 'EVENTS_KEY', 'push: true', 'docker push'):
            self.assertNotIn(forbidden, text)
        self.assertEqual(job['outputs']['artifact_id'], '${{ steps.images.outputs.artifact-id }}')

    def test_only_matching_candidate_is_staged(self):
        code = shell('production-reproducible-verify', 'Compare candidate runtime inputs and stage verified image')
        config = {'Entrypoint': ['/usr/local/bin/tempo-zone'], 'Cmd': None, 'Env': [],
                  'WorkingDir': '/data', 'User': '', 'ExposedPorts': None, 'Volumes': None,
                  'Healthcheck': None, 'StopSignal': None}
        config_hash = hashlib.sha256(json.dumps(config, sort_keys=True, separators=(',', ':')).encode()).hexdigest()
        binary = b'canonical binary'; ca = b'canonical CA bundle'
        for mismatch in ('none', 'binary', 'config', 'ca'):
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as tmp:
                p = Path(tmp); (p/'bin').mkdir()
                (p/'binary').write_bytes(binary); (p/'ca').write_bytes(ca)
                docker = p/'bin/docker'
                docker.write_text('''#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
args = sys.argv[1:]; root = pathlib.Path(os.environ['RUNNER_TEMP'])
with (root/'calls').open('a') as f: f.write(json.dumps(args)+'\\n')
if args[0] == 'create': print('fixture-container')
elif args[0] == 'cp': shutil.copyfile(root/('ca' if 'ca-certificates' in args[1] else 'binary'), args[2])
elif args[:2] == ['image','inspect']: print(os.environ['CONFIG'])
elif args[:3] == ['buildx','imagetools','inspect']: print('sha256:'+'2'*64)
'''); docker.chmod(0o755)
                env = {**os.environ, 'PATH': str(p/'bin')+':'+os.environ['PATH'],
                       'RUNNER_TEMP':tmp, 'GITHUB_OUTPUT':str(p/'outputs'), 'CONFIG':json.dumps(config),
                       'COMMIT_SHA':'1'*40, 'SOURCE_DATE_EPOCH':'1700000000', 'VERSION':'dev',
                       'IMAGE_REPOSITORY':'ghcr.io/tempoxyz/tempo-zone-staging', 'IMAGE_TAG':'fixture',
                       'CLEAN_SHA256':hashlib.sha256(binary).hexdigest(),
                       'CLEAN_RUNTIME_CONFIG_SHA256':config_hash,
                       'CLEAN_CA_BUNDLE_SHA256':hashlib.sha256(ca).hexdigest(),
                       'BINARY_PATH':'/usr/local/bin/tempo-zone', 'CONTRACT_NAME':'fixture',
                       'CONTRACT_VERSION':'1', 'MANIFEST':str(p/'manifest.json')}
                key = {'binary':'CLEAN_SHA256', 'config':'CLEAN_RUNTIME_CONFIG_SHA256', 'ca':'CLEAN_CA_BUNDLE_SHA256'}.get(mismatch)
                if key: env[key] = '0'*64
                result = subprocess.run(['bash','-c',code], env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode == 0, mismatch == 'none', result.stderr)
                calls = [json.loads(line) for line in (p/'calls').read_text().splitlines()]
                pushes = [c for c in calls if c[0]=='push']
                self.assertEqual(pushes, [['push','ghcr.io/tempoxyz/tempo-zone-staging:fixture']] if mismatch=='none' else [])
                manifest = json.loads((p/'manifest.json').read_text())
                self.assertEqual(manifest['binary_comparison_result'], 'success' if mismatch=='none' else 'failed')

    def test_companion_publisher_rejects_node_tags(self):
        code = shell('publish-companion-images', 'Publish fixed companion image names')
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp); (p/'bin').mkdir()
            docker = p/'bin/docker'; docker.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$RUNNER_TEMP/calls"\n'); docker.chmod(0o755)
            env = {**os.environ, 'PATH':str(p/'bin')+':'+os.environ['PATH'], 'RUNNER_TEMP':tmp,
                   'REGISTRY':'ghcr.io/tempoxyz', 'XTASK_TAGS':'ghcr.io/tempoxyz/tempo-zone:latest',
                   'PROVER_TAGS':'', 'UTILS_TAGS':'', 'BUILDER_REF':''}
            result = subprocess.run(['bash','-c',code], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('push ', (p/'calls').read_text())

if __name__ == '__main__':
    unittest.main()
