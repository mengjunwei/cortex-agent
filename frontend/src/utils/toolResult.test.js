import test from 'node:test'
import assert from 'node:assert/strict'
import {
  buildRunCommandConfirmationPrompt,
  getToolResultConfirmation,
  getRunCommandDiagnostics,
  getGrepSummary,
  getCollapsedDirectoryEntries,
} from './toolResult.js'

test('detects run_command confirmation result', () => {
  const result = {
    ok: false,
    require_confirmation: true,
    confirm_token: 'abc123',
    command: 'rm -rf /tmp/demo',
    error: '需要确认',
  }

  assert.deepEqual(getToolResultConfirmation('run_command', result), {
    toolName: 'run_command',
    command: 'rm -rf /tmp/demo',
    confirmToken: 'abc123',
    error: '需要确认',
  })
})

test('builds follow-up prompt with confirm_token for approved dangerous command', () => {
  const prompt = buildRunCommandConfirmationPrompt({
    command: 'rm -rf /tmp/demo',
    confirmToken: 'abc123',
  })

  assert.match(prompt, /用户已确认执行危险命令/)
  assert.match(prompt, /rm -rf \/tmp\/demo/)
  assert.match(prompt, /confirm_token/)
  assert.match(prompt, /abc123/)
})

test('extracts diagnostics from run_command result', () => {
  const result = {
    diagnostics: [
      { severity: 'error', file: 'src/main.rs', line: 2, column: 5, message: 'mismatched types' },
    ],
  }

  assert.deepEqual(getRunCommandDiagnostics(result), result.diagnostics)
})

test('extracts grep summary when summary_enabled is true', () => {
  const result = {
    summary: { summary_enabled: true, total_files: 2, note: '结果已摘要' },
    matches: [{ file: 'a.rs', count: 10 }],
    total_matches: 60,
  }

  assert.deepEqual(getGrepSummary(result), {
    summary: result.summary,
    files: result.matches,
    totalMatches: 60,
  })
})

test('extracts collapsed directory entries', () => {
  const result = {
    entries: [
      { name: 'src', kind: 'dir' },
      { name: 'deep', kind: 'collapsed', collapsed_count: 12 },
    ],
  }

  assert.deepEqual(getCollapsedDirectoryEntries(result), [
    { name: 'deep', kind: 'collapsed', collapsed_count: 12 },
  ])
})
