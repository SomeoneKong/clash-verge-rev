// Builds the privileged update component into the app's resources, and after the app is built,
// the signed payloads the component applies.
//
//   node scripts/fork-updater-payload.mjs prepare <target>
//   node scripts/fork-updater-payload.mjs payloads <target> <tag> <out dir>
//
// `prepare` runs after `prebuild` and before `tauri build`. `payloads` signs with the updater key
// from TAURI_SIGNING_PRIVATE_KEY / TAURI_SIGNING_PRIVATE_KEY_PASSWORD and writes, beside the
// zips, `fork-payloads-<platform>.json`, which `fork-update-json.mjs` reads.
import { execFileSync } from 'node:child_process'
import crypto from 'node:crypto'
import fs from 'node:fs'
import path from 'node:path'

import AdmZip from 'adm-zip'

const PLATFORMS = {
  'x86_64-pc-windows-msvc': 'windows-x86_64',
  'x86_64-unknown-linux-gnu': 'linux-x86_64',
  'aarch64-unknown-linux-gnu': 'linux-aarch64',
  'aarch64-apple-darwin': 'darwin-aarch64',
}

const [command, target, tag, outDir] = process.argv.slice(2)
const platform = PLATFORMS[target]
if (!platform) throw new Error(`no update payloads for ${target}`)
const windows = platform.startsWith('windows')
const macos = platform.startsWith('darwin')
const exe = windows ? '.exe' : ''
const root = process.cwd()
const tauriDir = path.join(root, 'src-tauri')
const resources = path.join(tauriDir, 'resources')
const release = path.join(root, 'target', target, 'release')
const updaterName = `clash-verge-updater${exe}`

const sha256 = (data) => crypto.createHash('sha256').update(data).digest('hex')
const fileSha = (file) => sha256(fs.readFileSync(file))

