/// <reference types="node" />
// The launcher's startup: see ../launcher.ts.
import { writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { chdir, cwd, env } from 'node:process'
import { fileURLToPath, pathToFileURL } from 'node:url'
import type { Context } from '@deepseek-ai/cordis'
import Loader from '@deepseek-ai/cordis-plugin-loader'
import {
  PluginPackages, auditStartupEntries, createRuntimeResolution, initProfile, loadLayeredEnv,
  loadProfileDirectory, mountRootInclude, readProfilePatches, reportSkippedBundles, resolveProfileDir,
} from '@deepseek-ai/dsh-app-boot'
import type { ProfileContext } from '@deepseek-ai/dsh-app-boot'
import { provideCmdline } from '@deepseek-ai/dsh-cmdline'
import { dshHomePath, resolveDshHome } from '@deepseek-ai/dsh-home-paths'
import { DSH_LAUNCH_ENVIRONMENT_KEY } from '@deepseek-ai/dsh-launch-environment'
import { PROFILE_ROOT_FILENAME } from '@deepseek-ai/dsh/profile-boot'
import type { Config } from '../launcher.ts'

/** Bundles of a new profile: the shared dsh core, the web app and the aimux routes. */
const BUNDLES = ['@deepseek-ai/dsh-base', '@deepseek-ai/dsh-web-app', '@rutis/dsh-aimux']

/** Start the profile; see `apply` in ../launcher.ts for why this is synchronous. */
export function boot(ctx: Context, config: Config) {
  // The runner starts in the runtime package; dsh treats the working
  // directory as the workspace and reads `.env` files from it.
  if (config.cwd) chdir(config.cwd)
  // This npm project is the installation scope the bundles resolve from.
  const installAnchor = fileURLToPath(new URL('../package.json', import.meta.url))
  const profileName = config.profile ?? 'rutis-web'
  const dir = resolveProfileDir(profileName)
  initProfile(dir, BUNDLES)
  const profile = loadProfileDirectory('rutis-dsh', dir, installAnchor)
  reportSkippedBundles('rutis-dsh', profile)
  // Everything comes from patch layers; the root include starts empty.
  const rootConfig = join(profile.dir, PROFILE_ROOT_FILENAME)
  writeFileSync(rootConfig, '[]\n')
  const profileContext: ProfileContext = {
    name: profileName,
    dir: profile.dir,
    patchPath: profile.patchPath,
    installAnchor,
    cwd: cwd(),
    home: resolveDshHome(),
    startedBundles: profile.layers.map(layer => layer.packageName),
    overlays: [],
    telemetryDisabledEnv: env.DSH_TELEMETRY_DISABLED,
  }
  const patches = readProfilePatches('rutis-dsh', profileContext, profile)

  let ready = false
  const waiting = new Set<() => void>()
  ctx.provide('dshHomePath', dshHomePath)
  ctx.provide('profileContext', profileContext)
  ctx.provide(DSH_LAUNCH_ENVIRONMENT_KEY, loadLayeredEnv('rutis-dsh'))
  provideCmdline(ctx, {
    args: config.args ?? [],
    exit: code => { ctx.emit('rutis-dsh/exit', code) },
    ready: {
      onReady(listener) {
        if (ready) { listener(); return () => {} }
        waiting.add(listener)
        return () => { waiting.delete(listener) }
      },
    },
  })

  // As dsh-app-boot does: a configuration update runs without holding up the
  // dispatch that started it, and its failure is logged.
  const on = ctx.on.bind(ctx) as unknown as (name: string, listener: (config: unknown, noSave: unknown, next: () => unknown) => void, options: object) => void
  on('internal/update', (_config: unknown, _noSave: unknown, next: () => unknown) => {
    Promise.resolve(next()).catch(error => { ctx.logger.error(error) })
  }, { global: true, prepend: true })

  // A failed startup otherwise only reaches ctx.logger, which nothing
  // exports here: report it to the host.
  const failed = (error: unknown) => {
    const message = error instanceof Error ? error.message : String(error)
    console.error(message)
    ctx.emit('rutis-dsh/startup-failed', message)
  }
  ctx.plugin({
    name: 'rutis-dsh-launcher:loader',
    async apply(ctx: Context) {
      try {
        ctx.plugin(PluginPackages, { resolution: await createRuntimeResolution({ installAnchor, profile }) })
      } catch (error) {
        failed(error)
        throw error
      }
      ctx.plugin(Loader, { baseUrl: pathToFileURL(profile.dir).href + '/' })
      ctx.plugin({
        name: 'rutis-dsh-launcher:tree',
        inject: ['loader', 'pluginPackages'],
        async apply(ctx: Context) {
          try {
            // Keyed by the root: settings saves reconcile the profile through it.
            await mountRootInclude(ctx.root, rootConfig, patches, undefined, 'rutis-dsh')
            await (ctx as unknown as { loader: { await(): Promise<void> } }).loader.await()
            await auditStartupEntries(ctx.root, 'rutis-dsh')
          } catch (error) {
            failed(error)
            throw error
          }
          ready = true
          for (const listener of waiting) listener()
          waiting.clear()
          const llm = ctx.get('llm') as { listProviders(): { id: string }[] } | undefined
          ctx.emit('rutis-dsh/ready', llm?.listProviders().map(provider => provider.id) ?? [])
        },
      })
    },
  })
}
