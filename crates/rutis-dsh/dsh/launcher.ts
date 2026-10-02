/// <reference types="node" />
// Runs a dsh profile (by default the web UI) inside the Context rutis-interop
// mounts this plugin in, with the aimux routes as one bundle. It follows what
// `@deepseek-ai/dsh/profile-boot` and dsh-app-boot's `boot()` do, without
// creating a Context, installing signal handlers or exiting the process: the
// rutis host owns the process and hears about startup and exit as events.
import type { Context } from '@deepseek-ai/cordis'
// The `aimux` service the rutis host provides to the bundle.
import type {} from '@rutis/dsh-aimux/adapter'
import { boot } from './launcher/boot.ts'

declare module '@deepseek-ai/cordis' {
  interface Context {
    rutisDsh: Launched
  }
  interface Events {
    /** The profile started; `providers` are the dsh-llm routes now available. */
    'rutis-dsh/ready'(providers: string[]): void
    /** The profile failed to start; the mount stays up until the host disposes it. */
    'rutis-dsh/startup-failed'(message: string): void
    /** The dsh app asked to exit (e.g. `--help` or a usage error). */
    'rutis-dsh/exit'(code: number): void
  }
}

export interface Config {
  /** Profile under `$DSH_HOME/profiles`, created on first start with the web bundles. */
  profile?: string
  /** Command line of the dsh app, e.g. `['--port', '3080', '--no-open']`. */
  args?: string[]
  /** Workspace directory; defaults to the host's working directory. */
  cwd?: string
}

/**
 * The running profile. Exported to rutis: it is withdrawn when the dsh
 * process goes away, which is how the host notices the process ended.
 */
export class Launched {
  constructor(readonly profile: string) {}
}

export const name = 'rutis-dsh-launcher'

// apply stays synchronous: profile rows read the services `boot` provides
// (e.g. `!!js dshHomePath(...)`) from an ancestor fiber that is already active.
// They are provided in ./launcher/boot.ts so that only `rutisDsh` is exported.
export function apply(ctx: Context, config: Config = {}) {
  const profile = config.profile ?? 'rutis-web'
  ctx.provide('rutisDsh', new Launched(profile))
  boot(ctx, { ...config, profile })
}
