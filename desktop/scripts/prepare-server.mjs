import { spawnSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const desktop = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const root = resolve(desktop, '..');
function run(command, args, cwd = root) {
  const result = spawnSync(command, args, { cwd, stdio: 'inherit', env: process.env });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
const rust = spawnSync('rustc', ['-vV'], { encoding: 'utf8' });
if (rust.error || rust.status !== 0) throw rust.error ?? new Error(rust.stderr);
const host = /^host: (.+)$/m.exec(rust.stdout)?.[1];
if (!host) throw new Error('Cannot determine Rust target');
const target = process.env.TAURI_ENV_TARGET_TRIPLE || host;
const args = ['build', '--release', '--manifest-path', join(root, 'server/Cargo.toml')];
if (target !== host) args.push('--target', target);
run('cargo', args);
const extension = target.includes('windows') ? '.exe' : '';
const targetDir = process.env.CARGO_TARGET_DIR ? resolve(root, process.env.CARGO_TARGET_DIR) : join(root, 'server/target');
const binary = join(targetDir, ...(target === host ? [] : [target]), 'release', `swiftcache${extension}`);
const destination = join(desktop, 'src-tauri/binaries');
mkdirSync(destination, { recursive: true });
copyFileSync(binary, join(destination, `swiftcache-${target}${extension}`));
if (!existsSync(join(desktop, 'src-tauri/icons/icon.png'))) {
  run(process.platform === 'win32' ? 'bun.exe' : 'bun', ['x', 'tauri', 'icon', 'src-tauri/icons/icon.svg'], desktop);
}
console.log(`SwiftCache service staged for ${target}`);
