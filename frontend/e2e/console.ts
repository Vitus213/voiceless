import { expect, test as base } from '@playwright/test';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { delimiter, join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

const executable = process.env.VOICELESS_BIN
  ? resolve(process.env.VOICELESS_BIN)
  : fileURLToPath(new URL('../../rust/target/debug/voiceless', import.meta.url));
const sessionConfig = fileURLToPath(new URL('./fixtures/session.conf', import.meta.url));

type Launch = {
  daemon: boolean;
  /** Stand-ins for the desktop tools, put first on PATH. */
  bin?: string;
  /** More environment for the server; `undefined` removes a variable. */
  env?: Record<string, string | undefined>;
  /** Runs in the fresh root before the server starts, e.g. to seed models. */
  prepare?: (root: string) => Promise<void>;
};

type Running = { url: string; root: string };

/**
 * A console over a fresh XDG root (no configuration, history or models),
 * closed once `use` settles. `adopt` turns what is running into the value
 * `use` receives.
 */
export async function withConsole<T>(
  use: (value: T) => Promise<void>,
  { daemon, bin, env, prepare }: Launch,
  adopt: (running: Running) => T = (running) => running.url as T,
) {
  const root = await mkdtemp(join(tmpdir(), 'voiceless-console-'));
  try {
    const config = join(root, 'config.toml');
    if (daemon) await writeFile(config, '[console]\nport = 0\n[audio]\npause_media = false\n', { mode: 0o600 });
    await prepare?.(root);
    const server = daemon ? 'dbus-run-session' : executable;
    const args = daemon ? [`--config-file=${sessionConfig}`, '--', executable, '--config', config, 'daemon', '--no-inject']
      : ['web', '--no-open', '--port', '0'];
    const child = spawn(server, args, {
      env: {
        ...process.env,
        XDG_CACHE_HOME: join(root, 'cache'),
        XDG_CONFIG_HOME: join(root, 'config'),
        XDG_DATA_HOME: join(root, 'data'),
        XDG_STATE_HOME: join(root, 'state'),
        ...(bin && { PATH: `${bin}${delimiter}${process.env.PATH}` }),
        ...env,
      },
      detached: daemon,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    const lines = createInterface({ input: child.stdout });
    let stderr = '';
    child.stderr.setEncoding('utf8').on('data', (chunk: string) => { stderr += chunk; });
    const closed = once(child, 'close').catch(() => undefined);
    try {
      const { promise, resolve, reject } = Promise.withResolvers<string>();
      const deadline = setTimeout(() => reject(new Error(`Voiceless console did not start\n${stderr}`)), 8_000);
      lines.on('line', (line) => {
        const match = /^Voiceless console: (http:\/\/127\.0\.0\.1:\d+\/)$/u.exec(line);
        if (match) resolve(match[1]);
      });
      child.once('error', reject);
      child.once('close', (code) => reject(new Error(`Voiceless console exited with ${code}\n${stderr}`)));
      const url = await promise.finally(() => clearTimeout(deadline));
      await expect.poll(async () => (await fetch(url)).status).toBe(200);
      await use(adopt({ url, root }));
    } finally {
      lines.close();
      if (daemon && child.pid && child.exitCode === null && child.signalCode === null) {
        process.kill(-child.pid, 'SIGTERM');
      } else {
        child.kill('SIGKILL');
      }
      await closed;
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

// One console per test: a test that saves settings must not change what the next one reads.
export const test = base.extend<{ consoleUrl: string; daemonUrl: string }, object>({
  consoleUrl: async ({}, use) => withConsole(use, { daemon: false }),
  daemonUrl: async ({}, use) => withConsole(use, { daemon: true }),
});

export { expect };
