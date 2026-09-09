#!/usr/bin/env node

// Benchmark the selected Zones contracts under Tempo's selected execution fork.
// Keep genesis and every fork's runtime installation consistent with those contracts.
import { readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const contracts = {
  PORTAL: 'ZonePortal',
  VERIFIER: 'Verifier',
  MESSENGER: 'ZoneMessenger',
};

export function replaceRuntimes(source, artifacts) {
  const bytecodes = {};
  for (const [kind, contract] of Object.entries(contracts)) {
    const object = artifacts[contract]?.deployedBytecode?.object;
    if (typeof object !== 'string' || !/^(?:0x)?(?:[a-fA-F0-9]{2})+$/.test(object)) {
      throw new Error(`Invalid or missing ${contract} deployed bytecode`);
    }
    bytecodes[kind] = object.replace(/^0x/, '').toLowerCase();
  }

  const declarations = [...source.matchAll(/pub\s+const\s+((?:[A-Z0-9]+_)*ZONE_(PORTAL|VERIFIER|MESSENGER)_RUNTIME)\s*:/g)];
  if (declarations.length === 0) throw new Error('No Tempo Zone runtime constants found');
  const replaced = new Set();
  const groups = new Map();
  const patched = source.replace(
    /pub\s+const\s+((?:[A-Z0-9]+_)*ZONE_(PORTAL|VERIFIER|MESSENGER)_RUNTIME)\s*:\s*Bytes\s*=\s*bytes!\(\s*((?:"(?:0x)?[a-fA-F0-9]*"\s*)+)\);/g,
    (_, name, kind) => {
      if (replaced.has(name)) throw new Error(`Duplicate runtime constant: ${name}`);
      replaced.add(name);
      const prefix = name.slice(0, -`ZONE_${kind}_RUNTIME`.length);
      if (!groups.has(prefix)) groups.set(prefix, new Set());
      groups.get(prefix).add(kind);
      const chunks = bytecodes[kind].match(/.{1,112}/g);
      const body = chunks.map((chunk, i) => `    "${i === 0 ? '0x' : ''}${chunk}"`).join('\n');
      return `pub const ${name}: Bytes = bytes!(\n${body}\n);`;
    },
  );
  for (const [, name] of declarations) {
    if (!replaced.has(name)) throw new Error(`Unsupported runtime definition: ${name}`);
  }
  for (const [prefix, kinds] of groups) {
    if (kinds.size !== Object.keys(contracts).length) {
      throw new Error(`Incomplete runtime set: ${prefix || 'initial'}`);
    }
  }
  return { source: patched, constants: [...replaced] };
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [tempoRoot, artifactsRoot, ...extra] = process.argv.slice(2);
  if (!tempoRoot || !artifactsRoot || extra.length) {
    throw new Error('Usage: prepare-tempo-runtimes.mjs TEMPO_ROOT FOUNDRY_OUT');
  }
  const artifacts = Object.fromEntries(Object.values(contracts).map(contract => [
    contract,
    JSON.parse(readFileSync(resolve(artifactsRoot, `${contract}.sol`, `${contract}.json`), 'utf8')),
  ]));
  const path = resolve(tempoRoot, 'crates/contracts/src/zones.rs');
  const result = replaceRuntimes(readFileSync(path, 'utf8'), artifacts);
  writeFileSync(path, result.source);
  console.log(`Embedded benchmark contracts in Tempo: ${result.constants.join(', ')}`);
}
