#!/usr/bin/env node
/**
 * Build the DeepSeek Harness web "node carrier" for the Tauri shell:
 *
 *   - stages the @deepseek-ai/dsh dependency closure into a flat, symlink-free
 *     node_modules tree (the same deploy route the harness SDK runtime uses),
 *   - downloads the official Node binary (no system Node required),
 *   - packages the closure as a single tarball shipped inside the .app
 *     (bundle.resources) and extracted on first launch — fully offline, and
 *   - syncs the desktop app version (package.json / tauri.conf.json / Cargo.toml)
 *     to the harness @deepseek-ai/dsh version so the bundle never drifts.
 *
 * The Tauri shell spawns `node <data>/runtime/node-app/lib/bin.js --profile web`,
 * which is exactly how `dsh web` runs — the profile system (writable
 * $DSH_HOME, healProfilesModuleFallback symlinks, runtime ctx.loader.create)
 * needs a real filesystem, which a pkg --sea VFS cannot provide.
 */
import { spawn } from 'node:child_process'
import { createHash } from 'node:crypto'
import { createReadStream, existsSync, globSync } from 'node:fs'
import {
  chmod, cp, lstat, mkdir, readFile, readdir, realpath, rename, rm, stat, writeFile,
} from 'node:fs/promises'
import { basename, dirname, join, resolve, sep } from 'node:path'

const ROOT = resolve(import.meta.dirname, '..')
const HARNESS_ROOT = process.env.HARNESS || join(ROOT, 'harness')
const OUT_DIR = join(ROOT, 'src-tauri', 'binaries')
const STAGING = join(OUT_DIR, '.staging')
const NODE_APP_DIR = join(ROOT, 'src-tauri', 'resources', 'node-app')

const DEPLOY_ROOT_PACKAGE = '@deepseek-ai/dsh'
const NODE_VERSION = 'v24.19.0'

/** pkg platform-arch -> Rust target triple (Tauri externalBin naming). */
const RUST_TRIPLES = {
  'macos-arm64': 'aarch64-apple-darwin',
  'macos-x64': 'x86_64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'win-x64': 'x86_64-pc-windows-msvc',
}

/** pkg platform-arch -> nodejs.org dist platform-arch. */
const NODE_PLATFORMS = {
  'macos-arm64': 'darwin-arm64',
  'macos-x64': 'darwin-x64',
  'linux-x64': 'linux-x64',
  'linux-arm64': 'linux-arm64',
  'win-x64': 'win-x64',
}

/** node-pty ships prebuilds for every OS in one package; per target we keep
 * only the matching dir (the win32-* dirs are ~58 MB of DLLs + .pdb symbols). */
const NODE_PTY_PREBUILDS = {
  'macos-arm64': 'darwin-arm64',
  'macos-x64': 'darwin-x64',
  'linux-x64': 'linux-x64',
  'linux-arm64': 'linux-arm64',
  'win-x64': 'win32-x64',
}

/** Workspace globs from the harness pnpm-workspace.yaml. */
const WORKSPACE_GLOBS = [
  'vendor/*',
  'packages/*/*',
  'apps/*',
  'website',
  'native/landlock-run',
  'native/landlock-run/packages/*',
]

function hostTarget() {
  const platform = process.platform === 'darwin' ? 'macos'
    : process.platform === 'linux' ? 'linux'
      : process.platform === 'win32' ? 'win' : undefined
  const arch = process.arch === 'arm64' ? 'arm64' : process.arch === 'x64' ? 'x64' : undefined
  if (platform === undefined || arch === undefined) return undefined
  return `${platform}-${arch}`
}

function parseArgs(argv) {
  let skipBuild = false
  let dryRun = false
  let noStrip = false
  const targets = []
  for (const arg of argv) {
    if (arg === '--skip-build') skipBuild = true
    else if (arg === '--no-strip') noStrip = true
    else if (arg === '--dry-run') dryRun = true
    else if (arg === '--help' || arg === '-h') {
      console.log(usage())
      process.exit(0)
    } else if (arg.startsWith('--targets=')) {
      targets.push(...arg.slice('--targets='.length).split(',').map(s => s.trim()).filter(Boolean))
    } else if (!arg.startsWith('--')) {
      targets.push(arg)
    }
  }
  if (targets.length === 0) {
    const host = hostTarget()
    if (host === undefined) {
      console.error('build-sidecar: cannot infer host target; pass --targets=... explicitly.')
      process.exit(1)
    }
    targets.push(host)
  }
  return { skipBuild, dryRun, noStrip, targets }
}

