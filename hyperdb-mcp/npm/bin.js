#!/usr/bin/env node
// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

const { spawnSync } = require('child_process')
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
    const packagePath = require.resolve(`${pkg}/package.json`)
    const pkgDir = dirname(packagePath)
    const bin = join(pkgDir, getBinaryName())
    if (existsSync(bin)) return { bin, dir: pkgDir, pkg, packagePath }
  } catch (_) {}

  // Fallback: binary in platform subdirectory (local dev / assemble-npm.sh)
  const platformDir = pkg.replace('hyperdb-mcp-', '')
  const subdir = join(__dirname, platformDir)
  const subdirBin = join(subdir, getBinaryName())
  const sourcePackagePath = join(subdir, 'package.json')
  if (existsSync(subdirBin)) {
    return {
      bin: subdirBin,
      dir: subdir,
      pkg,
      packagePath: sourcePackagePath,
    }
  }

  // Fallback: binary in same directory
  const localBin = join(__dirname, getBinaryName())
  if (existsSync(localBin)) {
    return {
      bin: localBin,
      dir: __dirname,
      pkg,
      // The manifest sits next to the binary in this branch — not in the
      // platform subdirectory `sourcePackagePath` points at (that path does
      // not exist here), so recompute it relative to __dirname.
      packagePath: join(__dirname, 'package.json'),
    }
  }

  throw new Error(
    `Could not find hyperdb-mcp binary for ${platform}-${arch}. ` +
    `Expected platform package: ${pkg}`
  )
}

function packageIdentity(packagePath, fallbackName) {
  let manifest = {}
  try {
    manifest = require(packagePath)
  } catch (_) {}

  return {
    name: typeof manifest.name === 'string' ? manifest.name : fallbackName,
    version: typeof manifest.version === 'string' ? manifest.version : null,
    package_path: packagePath,
  }
}

function buildLauncherInfo({ wrapper, platform, executable_path }) {
  return {
    wrapper: {
      name: wrapper.name,
      version: wrapper.version ?? null,
      package_path: wrapper.package_path,
    },
    platform: {
      name: platform.name,
      version: platform.version ?? null,
      package_path: platform.package_path,
    },
    executable_path,
  }
}

function prepareLauncherEnvironment({
  inherited_env,
  configured_hyperd,
  bundled_hyperd,
  launcher_info,
}) {
  const env = { ...inherited_env }
  if (!configured_hyperd && bundled_hyperd !== undefined) {
    env.HYPERD_PATH = bundled_hyperd
  }
  env.HYPERDB_MCP_LAUNCHER_INFO = JSON.stringify(launcher_info)
  return env
}

function launch({ executable_path, args, env, spawnSync }) {
  const result = spawnSync(executable_path, args, {
    stdio: 'inherit',
    env,
  })

  if (result.error) {
    throw result.error
  }

  return result.status ?? 1
}

function main() {
  const { bin, dir, pkg, packagePath } = findBinary()
  const configuredHyperd = process.env.HYPERD_PATH

  // Point hyperdb-mcp at the bundled hyperd if not already set
  const bundledHyperd = join(dir, getHyperdName())
  const launcherInfo = buildLauncherInfo({
    wrapper: packageIdentity(join(__dirname, 'package.json'), 'hyperdb-mcp'),
    platform: packageIdentity(packagePath, pkg),
    executable_path: bin,
  })
  const env = prepareLauncherEnvironment({
    inherited_env: process.env,
    configured_hyperd: configuredHyperd,
    bundled_hyperd: existsSync(bundledHyperd) ? bundledHyperd : undefined,
    launcher_info: launcherInfo,
  })

  // Spawn the MCP server, inheriting stdio for MCP protocol communication
  return launch({
    executable_path: bin,
    args: process.argv.slice(2),
    env,
    spawnSync,
  })
}

if (require.main === module) {
  process.exit(main())
}

module.exports = { buildLauncherInfo, prepareLauncherEnvironment, launch }
