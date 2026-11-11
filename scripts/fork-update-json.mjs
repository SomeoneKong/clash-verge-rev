// Writes the update.json of a fork release from its assets and their updater signatures.
//
//   node scripts/fork-update-json.mjs <assets dir> <tag> <notes file>
//
// The signatures are embedded, so the .sig files are removed from the assets afterwards.
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
const platforms = {}
for (const { suffix, platforms: keys } of targets) {
  const name = names.find((n) => n.endsWith(suffix))
  if (!name) throw new Error(`no asset ending with ${suffix}`)
  const sig = path.join(dir, `${name}.sig`)
  if (!fs.existsSync(sig)) throw new Error(`${name} has no updater signature`)
  const entry = {
    signature: fs.readFileSync(sig, 'utf8').trim(),
    url: `https://github.com/${repo}/releases/download/${tag}/${name}`,
  }
  for (const key of keys) platforms[key] = entry
}

const update = {
  version: tag.replace(/^v/, ''),
  notes: fs.readFileSync(notesFile, 'utf8'),
  pub_date: new Date().toISOString(),
  platforms,
}
fs.writeFileSync(
  path.join(dir, 'update.json'),
  `${JSON.stringify(update, null, 2)}\n`,
)
for (const name of names.filter((n) => n.endsWith('.sig'))) {
  fs.rmSync(path.join(dir, name))
}
console.log(JSON.stringify({ ...update, notes: '...' }, null, 2))
