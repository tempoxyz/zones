import assert from 'node:assert/strict';
import { test } from 'node:test';
import { replaceRuntimes } from './prepare-tempo-runtimes.mjs';

const artifacts = {
  ZonePortal: { deployedBytecode: { object: '0x6001' } },
  Verifier: { deployedBytecode: { object: '6002' } },
  ZoneMessenger: { deployedBytecode: { object: '0x60AB' } },
};
const runtimes = prefix => ['PORTAL', 'VERIFIER', 'MESSENGER'].map(kind =>
  `pub const ${prefix}ZONE_${kind}_RUNTIME: Bytes = bytes!(\n    "0x6000"\n    "00"\n);`,
).join('\n');

test('aligns initial, T12, and future fork runtimes without changing other source', () => {
  const before = '// preserved\nuse alloy_primitives::{Bytes, bytes};\n';
  const after = '\npub const UNRELATED: u64 = 12;\n';
  const source = before + runtimes('') + runtimes('T12_') + runtimes('T13_') + after;
  const result = replaceRuntimes(source, artifacts);
  assert.equal(result.constants.length, 9);
  assert.ok(result.source.startsWith(before));
  assert.ok(result.source.endsWith(after));
  for (const prefix of ['', 'T12_', 'T13_']) {
    for (const [kind, bytecode] of [['PORTAL', '6001'], ['VERIFIER', '6002'], ['MESSENGER', '60ab']]) {
      assert.ok(result.source.includes(`pub const ${prefix}ZONE_${kind}_RUNTIME: Bytes = bytes!(\n    "0x${bytecode}"\n);`));
    }
  }
  assert.deepEqual(replaceRuntimes(result.source, artifacts), result);
});

test('supports Tempo revisions with only the initial runtime set', () => {
  assert.equal(replaceRuntimes(runtimes(''), artifacts).constants.length, 3);
});

test('supports a future fork that upgrades only one contract', () => {
  const source = runtimes('') + runtimes('T12_') +
    '\npub const T13_ZONE_PORTAL_RUNTIME: Bytes = bytes!("0x6000");';
  const result = replaceRuntimes(source, artifacts);
  assert.equal(result.constants.length, 7);
  assert.ok(result.source.includes('pub const T13_ZONE_PORTAL_RUNTIME: Bytes = bytes!(\n    "0x6001"\n);'));
});

test('rejects empty, odd-length, non-hex and unlinked bytecode', () => {
  for (const object of ['', '0x', '0x123', '0xgg', '__$library$__']) {
    assert.throws(() => replaceRuntimes(runtimes(''), {
      ...artifacts, ZonePortal: { deployedBytecode: { object } },
    }), /Invalid or missing ZonePortal/);
  }
});

test('fails closed when upstream changes the runtime representation', () => {
  assert.throws(() => replaceRuntimes('', artifacts), /No Tempo Zone runtime/);
  assert.throws(() => replaceRuntimes(runtimes('').replace('Bytes = bytes!', 'Bytes = include_bytes!'), artifacts), /Unsupported runtime definition/);
  assert.throws(() => replaceRuntimes(runtimes('').replace('ZONE_VERIFIER_RUNTIME', 'UNRELATED_RUNTIME'), artifacts), /Missing runtime constants for Verifier/);
  assert.throws(() => replaceRuntimes(runtimes('') + runtimes(''), artifacts), /Duplicate runtime constant/);
});
