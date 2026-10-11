// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

/**
 * Copies the compiled native module to the correct npm platform package directory.
 * Used by CI after `cargo build --release --target <triple>`.
 *
 * Usage: node scripts/ci-copy-artifact.js <rust-target-triple>
 *
 * Example: node scripts/ci-copy-artifact.js aarch64-apple-darwin
 */
const { copyFileSync, existsSync } = require('fs');
const { join } = require('path');

const TARGET_MAP = {
  'aarch64-apple-darwin':        { npm: 'darwin-arm64',     lib: 'libhyperdb_api_node.dylib' },
  'x86_64-apple-darwin':         { npm: 'darwin-x64',       lib: 'libhyperdb_api_node.dylib' },
  'x86_64-unknown-linux-gnu':    { npm: 'linux-x64-gnu',    lib: 'libhyperdb_api_node.so' },
  'x86_64-pc-windows-msvc':      { npm: 'win32-x64-msvc',   lib: 'hyperdb_api_node.dll' },
};

const target = process.argv[2];
if (!target) {
  console.error('Usage: node scripts/ci-copy-artifact.js <rust-target-triple>');
  process.exit(1);
}

const mapping = TARGET_MAP[target];
if (!mapping) {
  console.error(`Unknown target: ${target}`);
  console.error('Known targets:', Object.keys(TARGET_MAP).join(', '));
  process.exit(1);
}

const rootDir = join(__dirname, '..');
const workspaceDir = join(rootDir, '..');
const src = join(workspaceDir, 'target', target, 'release', mapping.lib);
const npmDir = join(rootDir, 'npm', mapping.npm);

// Read the platform package.json to get the expected .node filename
const pkgJson = require(join(npmDir, 'package.json'));
const nodeFile = pkgJson.main;
const dest = join(npmDir, nodeFile);

if (!existsSync(src)) {
  console.error(`Build artifact not found: ${src}`);
  console.error(`Did 'cargo build -p hyperdb-api-node --release --target ${target}' succeed?`);
  process.exit(1);
}

copyFileSync(src, dest);
console.log(`Copied: ${src}`);
console.log(`    To: ${dest}`);
