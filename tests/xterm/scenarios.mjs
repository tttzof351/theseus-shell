const bashRows = Array.from({ length: 180 }, (_, i) => `BASH_ROW_${String(i).padStart(3, '0')}`);
const tableRows = Array.from({ length: 36 }, (_, i) => `TABLE_ROW_${String(i).padStart(3, '0')}`);
const tableOutput = [...bashRows, 'TABLE_REASONING', 'TABLE_INTRO', ...tableRows, 'TABLE_SUMMARY'];
const tablePreview = i => ({
  screenContains: [`TABLE_ROW_${String(i).padStart(3, '0')}`, 'KEPT_DRAFT'],
  normalOnce: bashRows,
  historyExcludes: ['TABLE_ROW_', 'KEPT_DRAFT'],
  cursorAfter: 'tester theseus-shell> KEPT_DRAFT',
  spinner: true,
});

export const scenarios = {
  'long-bash-table': {
    // The tail can be visible before the UI consumes BlockFinished. Earlier
    // rows are still a mutable preview then; check their publication below.
    'tool-tail': { screenContains: ['BASH_ROW_179'], normalOnce: ['BASH_ROW_179'], spinner: true },
    'table-preview-18': tablePreview(18),
    'table-preview-35': tablePreview(35),
    'table-complete': {
      screenContains: ['TABLE_SUMMARY', 'KEPT_DRAFT'], normalOnce: tableOutput,
      historyExcludes: ['KEPT_DRAFT'], cursorAfter: 'tester theseus-shell> KEPT_DRAFT', spinner: false,
    },
    final: {
      normalOnce: [...tableOutput, 'AFTER_TABLE'], normalExcludes: ['KEPT_DRAFT'],
      cursorAfter: 'tester theseus-shell> ', spinner: false,
    },
  },
  'markdown-resize': {
    'unresolved-reference': { screenContains: ['[doc]'], spinner: true },
    'resolved-reference': {
      screenContains: ['LINK_TITLE (https://example.test/guide)', 'TABLE_界'],
      screenExcludes: ['[doc]'], spinner: true,
    },
    'narrow-preview': {
      size: [60, 18], screenContains: ['UNICODE_界🙂', 'RESIZE_DRAFT'],
      historyExcludes: ['RESIZE_DRAFT', 'UNICODE_'], bold: ['UNICODE_界🙂'],
      cursorAfter: 'tester theseus-shell> RESIZE_DRAFT', spinner: true,
    },
    final: {
      size: [90, 18], normalOnce: [
        'LINK_TITLE', 'https://example.test/guide', 'TABLE_界', 'TABLE_🙂',
        'CODE_MARKER', 'UNICODE_界🙂', 'AFTER_MARKDOWN',
      ], normalExcludes: ['[doc]'], cursorAfter: 'tester theseus-shell> ', spinner: false,
    },
  },
  'vim-handoff': {
    'vim-edit': { alternate: true, size: [60, 18], screenContains: ['VIM_SSE_TEXT'], spinner: false },
    'after-vim': {
      size: [90, 18], normalExcludes: ['VIM_SSE_TEXT'], cursorAfter: 'tester theseus-shell> ', spinner: false,
    },
    final: {
      normalOnce: ['SSE_BEFORE_LEASES', 'INPUT_IMMEDIATE', 'WRAPPED_START', 'WRAPPED_END', 'AFTER_VIM', 'SSE_AFTER_LEASES'],
      normalExcludes: ['VIM_SSE_TEXT'], bold: ['SSE_AFTER_LEASES'],
      cursorAfter: 'tester theseus-shell> ', spinner: false,
    },
  },
  ...Object.fromEntries(['cancel', 'provider', 'eof'].map(outcome => [`interruption-${outcome}`, {
    busy: {
      screenContains: ['PRESERVED_PREFIX', 'KEPT_DRAFT'], bold: ['PRESERVED_PREFIX'],
      historyExcludes: ['PRESERVED_PREFIX', 'KEPT_DRAFT'],
      cursorAfter: 'tester theseus-shell> KEPT_DRAFT', spinner: true,
    },
    interrupted: {
      screenContains: ['PRESERVED_PREFIX', 'KEPT_DRAFT', outcome === 'cancel' ? '[interrupted]' : '[failed:'],
      normalOnce: ['PRESERVED_PREFIX'], normalExcludes: ['LATE_REJECTED'],
      cursorAfter: 'tester theseus-shell> KEPT_DRAFT', spinner: false,
    },
    final: {
      normalOnce: ['PRESERVED_PREFIX', 'NEXT_RESPONSE'], normalExcludes: ['LATE_REJECTED'],
      bold: ['NEXT_RESPONSE'], cursorAfter: 'tester theseus-shell> ', spinner: false,
    },
  }])),
};
