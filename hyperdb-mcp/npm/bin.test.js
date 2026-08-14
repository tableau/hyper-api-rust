// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

'use strict'

const test = require('node:test')
const assert = require('node:assert/strict')
const { spawnSync } = require('node:child_process')
const { resolve } = require('node:path')

const launcherModule = resolve(__dirname, 'bin.js')

function runLauncherModule(scriptBody) {
  return spawnSync(
    process.execPath,
    ['-e', `const launcherModule = ${JSON.stringify(launcherModule)};\n${scriptBody}`],
    { encoding: 'utf8' }
  )
}

function assertChildSucceeded(result) {
  assert.equal(
    result.status,
    0,
    `child exited ${result.status}; signal=${result.signal}; stderr=${result.stderr}`
  )
}

test('launcher_module_is_import_safe', () => {
  const result = runLauncherModule(`
    const launcher = require(launcherModule)
    process.stdout.write(JSON.stringify({
      buildLauncherInfo: typeof launcher.buildLauncherInfo,
      launch: typeof launcher.launch,
    }))
  `)

  assertChildSucceeded(result)
  assert.deepEqual(JSON.parse(result.stdout), {
    buildLauncherInfo: 'function',
    launch: 'function',
  })
})

test('launcher_info_contains_only_allowlisted_fields', () => {
  const result = runLauncherModule(`
    const { buildLauncherInfo } = require(launcherModule)
    const secret = 'NODE_SECRET_SENTINEL_00e727'
    const withVersions = buildLauncherInfo({
      wrapper: {
        name: 'hyperdb-mcp',
        version: '1.2.3',
        package_path: '/wrapper/package.json',
        token: secret,
      },
      platform: {
        name: 'hyperdb-mcp-linux-x64-gnu',
        version: '1.2.3',
        package_path: '/platform/package.json',
        credentials: { secret },
      },
      executable_path: '/platform/hyperdb-mcp',
      environment: { secret },
    })
    const sourceManifests = buildLauncherInfo({
      wrapper: { name: 'hyperdb-mcp', package_path: '/source/package.json' },
      platform: {
        name: 'hyperdb-mcp-linux-x64-gnu',
        package_path: '/source/linux-x64-gnu/package.json',
      },
      executable_path: '/source/linux-x64-gnu/hyperdb-mcp',
    })
    process.stdout.write(JSON.stringify({ withVersions, sourceManifests }))
  `)

  assertChildSucceeded(result)
  assert.deepEqual(JSON.parse(result.stdout), {
    withVersions: {
      wrapper: {
        name: 'hyperdb-mcp',
        version: '1.2.3',
        package_path: '/wrapper/package.json',
      },
      platform: {
        name: 'hyperdb-mcp-linux-x64-gnu',
        version: '1.2.3',
        package_path: '/platform/package.json',
      },
      executable_path: '/platform/hyperdb-mcp',
    },
    sourceManifests: {
      wrapper: {
        name: 'hyperdb-mcp',
        version: null,
        package_path: '/source/package.json',
      },
      platform: {
        name: 'hyperdb-mcp-linux-x64-gnu',
        version: null,
        package_path: '/source/linux-x64-gnu/package.json',
      },
      executable_path: '/source/linux-x64-gnu/hyperdb-mcp',
    },
  })
  assert.doesNotMatch(result.stdout, /NODE_SECRET_SENTINEL_00e727/)
})

