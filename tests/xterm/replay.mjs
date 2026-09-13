import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { Terminal } = require('@xterm/headless');
const { Unicode11Addon } = require('@xterm/addon-unicode11');

const spinner = /[\u2800-\u28ff]/u;
const homeErase = '\x1b[1;1H\x1b[J';
const publishScroll = '\x1b[65535;1H\n';

function lines(buffer, first, end) {
  return Array.from({ length: end - first }, (_, i) => buffer.getLine(first + i).translateToString(true)).join('\n');
}

export function snapshot(terminal) {
  const active = terminal.buffer.active;
  const normal = terminal.buffer.normal;
  return {
    size: [terminal.cols, terminal.rows],
    alternate: active.type === 'alternate',
    cursor: [active.cursorX, active.cursorY],
    screen: lines(active, active.baseY, active.baseY + terminal.rows),
    normal: lines(normal, 0, normal.length),
    history: lines(normal, 0, normal.baseY),
  };
}

function check(terminal, expected, name) {
  const state = snapshot(terminal);
  assert.equal(state.alternate, expected.alternate ?? false, `${name}: active buffer`);
  if (expected.size) assert.deepEqual(state.size, expected.size, `${name}: terminal size`);
  assert.ok(!spinner.test(state.history), `${name}: spinner leaked into native history`);
  for (const text of expected.screenContains ?? []) {
    assert.ok(state.screen.includes(text), `${name}: screen missing ${text}`);
  }
  for (const [key, haystack] of [['screenExcludes', state.screen], ['normalExcludes', state.normal], ['historyExcludes', state.history]]) {
    for (const text of expected[key] ?? []) assert.ok(!haystack.includes(text), `${name}: ${key} contains ${text}`);
  }
  let last = -1;
  for (const text of expected.normalOnce ?? []) {
    assert.equal(state.normal.split(text).length - 1, 1, `${name}: expected ${text} exactly once`);
    const position = state.normal.indexOf(text);
    assert.ok(position > last, `${name}: output order at ${text}`);
    last = position;
  }
  if (expected.spinner !== undefined) assert.equal(spinner.test(state.screen), expected.spinner, `${name}: spinner`);
  const active = terminal.buffer.active;
  if (expected.cursorAfter !== undefined) {
    const before = active.getLine(active.baseY + active.cursorY).translateToString(false, 0, active.cursorX);
    assert.equal(before, expected.cursorAfter, `${name}: cursor position within editor`);
  }
  for (const marker of expected.bold ?? []) {
    let found = false;
    for (let y = active.baseY; y < active.baseY + terminal.rows; y++) {
      const line = active.getLine(y);
      for (let x = 0; x < line.length; x++) {
        if (!line.translateToString(true, x).startsWith(marker)) continue;
        let text = '';
        for (let column = x; column < line.length && text.length < marker.length; column++) {
          const cell = line.getCell(column);
          if (cell.getWidth() === 0) continue;
          assert.ok(cell.isBold(), `${name}: ${marker} lost bold at column ${column}`);
          text += cell.getChars();
        }
        assert.equal(text, marker);
        found = true;
        break;
      }
    }
    assert.ok(found, `${name}: bold marker ${marker} is not visible`);
  }
}

async function write(terminal, bytes, fragmented) {
  if (!bytes.length) return;
  const sizes = fragmented ? [1, 2, 3, 7, 31, 1024] : [bytes.length];
  await new Promise(resolve => {
    let index = 0;
    for (let offset = 0; offset < bytes.length;) {
      const end = Math.min(bytes.length, offset + sizes[index++ % sizes.length]);
      terminal.write(bytes.subarray(offset, end), end === bytes.length ? resolve : undefined);
      offset = end;
    }
  });
}

export async function replay(trace, bytes, expected, options) {
  assert.equal(trace.version, 1, 'unsupported trace format');
  const terminal = new Terminal({
    rows: trace.rows, cols: trace.cols, scrollback: 20_000, allowProposedApi: true,
    scrollOnEraseInDisplay: options.scrollOnEraseInDisplay,
  });
  terminal.loadAddon(new Unicode11Addon());
  terminal.unicode.activeVersion = '11'; // VS Code's default, including emoji width.
  let checkpoint = 'initial';
  let mutations = 0;
  const process = async data => {
    if (options.mutation) {
      const source = options.mutation === 'ed2' ? homeErase : publishScroll;
      const replacement = options.mutation === 'ed2' ? '\x1b[2J\x1b[1;1H' : '\x1b[1S';
      // Latin-1 round-trips arbitrary PTY bytes; do not decode partial UTF-8 here.
      const raw = data.toString('latin1');
      mutations += raw.split(source).length - 1;
      data = Buffer.from(raw.replaceAll(source, replacement), 'latin1');
    }
    await write(terminal, data, options.fragmented);
  };
  try {
    let offset = 0;
    const seen = [];
    for (const event of trace.events) {
      assert.ok(Number.isInteger(event.offset) && event.offset >= offset && event.offset <= bytes.length, 'invalid trace offset');
      await process(bytes.subarray(offset, event.offset));
      offset = event.offset;
      if (event.type === 'resize') {
        terminal.resize(event.cols, event.rows);
      } else {
        assert.equal(event.type, 'checkpoint', 'unknown trace event');
        checkpoint = event.name;
        assert.ok(Object.hasOwn(expected, checkpoint), `unknown checkpoint ${checkpoint}`);
        seen.push(checkpoint);
        check(terminal, expected[checkpoint], checkpoint);
      }
    }
    assert.deepEqual(seen, Object.keys(expected), 'missing or reordered checkpoints');
    await process(bytes.subarray(offset));
    check(terminal, expected[checkpoint], checkpoint);
    if (options.mutation) assert.ok(mutations > 0, 'regression mutation did not match the trace');
    return snapshot(terminal);
  } catch (error) {
    error.terminalState = { checkpoint, mutations, ...snapshot(terminal) };
    throw error;
  } finally {
    terminal.dispose();
  }
}
