import { type CheckOptions, Update } from '@tauri-apps/plugin-updater'

import { forkCheckUpdate } from './cmds'

export async function check(options: CheckOptions): Promise<Update | null> {
  const release = await forkCheckUpdate(
    options?.headers ? Array.from(new Headers(options.headers).entries()) : [],
    options?.timeout ?? null,
    options?.proxy ?? null,
    options?.target ?? null,
  )
  if (!release) return null
  return new Update({
    ...release,
    body: release.body ?? undefined,
    date:
      typeof release.rawJson.pub_date === 'string'
        ? release.rawJson.pub_date
        : undefined,
  })
}
