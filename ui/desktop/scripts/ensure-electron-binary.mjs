#!/usr/bin/env node

import fs from 'node:fs/promises';
import fsSync from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const uiRoot = path.resolve(scriptDir, '..', '..');
const hoistedElectronDir = path.join(uiRoot, 'node_modules', 'electron');
const hoistedDist = path.join(hoistedElectronDir, 'dist');
const hoistedPathTxt = path.join(hoistedElectronDir, 'path.txt');
const pnpmStoreDir = path.join(uiRoot, 'node_modules', '.pnpm');

function exists(filePath) {
  return fsSync.existsSync(filePath);
}

function fail(message) {
  console.error(`[ensure-electron] ${message}`);
  process.exit(1);
}

async function replaceWithSymlink(targetDist) {
  if (exists(targetDist)) {
    await fs.rm(targetDist, { recursive: true, force: true });
  }

  await fs.symlink(hoistedDist, targetDist, process.platform === 'win32' ? 'junction' : 'dir');
}

if (!exists(hoistedDist) || !exists(hoistedPathTxt)) {
  fail(`missing Electron binary under ${hoistedElectronDir}. Run "pnpm install" from ui/.`);
}

const executablePath = (await fs.readFile(hoistedPathTxt, 'utf8')).trim();
if (!executablePath) {
  fail(`empty Electron path file at ${hoistedPathTxt}. Run "pnpm install" from ui/.`);
}

const hoistedExecutable = path.join(hoistedDist, executablePath);
if (!exists(hoistedExecutable)) {
  fail(`missing Electron executable at ${hoistedExecutable}. Run "pnpm install" from ui/.`);
}

if (!exists(pnpmStoreDir)) {
  console.log('[ensure-electron] Electron binary ready');
  process.exit(0);
}

const entries = await fs.readdir(pnpmStoreDir, { withFileTypes: true });
let repaired = 0;
let checked = 0;

for (const entry of entries) {
  if (!entry.isDirectory() || !entry.name.startsWith('electron@')) {
    continue;
  }

  const electronDir = path.join(pnpmStoreDir, entry.name, 'node_modules', 'electron');
  if (!exists(electronDir)) {
    continue;
  }

  checked += 1;

  const targetPathTxt = path.join(electronDir, 'path.txt');
  const targetDist = path.join(electronDir, 'dist');
  const targetExecutable = path.join(targetDist, executablePath);

  await fs.writeFile(targetPathTxt, executablePath);

  if (!exists(targetExecutable)) {
    await replaceWithSymlink(targetDist);
    repaired += 1;
  }
}

if (checked === 0) {
  console.log('[ensure-electron] Electron binary ready');
} else {
  console.log(`[ensure-electron] Electron binary ready (${repaired} pnpm package link${repaired === 1 ? '' : 's'} repaired)`);
}
