import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { scenarios } from './scenarios.mjs';
import { replay } from './replay.mjs';

const root = fileURLToPath(new URL('../../', import.meta.url));
const args = process.argv.slice(2);
let directory;
try {
  assert.ok(args.length === 0 || (args.length === 2 && args[0] === '--replay'), 'usage: npm test [-- --replay <trace-directory>]');
  if (args.length) {
    directory = resolve(args[1]);
  } else {
    const parent = join(root, 'target/xterm');
    mkdirSync(parent, { recursive: true });
    directory = mkdtempSync(join(parent, 'run-'));
    console.log(`Recording real PTY scenarios in ${directory}`);
    const result = spawnSync('cargo', ['test', '--locked', '--test', 'application_pty', 'streaming::'], {
      cwd: root, stdio: 'inherit', timeout: 180_000,
      env: { ...process.env, THESEUS_XTERM_TRACE_DIR: directory },
    });
    if (result.error) throw result.error;
    assert.equal(result.status, 0, `PTY scenarios failed (${result.signal ?? result.status})`);
  }
  let passed = 0;
  for (const [name, expected] of Object.entries(scenarios)) {
    // Missing exports fail; a skipped scenario must never masquerade as a pass.
    const trace = JSON.parse(readFileSync(join(directory, `${name}.json`), 'utf8'));
    const bytes = readFileSync(join(directory, `${name}.ansi`));
    for (const scrollOnEraseInDisplay of [false, true]) {
      for (const fragmented of [false, true]) {
        const label = `${name}-${scrollOnEraseInDisplay ? 'vscode' : 'default'}-${fragmented ? 'fragmented' : 'whole'}`;
        try {
          await replay(trace, bytes, expected, { scrollOnEraseInDisplay, fragmented });
          console.log(`ok ${label}`);
          passed++;
        } catch (error) {
          const state = error.terminalState;
          if (state) {
            writeFileSync(join(directory, `${label}.failure.json`), JSON.stringify(state, null, 2));
            writeFileSync(join(directory, `${label}.history.txt`), state.normal);
          }
          throw new Error(label, { cause: error });
        }
      }
    }
  }
  // Prove that these checks catch both original regressions, using the same
  // captured application output with each bad control sequence restored.
  const trace = JSON.parse(readFileSync(join(directory, 'long-bash-table.json'), 'utf8'));
  const bytes = readFileSync(join(directory, 'long-bash-table.ansi'));
  for (const mutation of ['ed2', 'su']) {
    await assert.rejects(
      replay(trace, bytes, scenarios['long-bash-table'], { scrollOnEraseInDisplay: true, fragmented: true, mutation }),
      error => error instanceof assert.AssertionError && error.terminalState?.mutations > 0,
      `${mutation}: the original rendering regression was not detected`,
    );
    console.log(`ok detects original ${mutation} regression`);
  }
  console.log(`${passed} xterm.js replays passed; both regression detectors passed. Traces: ${directory}`);
} catch (error) {
  console.error(error);
  if (directory) console.error(`Traces and failure details: ${directory}`);
  process.exitCode = 1;
}
