import { test } from 'node:test'
import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, readFileSync, writeFileSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

test('a platform package carries the binary, its os and cpu, and the host version', () => {
  const dir = mkdtempSync(join(tmpdir(), 'rutis-host-'))
  const binary = join(dir, 'rutis-host')
  writeFileSync(binary, '#!/bin/sh\n')
  const out = join(dir, 'pkg')
  execFileSync(process.execPath, [new URL('../scripts/platform-package.mjs', import.meta.url).pathname, 'linux', 'x64', binary, out])
  const manifest = JSON.parse(readFileSync(join(out, 'package.json'), 'utf8'))
  const host = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'))
  assert.equal(manifest.name, '@arcships/rutis-host-linux-x64')
  assert.equal(manifest.version, host.version)
  assert.deepEqual([manifest.os, manifest.cpu], [['linux'], ['x64']])
  assert.ok(statSync(join(out, 'bin', 'rutis-host')).mode & 0o111)
  assert.equal(host.optionalDependencies[manifest.name], host.version, 'the host depends on it at its version')
})
