// Writes the update.json of a fork release from its assets and their updater signatures.
//
//   node scripts/fork-update-json.mjs <assets dir> <tag> <notes file>
//
// The signatures are embedded, so the .sig files are removed from the assets afterwards. The
// payloads of the update component, described by `fork-payloads-<platform>.json` (see
// `fork-updater-payload.mjs`), go under `fork`, which the Tauri updater ignores.
import fs from 'node:fs'
import path from 'node:path'

const [dir, tag, notesFile] = process.argv.slice(2)
if (!dir || !tag || !notesFile) {
  throw new Error('usage: fork-update-json.mjs <assets dir> <tag> <notes file>')
}
const repo = process.env.GITHUB_REPOSITORY
if (!repo) throw new Error('GITHUB_REPOSITORY is not set')

// The updater looks for `{os}-{arch}-{installer}`, then `{os}-{arch}`.
const targets = [
  {
    suffix: '_x64-setup.exe',
    platforms: ['windows-x86_64-nsis', 'windows-x86_64'],
  },
  {
    suffix: '_aarch64.app.tar.gz',
    platforms: ['darwin-aarch64-app', 'darwin-aarch64'],
  },
  { suffix: '_amd64.deb', platforms: ['linux-x86_64-deb'] },
  { suffix: '_arm64.deb', platforms: ['linux-aarch64-deb'] },
]

const names = fs.readdirSync(dir)
const url = (name) =>
  `https://github.com/${repo}/releases/download/${tag}/${name}`
const platforms = {}
for (const { suffix, platforms: keys } of targets) {
  const name = names.find((n) => n.endsWith(suffix))
  if (!name) throw new Error(`no asset ending with ${suffix}`)
  const sig = path.join(dir, `${name}.sig`)
  if (!fs.existsSync(sig)) throw new Error(`${name} has no updater signature`)
  const entry = {
    signature: fs.readFileSync(sig, 'utf8').trim(),
    url: url(name),
  }
  for (const key of keys) platforms[key] = entry
}

const payloads = {}
for (const name of names.filter((n) => /^fork-payloads-.+\.json$/.test(n))) {
  const platform = name.slice('fork-payloads-'.length, -'.json'.length)
  const entry = JSON.parse(fs.readFileSync(path.join(dir, name), 'utf8'))
  for (const value of Object.values(entry)) {
    if (typeof value !== 'object') continue
    if (!names.includes(value.name))
      throw new Error(`${value.name} is not an asset`)
    value.url = url(value.name)
    delete value.name
  }
  payloads[platform] = entry
  fs.rmSync(path.join(dir, name))
}
for (const platform of [
  'windows-x86_64',
  'linux-x86_64',
  'linux-aarch64',
  'darwin-aarch64',
]) {
  if (!payloads[platform]) throw new Error(`no update payloads for ${platform}`)
}

const update = {
  version: tag.replace(/^v/, ''),
  notes: fs.readFileSync(notesFile, 'utf8'),
  pub_date: new Date().toISOString(),
  platforms,
  fork: { format: 1, payloads },
}
fs.writeFileSync(
  path.join(dir, 'update.json'),
  `${JSON.stringify(update, null, 2)}\n`,
)
for (const name of names.filter((n) => n.endsWith('.sig'))) {
  fs.rmSync(path.join(dir, name))
}
console.log(JSON.stringify({ ...update, notes: '...' }, null, 2))
