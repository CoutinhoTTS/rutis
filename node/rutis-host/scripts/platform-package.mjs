// Make the npm package of one platform's rutis-host binary:
//   node scripts/platform-package.mjs <platform> <arch> <binary> <out dir>
// e.g. linux x64 target/x86_64-unknown-linux-gnu/release/rutis-host dist/linux-x64
import { chmodSync, copyFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'

const [platform, arch, binary, out] = process.argv.slice(2)
if (!out) throw new Error('usage: platform-package.mjs <platform> <arch> <binary> <out dir>')
const host = JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8'))
mkdirSync(join(out, 'bin'), { recursive: true })
copyFileSync(binary, join(out, 'bin', 'rutis-host'))
chmodSync(join(out, 'bin', 'rutis-host'), 0o755)
writeFileSync(join(out, 'package.json'), JSON.stringify({
  name: `@arcships/rutis-host-${platform}-${arch}`,
  version: host.version,
  description: `The rutis-host binary for ${platform} ${arch}; install @arcships/rutis-host instead`,
  license: host.license,
  repository: host.repository,
  os: [platform],
  cpu: [arch],
  files: ['bin'],
  publishConfig: { access: 'public' },
}, null, 2) + '\n')
console.log(`${out}: @arcships/rutis-host-${platform}-${arch}@${host.version}`)