const canonical = (value) => {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`
  if (value && typeof value === 'object') {
    const keys = Object.keys(value).sort()
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonical(value[k])}`).join(',')}}`
  }
  return JSON.stringify(value)
}

// What the installer leaves on the machine, except what a patch carries (the app, the cores and
// the component), the per-build core digests, and the geo data that mihomo keeps updating itself.
function fingerprint() {
  const lines = []
  const scripts = path.join(tauriDir, 'packages', 'windows')
  for (const name of fs.readdirSync(scripts).sort()) {
    if (name === 'core-hashes.nsh') continue
    const text = fs.readFileSync(path.join(scripts, name), 'utf8')
    lines.push(
      `packages/windows/${name} ${sha256(text.replace(/\r\n/g, '\n'))}`,
    )
  }
  for (const name of ['tauri.conf.json', 'tauri.windows.conf.json']) {
    const config = JSON.parse(
      fs.readFileSync(path.join(tauriDir, name), 'utf8'),
    )
    delete config.version
    lines.push(`${name} ${sha256(canonical(config))}`)
  }
  for (const name of fs.readdirSync(resources).sort()) {
    if (/\.(mmdb|metadb|dat)$/.test(name)) continue
    if (name === updaterName || name === 'install-fingerprint') continue
    lines.push(`resources/${name} ${fileSha(path.join(resources, name))}`)
  }
  return sha256(lines.join('\n'))
}

function prepare() {
  // The installer runs the component before it installs the VC runtime, so on Windows it links
  // the C runtime statically; its own target directory keeps those flags apart from the app's.
  const targetDir = path.join(root, 'target', 'updater')
  execFileSync(
    'cargo',
    [
      'build',
      '--release',
      '-p',
      'clash-verge-updater',
      '--target',
      target,
      '--target-dir',
      targetDir,
    ],
    {
      stdio: 'inherit',
      env: windows
        ? { ...process.env, RUSTFLAGS: '-C target-feature=+crt-static' }
        : process.env,
    },
  )
  const updater = path.join(resources, updaterName)
  fs.copyFileSync(path.join(targetDir, target, 'release', updaterName), updater)
  if (!windows) fs.chmodSync(updater, 0o755)
  if (windows) {
    const value = fingerprint()
    fs.writeFileSync(path.join(resources, 'install-fingerprint'), `${value}\n`)
    console.log(`install fingerprint ${value}`)
  }
}

const sidecar = (name) =>
  path.join(tauriDir, 'sidecar', `${name}-${target}${exe}`)
// Linux bundles the service as a sidecar; Windows and macOS keep it in the resources.
const serviceBinary = () =>
  windows || macos
    ? path.join(resources, `clash-verge-service${exe}`)
    : sidecar('clash-verge-service')

// `Mihomo Meta v1.19.34-custom.2 windows amd64 with go1.27.1 ...`
function coreVersion(file) {
  const words = execFileSync(file, ['-v'], { encoding: 'utf8' }).split(/\s+/)
  if (!words[2]) throw new Error(`${file} -v printed no version`)
  return words[2]
}

function cores() {
  return ['verge-mihomo', 'verge-mihomo-alpha'].map((name) => ({
    name,
    version: coreVersion(sidecar(name)),
    sha256: fileSha(sidecar(name)),
  }))
}

function findOne(dir, version, suffix) {
  const names = fs
    .readdirSync(dir)
    .filter((n) => n.includes(`_${version}_`) && n.endsWith(suffix))
  if (names.length !== 1) {
    throw new Error(`expected one ${suffix} in ${dir}, found ${names.length}`)
  }
  return path.join(dir, names[0])
}

function signature(file) {
  const cli = path.join(root, 'node_modules', '@tauri-apps', 'cli', 'tauri.js')
  execFileSync(process.execPath, [cli, 'signer', 'sign', file], {
    stdio: 'inherit',
  })
  const sig = `${file}.sig`
  const value = fs.readFileSync(sig, 'utf8').trim()
  fs.rmSync(sig)
  return value
}

function writePayload(kind, files, extra) {
  const version = tag.replace(/^v/, '')
  const manifest = {
    format: 1,
    version,
    platform,
    kind,
    ...extra,
    files: files.map(({ name, source, role }) => ({
      path: name,
      sha256: fileSha(source),
      role,
    })),
    cores: cores(),
    service: fileSha(serviceBinary()),
    updater: fileSha(path.join(resources, updaterName)),
    commit:
      process.env.GITHUB_SHA ??
      execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
    built_at: new Date().toISOString(),
  }
  const zip = new AdmZip()
  zip.addFile(
    'manifest.json',
    Buffer.from(`${JSON.stringify(manifest, null, 2)}\n`),
  )
  for (const { name, source } of files)
    zip.addFile(name, fs.readFileSync(source))
  const name = `Clash.Verge_${version}_${platform}.${kind}.zip`
  const file = path.join(outDir, name)
  zip.writeZip(file)
  const bytes = fs.readFileSync(file)
  return {
    name,
    size: bytes.length,
    sha256: sha256(bytes),
    signatures: [signature(file)],
  }
}

function payloads() {
  if (!tag || !outDir) throw new Error('payloads needs <tag> and <out dir>')
  fs.mkdirSync(outDir, { recursive: true })
  const bundle = path.join(release, 'bundle')
  const version = tag.replace(/^v/, '')
  const entry = {}
  if (windows) {
    const value = fs
      .readFileSync(path.join(resources, 'install-fingerprint'), 'utf8')
      .trim()
    entry.fingerprint = value
    entry.patch = writePayload(
      'patch',
      [
        {
          name: 'clash-verge.exe',
          source: path.join(release, 'clash-verge.exe'),
          role: 'gui',
        },
        {
          name: 'verge-mihomo.exe',
          source: sidecar('verge-mihomo'),
          role: 'core',
        },
        {
          name: 'verge-mihomo-alpha.exe',
          source: sidecar('verge-mihomo-alpha'),
          role: 'core',
        },
        {
          name: 'clash-verge-service.exe',
          source: serviceBinary(),
          role: 'service',
        },
        {
          name: 'clash-verge-service-install.exe',
          source: path.join(resources, 'clash-verge-service-install.exe'),
          role: 'service_tool',
        },
        {
          name: updaterName,
          source: path.join(resources, updaterName),
          role: 'updater',
        },
      ],
      { base_fingerprint: value, fingerprint: value },
    )
    const setup = findOne(path.join(bundle, 'nsis'), version, '-setup.exe')
    entry.full = writePayload(
      'full',
      [
        {
          name: path.basename(setup).replaceAll(' ', '.'),
          source: setup,
          role: 'installer',
        },
      ],
      { fingerprint: value },
    )
  } else if (macos) {
    // The app archive keeps its updater signature beside it; the GUI's updater checks that
    // signature again when it installs the app from the payload.
    const app = path.join(bundle, 'macos', 'Clash Verge.app.tar.gz')
    entry.bundle = writePayload(
      'bundle',
      [
        { name: 'Clash.Verge.app.tar.gz', source: app, role: 'bundle' },
        { name: 'verge-mihomo', source: sidecar('verge-mihomo'), role: 'core' },
        {
          name: 'verge-mihomo-alpha',
          source: sidecar('verge-mihomo-alpha'),
          role: 'core',
        },
        {
          name: 'clash-verge-service',
          source: serviceBinary(),
          role: 'service',
        },
        {
          name: 'clash-verge-service-install',
          source: path.join(resources, 'clash-verge-service-install'),
          role: 'service_tool',
        },
        {
          name: updaterName,
          source: path.join(resources, updaterName),
          role: 'updater',
        },
      ],
      { bundle: { signature: fs.readFileSync(`${app}.sig`, 'utf8').trim() } },
    )
  } else {
    const deb = findOne(path.join(bundle, 'deb'), version, '.deb')
    entry.deb = writePayload(
      'deb',
      [
        {
          name: path.basename(deb).replaceAll(' ', '.'),
          source: deb,
          role: 'package',
        },
      ],
      {},
    )
  }
  fs.writeFileSync(
    path.join(outDir, `fork-payloads-${platform}.json`),
    `${JSON.stringify(entry, null, 2)}\n`,
  )
  console.log(JSON.stringify(entry, null, 2))
}

if (command === 'prepare') prepare()
else if (command === 'payloads') payloads()
else throw new Error(`unknown command ${command}`)