function usage() {
  return [
    'Usage: node scripts/build-sidecar.mjs [flags]',
    '',
    '  --targets=<t1,t2,...>  targets, e.g. macos-arm64,linux-x64 (default: host).',
    '  --skip-build           skip `pnpm install`/`pnpm run build` in the harness.',
    '  --no-strip             ship the node binary unstripped (default: strip it).',
    '  --dry-run              print commands without executing.',
    '  --help                 print this help.',
    '',
    `Harness checkout: ${HARNESS_ROOT} (override with HARNESS=<path>).`,
    `Output: ${OUT_DIR}/node-<rust-triple> (externalBin) + node-app-<triple>.tar.gz`,
    '        and src-tauri/runtime-manifest.json (first-launch download).',
    '  RUNTIME_BASE_URL=<url>  optional download URL recorded in the manifest (the',
    '                          shipped app bundles the closure, so this is unused).',
    '  RUNTIME_VERSION=<v>     runtime version in the manifest (default: harness version).',
    '',
    'The desktop app version is auto-synced from the harness @deepseek-ai/dsh version.',
  ].join('\n')
}

function pnpmBin() {
  return process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm'
}

function formatCommand(command, args) {
  return [command, ...args].map(p => (p.includes(' ') ? JSON.stringify(p) : p)).join(' ')
}

async function run(label, command, args, cwd = HARNESS_ROOT) {
  const printable = formatCommand(command, args)
  console.log(`build-sidecar: ${label}: ${printable}`)
  await new Promise((resolvePromise, reject) => {
    // Windows cannot execute .cmd shims directly through CreateProcess. Node
    // reports EINVAL for pnpm.cmd unless the command is routed through cmd.exe.
    const shell = process.platform === 'win32' && /\.(?:cmd|bat)$/i.test(command)
    const child = spawn(command, args, {
      cwd,
      stdio: 'inherit',
      env: { ...process.env, CI: 'true' },
      shell,
    })
    child.once('error', error => {
      reject(new Error(`build-sidecar: ${label} failed to spawn: ${error.message} (${printable})`))
    })
    child.once('exit', (code, signal) => {
      if (code === 0) resolvePromise()
      else {
        const cause = code === null ? `signal ${signal ?? 'unknown'}` : `exit code ${code}`
        reject(new Error(`build-sidecar: ${label} failed (${cause}): ${printable}`))
      }
    })
  })
}

async function findSymlink(directory) {
  let entries
  try {
    entries = await readdir(directory, { withFileTypes: true })
  } catch {
    return undefined
  }
  for (const entry of entries) {
    const path = join(directory, entry.name)
    let metadata
    try {
      metadata = await lstat(path)
    } catch {
      continue
    }
    if (metadata.isSymbolicLink()) return path
    if (metadata.isDirectory()) {
      const nested = await findSymlink(path)
      if (nested !== undefined) return nested
    }
  }
  return undefined
}

/** Replace staged package symlinks with their target bytes (a symlink-free tree). */
async function materializeStagedLinks() {
  const nodeModules = join(STAGING, 'node_modules')
  let remaining = await findSymlink(nodeModules)
  while (remaining !== undefined) {
    const segments = remaining.slice(nodeModules.length + 1).split(sep)
    const binIndex = segments.lastIndexOf('.bin')
    if (binIndex >= 0) {
      await rm(join(nodeModules, ...segments.slice(0, binIndex + 1)), { recursive: true, force: true })
      remaining = await findSymlink(nodeModules)
      continue
    }
    const destination = remaining
    const source = await realpath(destination)
    await rm(destination, { recursive: true, force: true })
    await cp(source, destination, { recursive: true, dereference: true })
    remaining = await findSymlink(nodeModules)
  }
}

