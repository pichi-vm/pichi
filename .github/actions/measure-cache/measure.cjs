// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0
const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const workspace = process.env.GITHUB_WORKSPACE;
const temporary = fs.mkdtempSync(path.join(process.env.RUNNER_TEMP, 'cache-measure-'));
const upstream = path.join(workspace, '.measure-rust-cache/dist');
const nightly = `+${process.env.NIGHTLY}`;
process.env.CARGO_INCREMENTAL = '0';
const inputs = {
  'prefix-key': `measure-157-private-${process.env.GITHUB_RUN_ID}`,
  'add-job-id-key': 'true',
  'add-rust-environment-hash-key': 'true',
  workspaces: '. -> target\nappliance/dillo -> target',
  'cache-targets': 'false',
  'cache-directories': `${workspace}/target\n${workspace}/appliance/dillo/target`,
  'cache-bin': 'true',
  'cache-provider': 'github',
  'save-if': 'true',
  'cmd-format': '{0}',
};
for (const [key, value] of Object.entries(inputs)) {
  process.env[`INPUT_${key.toUpperCase()}`] = value;
}

function command(executable, args, extra = {}) {
  const result = spawnSync(executable, args, { stdio: 'inherit', cwd: workspace, ...extra });
  if (result.error || result.status !== 0) throw result.error || new Error(`${executable}: ${result.status}`);
}

function timed(label, operation) {
  console.log(`::group::${label}`);
  const start = performance.now();
  const result = operation();
  console.log('::endgroup::');
  const seconds = (performance.now() - start) / 1000;
  console.log(`CACHE_MEASUREMENT ${JSON.stringify({ label, seconds })}`);
  return result;
}

function fields(filename) {
  const lines = fs.readFileSync(filename, 'utf8').split(/\r?\n/);
  const values = {};
  for (let i = 0; i < lines.length; i++) {
    const delimiter = lines[i].indexOf('<<');
    if (delimiter >= 0) {
      const name = lines[i].slice(0, delimiter);
      const end = lines[i].slice(delimiter + 2);
      const chunks = [];
      while (++i < lines.length && lines[i] !== end) chunks.push(lines[i]);
      values[name] = chunks.join('\n');
    } else if (lines[i].includes('=')) {
      const index = lines[i].indexOf('=');
      values[lines[i].slice(0, index)] = lines[i].slice(index + 1);
    }
  }
  return values;
}

let invocation = 0;
function action(script, state = {}) {
  const stateFile = path.join(temporary, `state-${invocation}`);
  const outputFile = path.join(temporary, `output-${invocation++}`);
  fs.writeFileSync(stateFile, '');
  fs.writeFileSync(outputFile, '');
  const env = { ...process.env, GITHUB_STATE: stateFile, GITHUB_OUTPUT: outputFile };
  for (const [key, value] of Object.entries(state)) env[`STATE_${key}`] = value;
  command(process.execPath, [path.join(upstream, script)], { env });
  return { state: fields(stateFile), output: fields(outputFile) };
}

function build() {
  command('cargo', [nightly, 'build', '--locked', '--release', '--bin', 'pichi']);
  command('cargo', [nightly, '--config', 'packaging/cargo-config.toml', 'build', '--locked', '--release', '--bin', 'dillo'], {
    cwd: path.join(workspace, 'appliance/dillo'),
  });
}

const initial = timed('cold cache lookup', () => action('restore.js'));
if (initial.output['cache-hit'] !== 'false') throw new Error('Expected a new, empty benchmark cache');
timed('cold build', build);
timed('cache cleanup and save', () => action('save.js', initial.state));

for (const sample of [1, 2]) {
  // Only this isolated job's build outputs and dedicated Cargo home are removed.
  for (const directory of [
    path.join(workspace, 'target'),
    path.join(workspace, 'appliance/dillo/target'),
    path.join(process.env.CARGO_HOME, 'registry'),
    path.join(process.env.CARGO_HOME, 'git'),
  ]) fs.rmSync(directory, { recursive: true, force: true });
  const restored = timed(`warm ${sample} restore`, () => action('restore.js'));
  if (restored.output['cache-hit'] !== 'true') throw new Error('Saved dependency cache did not restore exactly');
  timed(`warm ${sample} build`, build);
}
