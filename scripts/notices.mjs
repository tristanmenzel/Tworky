// Writes THIRD-PARTY-NOTICES.txt from cargo-about (Rust crates) and license-checker-rseidelsohn
// (npm packages). CI regenerates it on Linux and fails on any diff, so the output must not depend
// on the OS: LF line endings, no paths, and a fixed order.
import { execFileSync } from 'node:child_process'
import { readdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { init } from 'license-checker-rseidelsohn'

const root = fileURLToPath(new URL('..', import.meta.url))

// devDependencies whose code Vite bundles into dist. `--production` misses them.
const bundledDevDependencies = ['svelte', 'clsx', 'vite']

// webview2-com-sys statically links WebView2LoaderStatic.lib from this NuGet package on Windows.
// scripts/webview2-sdk-license.txt is that package's LICENSE.txt.
const webview2 = { sys: '0.39.1', sdk: '1.0.3800.47' }

const rule = '-'.repeat(80)
const licenseFileName = /^(licen[cs]e|copying|notice)/i

const byCodePoint = (a, b) => (a < b ? -1 : a > b ? 1 : 0)

const checkLicenses = (opts) =>
  new Promise((resolve, reject) =>
    init({ start: root, unknown: true, ...opts }, (err, packages) => (err ? reject(err) : resolve(packages))),
  )

const rust = execFileSync('cargo', ['about', 'generate', '--workspace', '--locked', '--fail', 'about.hbs'], {
  cwd: root,
  encoding: 'utf8',
  maxBuffer: 256 * 1024 * 1024,
  stdio: ['ignore', 'pipe', 'inherit'],
})

const lockedWebview2Sys = readFileSync(join(root, 'Cargo.lock'), 'utf8').match(
  /name = "webview2-com-sys"\r?\nversion = "([^"]+)"/,
)?.[1]
if (lockedWebview2Sys !== webview2.sys)
  throw new Error(
    `Cargo.lock has webview2-com-sys ${lockedWebview2Sys}, not ${webview2.sys}: update the WebView2 SDK version and licence in scripts/notices.mjs`,
  )

const webview2Sdk = [
  rule,
  `Microsoft WebView2 SDK ${webview2.sdk} (WebView2LoaderStatic.lib, linked by webview2-com-sys ${webview2.sys} on Windows)`,
  `https://www.nuget.org/packages/Microsoft.Web.WebView2/${webview2.sdk}`,
  '',
  readFileSync(join(root, 'scripts', 'webview2-sdk-license.txt'), 'utf8'),
].join('\n')

const npmPackages = {
  ...(await checkLicenses({ production: true, excludePrivatePackages: true })),
  ...(await checkLicenses({ development: true, includePackages: bundledDevDependencies.join(';') })),
}

const missing = bundledDevDependencies.filter((name) => !Object.keys(npmPackages).some((id) => id.startsWith(`${name}@`)))
if (missing.length) throw new Error(`bundled devDependencies not found in node_modules: ${missing.join(', ')}`)

const unknown = Object.entries(npmPackages).filter(([, info]) => /unknown|\*/i.test(String(info.licenses)))
if (unknown.length) throw new Error(`npm packages with an unknown licence: ${unknown.map(([id]) => id).join(', ')}`)

const npm = Object.keys(npmPackages)
  .sort(byCodePoint)
  .map((id) => {
    const { licenses, repository, path } = npmPackages[id]
    const files = readdirSync(path)
      .filter((file) => licenseFileName.test(file))
      .sort(byCodePoint)
    if (!files.length) throw new Error(`${id} has no licence file`)
    const at = id.lastIndexOf('@')
    const header = [`${id.slice(0, at)} ${id.slice(at + 1)} (${[].concat(licenses).join(', ')})`]
    if (repository) header.push(repository)
    return [rule, ...header, '', ...files.map((file) => readFileSync(join(path, file), 'utf8'))].join('\n')
  })
  .join('\n\n')

const notices = `Third-party notices for Tworky
==============================

Tworky includes the following third-party software, under the licences given here.

Rust crates
===========

${rust}

Microsoft WebView2 SDK
======================

${webview2Sdk}

npm packages
============

${npm}
`

const normalised = `${notices
  .replace(/^﻿/gm, '')
  .replace(/\r\n?/g, '\n')
  .replace(/[ \t]+$/gm, '')
  .replace(/\n{3,}/g, '\n\n')
  .trim()}\n`

const placeholder = normalised.match(/<year>|<copyright holders>|<owner>/)
if (placeholder)
  throw new Error(`a licence text still has the template placeholder ${placeholder[0]}; clarify its crate in about.toml`)

writeFileSync(join(root, 'THIRD-PARTY-NOTICES.txt'), normalised)
console.log('wrote THIRD-PARTY-NOTICES.txt')