/** Restore direct dependencies the legacy deploy hoisted beside the source. */
async function restoreLegacyHoists() {
  const manifestPath = join(STAGING, 'package.json')
  const manifest = JSON.parse(await readFile(manifestPath, 'utf8'))
  const sources = [
    join(HARNESS_ROOT, 'apps', 'cli', 'node_modules'),
    join(HARNESS_ROOT, 'node_modules'),
  ]
  const restored = []
  for (const dependency of Object.keys(manifest.dependencies ?? {}).sort()) {
    const destination = join(STAGING, 'node_modules', dependency)
    if (existsSync(destination)) continue
    const source = sources.map(s => join(s, dependency)).find(s => existsSync(s))
    if (source === undefined) {
      throw new Error(`build-sidecar: deployed dependency ${dependency} is missing from staging and harness node_modules.`)
    }
    await mkdir(dirname(destination), { recursive: true })
    const nestedNodeModules = join(source, 'node_modules')
    await cp(source, destination, {
      recursive: true,
      dereference: true,
      filter: path => path !== nestedNodeModules && !path.startsWith(nestedNodeModules + sep),
    })
    restored.push(dependency)
  }
  if (restored.length > 0) {
    console.log(`build-sidecar: restored legacy deploy hoists: ${restored.join(', ')}`)
  }
}

/**
 * `pnpm deploy --legacy` drops transitive workspace dependencies that are not
 * top-level dependencies of the deploy root (e.g. @deepseek-ai/cordis's
 * @deepseek-ai/cosmokit). Recover the full workspace closure and copy every
 * missing package flat into the staging node_modules.
 */
async function completeWorkspaceClosure() {
  const workspace = new Map()
  for (const glob of WORKSPACE_GLOBS) {
    for (const rel of globSync(`${glob}/package.json`, { cwd: HARNESS_ROOT })) {
      const dir = join(HARNESS_ROOT, dirname(rel))
      let manifest
      try {
        manifest = JSON.parse(await readFile(join(dir, 'package.json'), 'utf8'))
      } catch {
        continue
      }
      if (typeof manifest.name === 'string') workspace.set(manifest.name, dir)
    }
  }
  const rootManifest = JSON.parse(await readFile(join(STAGING, 'package.json'), 'utf8'))
  const queue = [
    ...Object.keys(rootManifest.dependencies ?? {}),
    ...Object.keys(rootManifest.peerDependencies ?? {}),
  ]
  const closure = new Set()
  while (queue.length > 0) {
    const name = queue.pop()
    if (!workspace.has(name) || closure.has(name)) continue
    closure.add(name)
    const dir = workspace.get(name)
    let manifest
    try {
      manifest = JSON.parse(await readFile(join(dir, 'package.json'), 'utf8'))
    } catch {
      continue
    }
    for (const dep of [
      ...Object.keys(manifest.dependencies ?? {}),
      ...Object.keys(manifest.peerDependencies ?? {}),
      ...Object.keys(manifest.optionalDependencies ?? {}),
    ]) {
      queue.push(dep)
    }
  }

  const targetRoot = join(STAGING, 'node_modules')
  const copied = []
  for (const name of closure) {
    const scopedTarget = join(targetRoot, ...name.split('/'))
    if (existsSync(join(scopedTarget, 'package.json'))) continue
    const src = workspace.get(name)
    const nestedNodeModules = join(src, 'node_modules')
    await mkdir(dirname(scopedTarget), { recursive: true })
    await cp(src, scopedTarget, {
      recursive: true,
      dereference: true,
      filter: path => path !== nestedNodeModules && !path.startsWith(nestedNodeModules + sep),
    })
    copied.push(name)
  }
  if (copied.length > 0) {
    console.log(`build-sidecar: completed workspace closure (${copied.length} copied): ${copied.join(', ')}`)
  }
}

async function deployStaging(skipBuild) {
  await rm(STAGING, { recursive: true, force: true })
  if (!skipBuild) {
    await run('install', pnpmBin(), ['install'])
    await run('build', pnpmBin(), ['run', 'build'])
  }
  await run('deploy', pnpmBin(), [
    '--filter', DEPLOY_ROOT_PACKAGE, 'deploy',
    '--legacy', '--prod',
    '--config.node-linker=hoisted',
    '--config.auto-install-peers=false',
    '--config.link-workspace-packages=true',
    STAGING,
  ])
  await restoreLegacyHoists()
  await materializeStagedLinks()
  await completeWorkspaceClosure()
}

