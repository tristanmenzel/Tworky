import { spawn } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const env = process.env
const workerId = env.TW_WORKER_ID || env.WA_WORKER_ID
const vitePort = env.TW_PORT_VITE || env.WA_PORT_VITE

const config = {}
if (workerId) {
  config.identifier = `com.tristanmenzel.tworky.dev.${workerId.replace(/[^A-Za-z0-9.-]/g, '-')}`
}
if (workerId || vitePort) {
  config.build = { devUrl: `http://localhost:${vitePort || 1420}` }
}

if (config.build) env.TWORKY_VITE_PORT = String(vitePort || 1420)

const args = ['tauri', 'dev']
if (Object.keys(config).length) {
  const file = join(mkdtempSync(join(tmpdir(), 'tworky-dev-')), 'tauri.dev.json')
  writeFileSync(file, JSON.stringify(config))
  args.push('--config', `"${file}"`)
}
args.push(...process.argv.slice(2))

const child = spawn('npx', args, { stdio: 'inherit', shell: true })
child.on('exit', (code, signal) => process.exit(code ?? (signal ? 1 : 0)))
