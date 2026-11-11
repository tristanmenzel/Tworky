// Builds the `tw` CLI that the app bundles and puts on each session's PATH. On Windows it also ships
// a `wa.exe` copy for workers made before the rename (see CLAUDE.md).
//
// The bundle's resources always name target/release/tw, so a universal macOS app build
// (TAURI_ENV_TARGET_TRIPLE, set by `tauri build --target universal-apple-darwin`) gets a universal
// `tw` written there too.
import { execFileSync } from 'node:child_process'
import { copyFileSync, mkdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const release = `${root}target/release/`

if (process.env.TAURI_ENV_TARGET_TRIPLE === 'universal-apple-darwin') {
  const targets = ['aarch64-apple-darwin', 'x86_64-apple-darwin']
  for (const target of targets) {
    execFileSync('cargo', ['build', '--release', '-p', 'tw', '--target', target], { cwd: root, stdio: 'inherit' })
  }
  mkdirSync(release, { recursive: true })
  execFileSync(
    'lipo',
    ['-create', '-output', `${release}tw`, ...targets.map((t) => `${root}target/${t}/release/tw`)],
    { stdio: 'inherit' },
  )
  console.log(`wrote a universal ${release}tw`)
} else {
  execFileSync('cargo', ['build', '--release', '-p', 'tw'], { cwd: root, stdio: 'inherit' })
}

if (process.platform === 'win32') {
  copyFileSync(`${release}tw.exe`, `${release}wa.exe`)
  console.log(`copied ${release}tw.exe to wa.exe`)
}