/**
 * Read the packaged @deepseek-ai/dsh version from the deployed staging tree.
 * This is the single source of truth for the desktop app version, so the
 * bundle can never drift from the harness it wraps.
 */
async function detectHarnessVersion() {
  const manifestPath = join(STAGING, 'package.json')
  let manifest
  try {
    manifest = JSON.parse(await readFile(manifestPath, 'utf8'))
  } catch (err) {
    throw new Error(`build-sidecar: cannot read deployed manifest ${manifestPath}: ${err.message}`)
  }
  const version = manifest.version
  if (typeof version !== 'string' || version.length === 0) {
    throw new Error(`build-sidecar: deployed manifest ${manifestPath} has no version field.`)
  }
  return version
}

/**
 * Propagate the harness version into every place the desktop app records it
 * (package.json, tauri.conf.json, Cargo.toml). `tauri build` reads the bundle
 * version from tauri.conf.json, so this keeps the shipped .app / .dmg version
 * locked to the harness.
 */
async function syncDesktopVersion(version) {
  const pkgPath = join(ROOT, 'package.json')
  const pkg = JSON.parse(await readFile(pkgPath, 'utf8'))
  if (pkg.version !== version) {
    pkg.version = version
    await writeFile(pkgPath, `${JSON.stringify(pkg, null, 2)}\n`)
  }

  const confPath = join(ROOT, 'src-tauri', 'tauri.conf.json')
  const conf = JSON.parse(await readFile(confPath, 'utf8'))
  if (conf.version !== version) {
    conf.version = version
    await writeFile(confPath, `${JSON.stringify(conf, null, 2)}\n`)
  }

  const cargoPath = join(ROOT, 'src-tauri', 'Cargo.toml')
  const cargo = await readFile(cargoPath, 'utf8')
  const next = cargo.replace(/^version\s*=\s*"[^"]*"\s*$/m, `version = "${version}"`)
  if (next !== cargo) {
    await writeFile(cargoPath, next)
  }

  console.log(`build-sidecar: synced desktop version -> ${version} (harness ${DEPLOY_ROOT_PACKAGE}).`)
}

/** Download + extract the official Node binary for one target. */
async function fetchNodeBinary(target) {
  const nodePlatform = NODE_PLATFORMS[target]
  if (nodePlatform === undefined) {
    throw new Error(`build-sidecar: unsupported target ${JSON.stringify(target)} for the node carrier.`)
  }
  const triple = RUST_TRIPLES[target]
  const isWin = target.startsWith('win-')
  const ext = isWin ? '.zip' : '.tar.gz'
  const archive = join(OUT_DIR, `node-${NODE_VERSION}-${nodePlatform}${ext}`)
  const extractedDir = join(OUT_DIR, `node-${NODE_VERSION}-${nodePlatform}`)
  // Official Windows archives place node.exe at the archive root, while the
  // Unix archives use bin/node.
  const nodeBin = isWin
    ? join(extractedDir, 'node.exe')
    : join(extractedDir, 'bin', 'node')

  if (existsSync(nodeBin)) return nodeBin

  const url = `https://nodejs.org/dist/${NODE_VERSION}/node-${NODE_VERSION}-${nodePlatform}${ext}`
  await mkdir(OUT_DIR, { recursive: true })
  // Reuse the downloaded archive when present (assemble deletes the extracted
  // dir after use, so the archive is the cache that avoids re-downloading).
  if (!existsSync(archive)) {
    await run(`download node ${nodePlatform}`, 'curl', ['-fsSL', '--retry', '3', '-o', archive, url])
  }
  if (isWin) {
    // bsdtar ships with supported Windows installations and understands zip;
    // `unzip` is normally absent from Windows PATH.
    await run('extract node (zip)', 'tar', ['-xf', archive, '-C', OUT_DIR])
  } else {
    await run('extract node (tar)', 'tar', ['-xzf', archive, '-C', OUT_DIR])
  }
  if (!existsSync(nodeBin)) {
    throw new Error(`build-sidecar: node binary missing after extract: ${nodeBin}`)
  }
  return nodeBin
}

/** Recursive byte size of a file/dir (for prune + strip reporting). */
async function pathBytes(path) {
  let metadata
  try {
    metadata = await lstat(path)
  } catch {
    return 0
  }
  if (metadata.isDirectory()) {
    let total = 0
    for (const entry of await readdir(path, { withFileTypes: true })) {
      total += await pathBytes(join(path, entry.name))
    }
    return total
  }
  return metadata.size
}

