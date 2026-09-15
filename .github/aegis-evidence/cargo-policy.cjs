const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const assert = require('node:assert/strict');

const logPath = process.platform === 'darwin'
  ? '/Library/Application Support/Aegis/service.jsonl'
  : '/var/log/aegis/service.jsonl';
const readEvents = () => execFileSync('sudo', ['cat', logPath], { encoding: 'utf8' })
  .split(/\r?\n/).filter(line => line.startsWith('{')).map(line => JSON.parse(line));
const baseline = path.join(process.env.RUNNER_TEMP, 'aegis-evidence-log-start.json');
const mode = process.argv[2];
if (mode === 'prepare') {
  const source = process.env.CARGO_HOME || path.join(os.homedir(), '.cargo');
  const target = fs.mkdtempSync(path.join(process.env.RUNNER_TEMP, 'aegis-evidence-cargo-'));
  for (const name of ['config', 'config.toml']) {
    const config = path.join(source, name);
    if (fs.existsSync(config)) fs.copyFileSync(config, path.join(target, name));
  }
  fs.writeFileSync(baseline, JSON.stringify(readEvents().length));
  fs.appendFileSync(process.env.GITHUB_ENV, `CARGO_HOME=${target}\n`);
  console.log('Empty Cargo cache prepared; installed Cargo trust configuration retained.');
} else {
  const purl = mode === 'allow' ? 'pkg:cargo/itoa@1.0.15' : 'pkg:cargo/iddqd@0.5.0';
  const action = mode === 'allow' ? 'allow' : 'block';
  const events = readEvents().slice(fs.existsSync(baseline)
    ? JSON.parse(fs.readFileSync(baseline, 'utf8')) : 0);
  const matches = events.filter(event => event.purl === purl && event.action === action);
  // Only policy decision fields are printed, never configuration, tokens, or keys.
  for (const event of matches) {
    const safe = Object.fromEntries(['time', 'timestamp', 'request_id', 'purl', 'action', 'reason', 'status']
      .filter(key => key in event).map(key => [key, event[key]]));
    console.log(JSON.stringify(safe));
  }
  assert.ok(matches.length > 0, `No fresh ${action} decision for ${purl}`);
  if (mode !== 'allow') {
    assert.equal(process.env.BLOCK_OUTCOME, 'failure', 'The package command must really fail');
    assert.ok(matches.some(event => /recently.?published|cooldown/i.test(JSON.stringify(event))),
      'A missing verdict, auth failure, or network error is not policy-block evidence');
  }
}
