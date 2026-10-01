// Node/xtask images depend on source, while the prover package depends on genesis.
const fs = require('node:fs');
const path = require('node:path');
const {execFileSync} = require('node:child_process');

const packages = ['tempo-zone', 'tempo-zone-xtask', 'tempo-zone-prover-utils'];
const shaPattern = /^[0-9a-f]{40}$/;
const digestPattern = /^sha256:[0-9a-f]{64}$/;

function command(file, args) {
  return execFileSync(file, args, {encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe']}).trim();
}

function digest(ref, run) {
  let result;
  try {
    result = run('docker', ['buildx', 'imagetools', 'inspect', ref, '--format', '{{.Manifest.Digest}}']);
  } catch (error) {
    // A missing image is a cache miss. Authentication/network failures are not.
    if (/\bnot found\b|manifest unknown|MANIFEST_UNKNOWN/.test(String(error.stderr))) return null;
    throw error;
  }
  if (!digestPattern.test(result)) throw new Error(`Invalid digest for ${ref}`);
  return result;
}

function prepare(env, run = command) {
  const source = env.SOURCE_SHA || env.GITHUB_SHA;
  if (!shaPattern.test(source) || !shaPattern.test(env.GITHUB_SHA)) throw new Error('Expected full commit SHAs');
  if (env.SOURCE_SHA) {
    if (env.GITHUB_REPOSITORY !== 'tempoxyz/zones') throw new Error('Devnet reuse requires tempoxyz/zones');
    // E2E creates an empty child commit solely to give each genesis a unique tag.
    // Never allow an arbitrary image revision to stand in for changed source.
    if (run('git', ['rev-parse', 'HEAD']) !== env.GITHUB_SHA ||
        run('git', ['rev-parse', 'HEAD^']) !== source ||
        run('git', ['rev-parse', 'HEAD^{tree}']) !== run('git', ['rev-parse', `${source}^{tree}`])) {
      throw new Error('Devnet commit must have the source SHA as its parent and the same tree');
    }
  }
  const images = packages.map(name => {
    const ref = `${env.REGISTRY}/${name}:source-${source}`;
    return {name, ref, digest: env.SOURCE_SHA ? digest(ref, run) : null};
  });
  const missing = images.filter(image => !image.digest).map(image => image.name);
  const target = {};
  for (const image of images) {
    // PR builds publish only full-SHA candidate tags, never moving release tags.
    // Devnet builds publish the original source identity, then alias by run SHA.
    target[image.name] = env.SOURCE_SHA || env.GITHUB_EVENT_NAME === 'pull_request'
      ? {tags: [image.ref], labels: {'org.opencontainers.image.revision': source}}
      : {};
  }
  return {
    plan: {source, run: env.GITHUB_SHA, images},
    bake: {group: {'source-images': {targets: missing}}, target},
    outputs: {source_sha: source, source_short_sha: source.slice(0, 7), build_required: String(missing.length > 0)},
  };
}

function alias(plan, env, run = command) {
  if (plan.run !== env.GITHUB_SHA) throw new Error('Source image plan belongs to a different run');
  for (const image of plan.images) {
    // Preserve digests resolved before the build even if another run retags source.
    const resolved = image.digest || digest(image.ref, run);
    if (!resolved) throw new Error(`Source image was not published: ${image.ref}`);
    const pinned = `${env.REGISTRY}/${image.name}@${resolved}`;
    const tag = `${env.REGISTRY}/${image.name}:sha-${env.GITHUB_SHA.slice(0, 7)}`;
    run('docker', ['buildx', 'imagetools', 'create', '--prefer-index=false', '--tag', tag, pinned]);
    if (digest(tag, run) !== resolved) throw new Error(`Devnet alias changed the source digest: ${tag}`);
    console.log(`Reused ${pinned} as ${tag}`);
  }
}

if (require.main === module) {
  const planPath = path.join(process.env.RUNNER_TEMP, 'source-images.json');
  if (process.argv[2] === 'prepare') {
    const result = prepare(process.env);
    fs.writeFileSync(planPath, JSON.stringify(result.plan));
    fs.writeFileSync(path.join(process.env.RUNNER_TEMP, 'source-images-bake.json'), JSON.stringify(result.bake));
    fs.appendFileSync(process.env.GITHUB_OUTPUT, Object.entries(result.outputs).map(([k, v]) => `${k}=${v}\n`).join(''));
  } else if (process.argv[2] === 'alias') {
    alias(JSON.parse(fs.readFileSync(planPath, 'utf8')), process.env);
  } else {
    throw new Error('Expected prepare or alias');
  }
}

module.exports = {prepare, alias};
