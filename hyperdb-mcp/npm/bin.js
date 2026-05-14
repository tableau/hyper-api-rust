#!/usr/bin/env node
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

const { execFileSync } = require('child_process')
const { join, dirname } = require('path')
const { existsSync } = require('fs')

const { platform, arch } = process

function getPlatformPackage() {
  switch (platform) {
    case 'darwin':
      // darwin-x64 builds are disabled until macos-13 GHA runners are
      // reliable again — see npm-build-publish.yml matrix.
      return arch === 'arm64' ? 'hyperdb-mcp-darwin-arm64' : null
    case 'linux':
      return 'hyperdb-mcp-linux-x64-gnu'
    case 'win32':
      return 'hyperdb-mcp-win32-x64-msvc'
    default:
      return null
  }
}

function getBinaryName() {
  return platform === 'win32' ? 'hyperdb-mcp.exe' : 'hyperdb-mcp'
}

function getHyperdName() {
  return platform === 'win32' ? 'hyperd.exe' : 'hyperd'
}

function findBinary() {
  const pkg = getPlatformPackage()
  if (!pkg) {
    throw new Error(`Unsupported platform: ${platform}-${arch}`)
  }

  // Try resolving from the installed platform package
  try {
    const pkgDir = dirname(require.resolve(`${pkg}/package.json`))
    const bin = join(pkgDir, getBinaryName())
    if (existsSync(bin)) return { bin, dir: pkgDir }
  } catch (_) {}

  // Fallback: binary in platform subdirectory (local dev / assemble-npm.sh)
  const platformDir = pkg.replace('hyperdb-mcp-', '')
  const subdir = join(__dirname, platformDir)
  const subdirBin = join(subdir, getBinaryName())
  if (existsSync(subdirBin)) return { bin: subdirBin, dir: subdir }

  // Fallback: binary in same directory
  const localBin = join(__dirname, getBinaryName())
  if (existsSync(localBin)) return { bin: localBin, dir: __dirname }

  throw new Error(
    `Could not find hyperdb-mcp binary for ${platform}-${arch}. ` +
    `Expected platform package: ${pkg}`
  )
}

const { bin, dir } = findBinary()

// Point hyperdb-mcp at the bundled hyperd if not already set
if (!process.env.HYPERD_PATH) {
  const hyperd = join(dir, getHyperdName())
  if (existsSync(hyperd)) {
    process.env.HYPERD_PATH = hyperd
  }
}

// Spawn the MCP server, inheriting stdio for MCP protocol communication
const result = require('child_process').spawnSync(bin, process.argv.slice(2), {
  stdio: 'inherit',
  env: process.env,
})

if (result.error) {
  throw result.error
}

process.exit(result.status ?? 1)