/** Run a command, logging a warning instead of failing the build on error. */
async function tryRun(label, command, args) {
  return await new Promise(resolve => {
    const child = spawn(command, args, { stdio: 'inherit', env: process.env })
    child.once('error', error => {
      console.warn(`build-sidecar: ${label}: skipped: ${error.message}`)
      resolve(false)
    })
    child.once('exit', code => resolve(code === 0))
  })
}

/**
 * Drop per-platform native payloads that are dead weight for this target.
 * node-pty ships prebuilds for all OSes in one package; on macOS the win32-*
 * dirs (~58 MB of DLLs + .pdb debug symbols) are pure bloat. PDB files are
 * debug symbols and never needed in a shipped product.
 */
async function pruneTargetPrebuilds(root, target) {
  const keep = NODE_PTY_PREBUILDS[target]
  if (keep === undefined) return
  const prebuildsRoot = join(root, 'node_modules', 'node-pty', 'prebuilds')
  let entries
  try {
    entries = await readdir(prebuildsRoot, { withFileTypes: true })
  } catch {
    return // node-pty not in this closure; nothing to prune
  }
  let pruned = 0
  for (const entry of entries) {
    if (entry.name === keep) continue
    pruned += await pathBytes(join(prebuildsRoot, entry.name))
    await rm(join(prebuildsRoot, entry.name), { recursive: true, force: true })
  }
  for (const pdb of globSync('**/*.pdb', { cwd: prebuildsRoot, absolute: true })) {
    pruned += await pathBytes(pdb)
    await rm(pdb, { force: true })
  }
  if (pruned > 0) {
    console.log(`build-sidecar: pruned ${(pruned / 1048576).toFixed(1)} MiB of ${target} dead prebuilds (node-pty, kept ${keep})`)
  }
}

/**
 * Strip symbol tables from the sidecar node binary. Official node builds are
 * unstripped (116 MiB on darwin-arm64); `strip -x` gets ~93 MiB. Skipped on
 * Windows (PE is not safely stripped with binutils strip). Fails soft: an
 * unstripped binary still runs, so a missing `strip` only logs a warning.
 */
async function stripNodeBinary(dest, target) {
  if (target.startsWith('win-')) return
  const args = process.platform === 'darwin' ? ['-x', dest] : ['--strip-unneeded', dest]
  const ok = await tryRun(`strip node ${target}`, 'strip', args)
  if (!ok) {
    console.warn('build-sidecar: strip unavailable or failed; shipping the unstripped node binary.')
    return
  }
  // `strip` invalidates node's embedded code signature, and macOS (arm64)
  // refuses to execute an invalidly-signed binary (killed with SIGKILL). Tauri's
  // bundler does not re-sign externalBin sidecars, so re-sign ad-hoc here.
  if (process.platform === 'darwin') {
    const signed = await tryRun(`resign node ${target}`, 'codesign', ['--force', '-s', '-', dest])
    if (!signed) {
      console.warn('build-sidecar: codesign unavailable; the node sidecar may be killed at launch on Apple Silicon.')
    }
  }
  console.log(`build-sidecar: stripped node ${target}: ${((await stat(dest)).size / 1048576).toFixed(1)} MiB`)
}

/** Stream a file's SHA-256 hex digest (for the runtime manifest). */
async function sha256File(path) {
  return await new Promise((resolvePromise, reject) => {
    const hash = createHash('sha256')
    const stream = createReadStream(path)
    stream.on('error', reject)
    stream.on('data', chunk => hash.update(chunk))
    stream.on('end', () => resolvePromise(hash.digest('hex')))
  })
}

/**
 * Package the staged closure as `node-app-<triple>.tar.gz` (top-level dir
 * `node-app`, so it extracts to `<runtime>/node-app`), copy it into
 * `src-tauri/resources/` so Tauri bundles it inside the .app, and write the
 * runtime manifest (version + sha256) the shell bakes in via include_str!.
 * An optional `url` is only recorded when RUNTIME_BASE_URL is set (kept for a
 * future hybrid/update path); the shipped app does not download anything.
 */
