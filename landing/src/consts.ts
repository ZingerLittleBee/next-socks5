import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';

export const REPO = 'https://github.com/ZingerLittleBee/next-socks5';

export const INSTALL_CMD =
  'curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh | sh';

/**
 * Read the canonical project version from the workspace `Cargo.toml` at build
 * time, so the landing page never drifts from the actual released version.
 * Falls back to a hard-coded value if the file can't be read (e.g. the landing
 * folder is built in isolation).
 */
function readCargoVersion(): string {
  const FALLBACK = '0.7.0';
  try {
    // npm scripts run with cwd = landing/, so Cargo.toml sits one level up.
    const toml = readFileSync(resolve(process.cwd(), '../Cargo.toml'), 'utf-8');
    const match = toml.match(/^version\s*=\s*"([^"]+)"/m);
    return match?.[1] ?? FALLBACK;
  } catch {
    return FALLBACK;
  }
}

export const VERSION = readCargoVersion();

/** Page sections, in order. The tab bar, scrollspy and 1-5 shortcuts use this. */
export const SECTIONS = [
  { id: 'overview', label: 'Overview' },
  { id: 'dashboard', label: 'Dashboard' },
  { id: 'features', label: 'Features' },
  { id: 'perf', label: 'Perf' },
  { id: 'install', label: 'Install' },
] as const;
