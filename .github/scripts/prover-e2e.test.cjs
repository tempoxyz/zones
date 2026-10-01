const assert = require('node:assert/strict');
const {test} = require('node:test');
const {readFileSync} = require('node:fs');
const {resolve} = require('node:path');
const source = readFileSync(resolve(__dirname, '../workflows/prover-e2e.yml'), 'utf8');
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
function script(step) {
  const section = source.split(`      - name: ${step}\n`)[1];
  assert.ok(section, `Missing step ${step}`);
  const block = section.split('          script: |\n')[1].split(/\n(?=      - name:)/)[0];
  return new AsyncFunction('context', 'github', 'core', 'process', block.replace(/^            /gm, ''));
}
const candidate = script('Resolve candidate and seed owned pending status');
const report = script('Report irrelevant changes or dispatch failure');
const sha = 'a'.repeat(40), base = 'b'.repeat(40);
function fixture(event, overrides = {}) {
  const outputs = {}, posts = [], failures = [];
  const context = {eventName: event, sha, ref: 'refs/heads/main', runId: 123,
    repo: {owner: 'tempoxyz', repo: 'zones'}, serverUrl: 'https://github.com',
    payload: {merge_group: {head_sha: sha, base_sha: base}}, ...overrides};
  const env = {GITHUB_RUN_ATTEMPT: '2', INPUT_SHA: sha, SHA: sha, INPUT_SUITE: 'fast',
    STATUS_CONTEXT: 'Tempo Zone Prover E2E', PATHS_RESULT: 'success', CHANGES: '[]'};
  const statuses = [{context: env.STATUS_CONTEXT, description: '123-2: pending'}];
  const core = {setOutput: (key, value) => outputs[key] = value, setFailed: value => failures.push(value)};
  const github = {rest: {repos: {
    getCommit: async ({ref}) => ({data: {sha: ref}}),
    createCommitStatus: async body => posts.push(body), listCommitStatusesForRef: 'statuses',
  }}, paginate: {async *iterator() { yield {data: statuses}; }}};
  return {outputs, posts, failures, statuses, env, call: fn => fn(context, github, core, {env})};
}

test('merge queue resolves exact base/head and selects the fast context', async () => {
  const f = fixture('merge_group'); await f.call(candidate);
  assert.equal(f.outputs.sha, sha); assert.equal(f.outputs.base, base);
  assert.equal(f.outputs.suite, 'fast'); assert.equal(f.posts[0].context, 'Tempo Zone Prover E2E');
});
test('main push selects full coverage for the pushed SHA', async () => {
  const f = fixture('push'); await f.call(candidate);
  assert.equal(f.outputs.sha, sha); assert.equal(f.outputs.suite, 'full');
  assert.equal(f.posts[0].context, 'Tempo Zone Prover E2E (full)');
});
test('manual dispatch supports both suites', async () => {
  for (const suite of ['fast', 'full']) {
    const f = fixture('workflow_dispatch'); f.env.INPUT_SUITE = suite; await f.call(candidate);
    assert.equal(f.outputs.suite, suite); assert.equal(f.outputs.sha, sha);
  }
});
test('cron and non-main pushes are rejected', async () => {
  for (const f of [fixture('schedule'), fixture('push', {ref: 'refs/heads/feature'})]) {
    await assert.rejects(f.call(candidate)); assert.equal(f.posts.length, 0);
  }
});
test('unrelated merge-queue changes report success', async () => {
  const f = fixture('merge_group'); await f.call(report);
  assert.equal(f.posts[0].state, 'success');
  assert.equal(f.posts[0].description, '123-2: No relevant changes');
  assert.equal(f.failures.length, 0);
});
test('detection, dispatch and main-push failures cannot become skipped success', async () => {
  const detection = fixture('merge_group'); detection.env.PATHS_RESULT = 'failure';
  const dispatch = fixture('merge_group'); dispatch.env.CHANGES = '["prover"]';
  for (const f of [detection, dispatch, fixture('push'), fixture('workflow_dispatch')]) {
    await f.call(report); assert.equal(f.posts[0].state, 'failure'); assert.equal(f.failures.length, 1);
  }
});
test('an old publisher cannot overwrite a newer attempt', async () => {
  const f = fixture('merge_group'); f.statuses[0].description = '124-1: pending';
  await f.call(report); assert.equal(f.posts.length, 0);
});
test('full and fast dispatch failures update separate contexts', async () => {
  const f = fixture('push'); f.env.STATUS_CONTEXT = 'Tempo Zone Prover E2E (full)';
  f.statuses.push({context: f.env.STATUS_CONTEXT, description: '123-2: pending'});
  await f.call(report); assert.equal(f.posts[0].context, 'Tempo Zone Prover E2E (full)');
});