async function packageClosure(target, triple, version) {
  const packRoot = join(OUT_DIR, '.package')
  await rm(packRoot, { recursive: true, force: true })
  await mkdir(packRoot, { recursive: true })
  const stagedNodeApp = join(packRoot, 'node-app')
  await rename(STAGING, stagedNodeApp)

  const tarball = join(OUT_DIR, `node-app-${triple}.tar.gz`)
  await run('pack node-app', 'tar', ['-czf', tarball, '-C', packRoot, 'node-app'])

  const sha256 = await sha256File(tarball)
  const manifest = { version: process.env.RUNTIME_VERSION ?? version, sha256 }
  const base = (process.env.RUNTIME_BASE_URL ?? '').replace(/\/+$/, '')
  if (base) manifest.url = `${base}/${basename(tarball)}`
  const manifestPath = join(ROOT, 'src-tauri', 'runtime-manifest.json')
  await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`)

  // Ship the closure as a bundled resource (single file) rather than a
  // first-launch download, so the app is fully self-contained and offline.
  await mkdir(join(ROOT, 'src-tauri', 'resources'), { recursive: true })
  await cp(tarball, join(ROOT, 'src-tauri', 'resources', 'node-app-runtime.tar.gz'))

  console.log(`build-sidecar: packaged ${basename(tarball)} (${((await stat(tarball)).size / 1048576).toFixed(1)} MiB, sha256 ${sha256.slice(0, 12)}…)`)
  console.log(`build-sidecar: manifest -> ${manifestPath} (bundled resource${base ? `, url: ${base}/${basename(tarball)}` : ''})`)

  await rm(packRoot, { recursive: true, force: true })
  return [tarball, manifestPath]
}

async function assemble(target, noStrip, version) {
  const triple = RUST_TRIPLES[target]
  if (triple === undefined) throw new Error(`build-sidecar: unsupported target ${JSON.stringify(target)}.`)

  // 1. Verify the entry the shell will spawn before packaging.
  const entry = join(STAGING, 'lib', 'bin.js')
  if (!existsSync(entry)) {
    throw new Error(`build-sidecar: ${entry} missing — build the harness first (lib/ artifacts are required).`)
  }

  // 2. Drop per-target dead weight from the closure (e.g. node-pty win32 prebuilds).
  await pruneTargetPrebuilds(STAGING, target)

  // 3. Place the node binary as the Tauri externalBin, then strip (+re-sign).
  const nodeBin = await fetchNodeBinary(target)
  const ext = target.startsWith('win-') ? '.exe' : ''
  const dest = join(OUT_DIR, `node-${triple}${ext}`)
  await cp(nodeBin, dest)
  await chmod(dest, 0o755)
  if (!noStrip) await stripNodeBinary(dest, target)

  // 4. Drop the extracted node dist dir; keep the archive as a download cache.
  const nodePlatform = NODE_PLATFORMS[target]
  await rm(join(OUT_DIR, `node-${NODE_VERSION}-${nodePlatform}`), { recursive: true, force: true })

  // 5. Package the closure for first-launch download + write the manifest.
  const [tarball, manifestPath] = await packageClosure(target, triple, version)

  // 6. Remove any stale bundled closure from the pre-external-download layout.
  await rm(NODE_APP_DIR, { recursive: true, force: true })

  return [dest, tarball, manifestPath]
}

async function main() {
  const { skipBuild, dryRun, noStrip, targets } = parseArgs(process.argv.slice(2))
  console.log(`build-sidecar: harness: ${HARNESS_ROOT}`)
  console.log(`build-sidecar: targets: ${targets.join(', ')}`)
  if (!existsSync(HARNESS_ROOT)) {
    console.error(`build-sidecar: harness checkout not found at ${HARNESS_ROOT}; init the submodule (git submodule update --init).`)
    process.exit(1)
  }
  if (dryRun) {
    console.log('build-sidecar: [dry-run] would deploy + assemble the node carrier (no-op).')
    return
  }
  await deployStaging(skipBuild)
  const version = await detectHarnessVersion()
  await syncDesktopVersion(version)
  const products = []
  for (const target of targets) products.push(...await assemble(target, noStrip, version))
  console.log('build-sidecar: products:')
  for (const path of products) console.log(`  ${path}`)
  console.log('build-sidecar: done. Move on to `pnpm run build` (tauri build).')
}

await main()