test('launcher_preserves_case_insensitive_configured_hyperd', () => {
  const result = runLauncherModule(`
    const { prepareLauncherEnvironment } = require(launcherModule)
    const launcher_info = {
      wrapper: {
        name: 'hyperdb-mcp',
        version: '1.2.3',
        package_path: '/wrapper/package.json',
      },
      platform: {
        name: 'hyperdb-mcp-win32-x64-msvc',
        version: '1.2.3',
        package_path: '/platform/package.json',
      },
      executable_path: '/platform/hyperdb-mcp.exe',
    }
    const inheritedConfigured = {
      Path: 'C:\\\\Windows\\\\System32',
      Hyperd_Path: '/configured/hyperd',
      KEEP_ME: 'configured',
    }
    const inheritedBundled = {
      Path: 'C:\\\\Windows\\\\System32',
      KEEP_ME: 'bundled',
    }
    const inheritedEmpty = {
      Path: 'C:\\\\Windows\\\\System32',
      HYPERD_PATH: '',
      KEEP_ME: 'empty',
    }
    const configured = prepareLauncherEnvironment({
      inherited_env: inheritedConfigured,
      configured_hyperd: '/configured/hyperd',
      bundled_hyperd: '/bundled/hyperd',
      launcher_info,
    })
    const bundled = prepareLauncherEnvironment({
      inherited_env: inheritedBundled,
      configured_hyperd: undefined,
      bundled_hyperd: '/bundled/hyperd',
      launcher_info,
    })
    const emptyConfigured = prepareLauncherEnvironment({
      inherited_env: inheritedEmpty,
      configured_hyperd: '',
      bundled_hyperd: '/bundled/hyperd',
      launcher_info,
    })
    process.stdout.write(JSON.stringify({
      configured,
      bundled,
      emptyConfigured,
      inheritedConfigured,
      inheritedBundled,
      inheritedEmpty,
    }))
  `)

  assertChildSucceeded(result)
  const launcherInfoJson = '{"wrapper":{"name":"hyperdb-mcp","version":"1.2.3","package_path":"/wrapper/package.json"},"platform":{"name":"hyperdb-mcp-win32-x64-msvc","version":"1.2.3","package_path":"/platform/package.json"},"executable_path":"/platform/hyperdb-mcp.exe"}'
  assert.deepEqual(JSON.parse(result.stdout), {
    configured: {
      Path: 'C:\\Windows\\System32',
      Hyperd_Path: '/configured/hyperd',
      KEEP_ME: 'configured',
      HYPERDB_MCP_LAUNCHER_INFO: launcherInfoJson,
    },
    bundled: {
      Path: 'C:\\Windows\\System32',
      KEEP_ME: 'bundled',
      HYPERD_PATH: '/bundled/hyperd',
      HYPERDB_MCP_LAUNCHER_INFO: launcherInfoJson,
    },
    emptyConfigured: {
      Path: 'C:\\Windows\\System32',
      HYPERD_PATH: '/bundled/hyperd',
      KEEP_ME: 'empty',
      HYPERDB_MCP_LAUNCHER_INFO: launcherInfoJson,
    },
    inheritedConfigured: {
      Path: 'C:\\Windows\\System32',
      Hyperd_Path: '/configured/hyperd',
      KEEP_ME: 'configured',
    },
    inheritedBundled: {
      Path: 'C:\\Windows\\System32',
      KEEP_ME: 'bundled',
    },
    inheritedEmpty: {
      Path: 'C:\\Windows\\System32',
      HYPERD_PATH: '',
      KEEP_ME: 'empty',
    },
  })
  assert.equal(
    Object.hasOwn(JSON.parse(result.stdout).configured, 'HYPERD_PATH'),
    false,
    'a case-insensitive configured value must not gain a competing uppercase key'
  )
})

test('launcher_preserves_spawn_error_semantics', () => {
  const result = runLauncherModule(`
    const { launch } = require(launcherModule)
    const expected = new Error('spawn failed')
    let sameError = false
    try {
      launch({
        executable_path: '/platform/hyperdb-mcp',
        args: [],
        env: {},
        spawnSync: () => ({ error: expected }),
      })
    } catch (error) {
      sameError = error === expected
    }
    process.stdout.write(JSON.stringify({ sameError }))
  `)

  assertChildSucceeded(result)
  assert.deepEqual(JSON.parse(result.stdout), { sameError: true })
})

test('launcher_preserves_numeric_exit_status', () => {
  const result = runLauncherModule(`
    const { launch } = require(launcherModule)
    let observed
    const status = launch({
      executable_path: '/platform/hyperdb-mcp',
      args: ['--read-only'],
      env: { HYPERD_PATH: '/platform/hyperd' },
      spawnSync: (file, args, options) => {
        observed = { file, args, options }
        return { status: 37, signal: null }
      },
    })
    process.stdout.write(JSON.stringify({ status, observed }))
  `)

  assertChildSucceeded(result)
  assert.deepEqual(JSON.parse(result.stdout), {
    status: 37,
    observed: {
      file: '/platform/hyperdb-mcp',
      args: ['--read-only'],
      options: {
        stdio: 'inherit',
        env: { HYPERD_PATH: '/platform/hyperd' },
      },
    },
  })
})

test('launcher_preserves_signal_termination', () => {
  const result = runLauncherModule(`
    const { launch } = require(launcherModule)
    const status = launch({
      executable_path: '/platform/hyperdb-mcp',
      args: [],
      env: {},
      spawnSync: () => ({ status: null, signal: 'SIGTERM' }),
    })
    process.stdout.write(JSON.stringify({ status }))
  `)

  assertChildSucceeded(result)
  assert.deepEqual(JSON.parse(result.stdout), { status: 1 })
})
