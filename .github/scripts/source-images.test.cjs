const assert = require('node:assert/strict');
const {test} = require('node:test');
const {prepare, alias} = require('./source-images.cjs');

const source = 'a'.repeat(40);
const child = 'b'.repeat(40);
const hash = `sha256:${'c'.repeat(64)}`;
const env = {SOURCE_SHA: source, GITHUB_SHA: child, GITHUB_REPOSITORY: 'tempoxyz/zones',
  GITHUB_EVENT_NAME: 'workflow_dispatch', REGISTRY: 'ghcr.io/tempoxyz'};

function commands({head = child, parent = source, changed = false, missing = [], failure} = {}) {
  return (file, args) => {
    if (file === 'git') {
      return {HEAD: head, 'HEAD^': parent, 'HEAD^{tree}': changed ? 'different' : 'tree',
        [`${source}^{tree}`]: 'tree'}[args[1]];
    }
    assert.equal(file, 'docker');
    assert.deepEqual(args.slice(0, 3), ['buildx', 'imagetools', 'inspect']);
    if (failure) throw Object.assign(new Error(failure), {stderr: failure});
    if (missing.some(name => args[3].includes(`/${name}:`))) {
      throw Object.assign(new Error('missing'), {stderr: 'manifest unknown'});
    }
    return hash;
  };
}

test('identical source across devnets skips compilation and pins the same images', () => {
  const first = prepare(env, commands());
  const second = prepare({...env, GITHUB_SHA: 'd'.repeat(40)}, commands({head: 'd'.repeat(40)}));
  assert.equal(first.outputs.build_required, 'false');
  assert.deepEqual(first.bake.group['source-images'].targets, []);
  assert.deepEqual(first.plan.images, second.plan.images);
  assert.equal(first.outputs.source_sha, source);
});

test('a partial cache miss builds only the missing source image', () => {
  const result = prepare(env, commands({missing: ['tempo-zone-xtask']}));
  assert.equal(result.outputs.build_required, 'true');
  assert.deepEqual(result.bake.group['source-images'].targets, ['tempo-zone-xtask']);
  assert.deepEqual(result.bake.target['tempo-zone-xtask'].tags, [`ghcr.io/tempoxyz/tempo-zone-xtask:source-${source}`]);
  assert.equal(result.bake.target['tempo-zone-xtask'].labels['org.opencontainers.image.revision'], source);
});

test('reuse rejects changed trees, unrelated parents, and wrong checkouts', () => {
  for (const options of [{changed: true}, {parent: child}, {head: source}]) {
    assert.throws(() => prepare(env, commands(options)), /same tree/);
  }
  assert.throws(() => prepare({...env, SOURCE_SHA: 'main'}, commands()), /full commit SHAs/);
  assert.throws(() => prepare({...env, GITHUB_REPOSITORY: 'fork/zones'}, commands()), /requires tempoxyz/);
});

test('authentication and network failures do not silently become cache misses', () => {
  for (const failure of ['unauthorized', 'connection timed out']) {
    assert.throws(() => prepare(env, commands({failure})), new RegExp(failure));
  }
  assert.throws(() => prepare(env, (file, args) => file === 'git' ? commands()(file, args) : 'invalid'), /Invalid digest/);
});

test('ordinary CI still builds, and PR metadata publishes only candidate tags', () => {
  for (const event of ['pull_request', 'merge_group', 'push', 'schedule', 'workflow_dispatch']) {
    const result = prepare({...env, SOURCE_SHA: '', GITHUB_EVENT_NAME: event}, () => { throw new Error('unexpected lookup'); });
    assert.equal(result.outputs.source_sha, child);
    assert.equal(result.bake.group['source-images'].targets.length, 3);
    for (const image of result.plan.images) {
      assert.deepEqual(result.bake.target[image.name], event === 'pull_request'
        ? {tags: [image.ref], labels: {'org.opencontainers.image.revision': child}} : {});
    }
  }
});

test('aliases use captured digests, resolve misses after building, and verify identity', () => {
  const {plan} = prepare(env, commands({missing: ['tempo-zone-xtask']}));
  const published = [];
  alias(plan, env, (file, args) => {
    if (args[2] === 'create') {
      assert.equal(args[3], '--prefer-index=false');
      assert.match(args[5], /:sha-bbbbbbb$/);
      assert.ok(args[6].endsWith(`@${hash}`));
      published.push(args[5]);
      return '';
    }
    // Existing source tags must not be resolved again after compilation.
    if (args[3].includes(':source-')) assert.ok(args[3].includes('/tempo-zone-xtask:'));
    return hash;
  });
  assert.equal(published.length, 3);
  assert.throws(() => alias(plan, {...env, GITHUB_SHA: source}), /different run/);
  assert.throws(() => alias(plan, env, () => `sha256:${'e'.repeat(64)}`), /changed the source digest/);
});
