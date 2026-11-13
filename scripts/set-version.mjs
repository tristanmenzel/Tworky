// Stamps a release version into the workspace Cargo.toml, tauri.conf.json and package.json, so the
// app, its installer and `tw --version` all carry it.
//
//   node scripts/set-version.mjs <major.minor.patch>
//
// With no argument it prints `<major>.<minor>` from Cargo.toml, the part release.yml keeps and
// completes with the workflow's run number.
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const cargoToml = `${root}Cargo.toml`
const workspaceVersion = /(\[workspace\.package\][^[]*?\nversion\s*=\s*")([^"]+)(")/

const cargo = readFileSync(cargoToml, 'utf8')
const current = cargo.match(workspaceVersion)?.[2]
if (!current) throw new Error('Cargo.toml has no [workspace.package] version')

const version = process.argv[2]
if (!version) {
  console.log(current.split('.').slice(0, 2).join('.'))
  process.exit(0)
}
if (!/^\d+\.\d+\.\d+$/.test(version)) throw new Error(`not a major.minor.patch version: ${version}`)

writeFileSync(cargoToml, cargo.replace(workspaceVersion, `$1${version}$3`))
for (const file of ['src-tauri/tauri.conf.json', 'package.json']) {
  const path = `${root}${file}`
  const text = readFileSync(path, 'utf8')
  const stamped = text.replace(/^(\s*"version":\s*")[^"]+(")/m, `$1${version}$2`)
  if (stamped === text && !text.includes(`"version": "${version}"`)) throw new Error(`${file} has no version`)
  writeFileSync(path, stamped)
}
