// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

/* hyperdb-api-node native binding loader */
/* eslint-disable */

const { existsSync, readFileSync } = require('fs')
const { join, dirname } = require('path')

const { platform, arch } = process

let nativeBinding = null
let loadError = null

function isMusl() {
  if (!process.report || typeof process.report.getReport !== 'function') {
    try {
      // execFileSync avoids shell interpretation of the `which` argument.
      const { execFileSync } = require('child_process')
      const lddPath = execFileSync('which', ['ldd']).toString().trim()
      return readFileSync(lddPath, 'utf8').includes('musl')
    } catch (e) {
      return true
    }
  } else {
    const { glibcVersionRuntime } = process.report.getReport().header
    return !glibcVersionRuntime
  }
}

function getPlatformPackage() {
  switch (platform) {
    case 'darwin':
      // darwin-x64 builds are disabled until macos-13 GHA runners are
      // reliable again — see npm-build-publish.yml matrix.
      return arch === 'arm64' ? 'hyperdb-api-node-darwin-arm64' : null
    case 'linux':
      if (arch === 'arm64') return isMusl() ? 'hyperdb-api-node-linux-arm64-musl' : 'hyperdb-api-node-linux-arm64-gnu'
      return isMusl() ? 'hyperdb-api-node-linux-x64-musl' : 'hyperdb-api-node-linux-x64-gnu'
    case 'win32':
      return 'hyperdb-api-node-win32-x64-msvc'
    default:
      return null
  }
}

const platformPackage = getPlatformPackage()
if (platformPackage) {
  try { nativeBinding = require(platformPackage) } catch (_) {}
}
if (!nativeBinding) {
  const tag = `${platform === 'win32' ? 'win32' : platform === 'darwin' ? 'darwin' : 'linux'}-${arch === 'arm64' ? 'arm64' : 'x64'}`
  const candidates = platformPackage ? [platformPackage.replace('hyperdb-api-node-', ''), tag] : [tag]
  for (const t of candidates) {
    if (nativeBinding) break
    const p = join(__dirname, `hyperdb-api-node.${t}.node`)
    try { if (existsSync(p)) nativeBinding = require(p) } catch (e) { loadError = loadError || e }
  }
}
if (!nativeBinding) {
  try {
    const generic = join(__dirname, 'hyperdb-api-node.node')
    if (existsSync(generic)) nativeBinding = require(generic)
  } catch (e) { loadError = loadError || e }
}
if (!nativeBinding) {
  throw new Error(
    `Failed to load hyperdb-api-node native binding (${platform}-${arch}). ` +
      `Install a prebuilt package or run 'npm run build:debug'.` +
      (loadError ? ` Load error: ${loadError.message}` : '')
  )
}

// napi-derive cannot express Symbol hooks; wire them up here.
if (nativeBinding.QueryStream) {
  nativeBinding.QueryStream.prototype[Symbol.asyncIterator] = async function* () {
    let chunk
    while ((chunk = await this.nextChunk()) !== null) {
      for (const row of chunk) yield row
    }
  }
}

if (nativeBinding.Connection) {
  const proto = nativeBinding.Connection.prototype

  // Tagged templates rewrite to $1/$2/... and run through prepared
  // statements — same path as conn.prepare(). Zero JS-side escaping.
  const buildTemplate = (strings, values) => {
    let sql = strings[0]
    for (let i = 0; i < values.length; i++) sql += '$' + (i + 1) + strings[i + 1]
    return sql
  }
  const toPreparedParam = (v) => {
    if (v === null || v === undefined) return null
    if (v instanceof Date) {
      // Format as 'YYYY-MM-DD HH:MM:SS.fffZ' — Hyper parses this as UTC for
      // both TIMESTAMP and TIMESTAMPTZ columns. JS Date is always UTC under
      // .toISOString(), so this preserves the user's intent.
      return v.toISOString().replace('T', ' ')
    }
    if (Buffer.isBuffer(v)) {
      throw new TypeError(
        'Buffer parameters are not supported by prepared statements; stage BYTEA data via COPY.'
      )
    }
    return v
  }
  proto.sql = async function (strings, ...values) {
    const stmt = this.prepare(buildTemplate(strings, values))
    try { return await stmt.query(values.map(toPreparedParam)) } finally { await stmt.close() }
  }
  // Typed flavor: same as sql`` but returns plain objects shaped like T.
  // The generic is type-erased at runtime; values are extracted via getString.
  proto.sqlTyped = async function (strings, ...values) {
    const sqlText = buildTemplate(strings, values)
    const stmt = this.prepare(sqlText)
    try {
      const rows = await stmt.query(values.map(toPreparedParam))
      const schema = await stmt.getSchema()
      const names = schema ? schema.map(c => c.name) : []
      return rows.map((row) => {
        const obj = {}
        for (let i = 0; i < names.length; i++) obj[names[i]] = row.getString(i)
        return obj
      })
    } finally {
      await stmt.close()
    }
  }
  proto.command = async function (strings, ...values) {
    const stmt = this.prepare(buildTemplate(strings, values))
    try { return await stmt.execute(values.map(toPreparedParam)) } finally { await stmt.close() }
  }

  // Object-mode query: returns rows as plain JS objects { col: value, ... }
  // Uses getString for all columns (text representation) for simplicity.
  // Transaction helper: BEGIN, run callback, COMMIT on success / ROLLBACK on error
  proto.transaction = async function (fn) {
    await this.executeCommand('BEGIN')
    let committed = false
    try {
      const result = await fn(this)
      await this.executeCommand('COMMIT')
      committed = true
      return result
    } catch (err) {
      if (!committed) await this.executeCommand('ROLLBACK').catch(() => {})
      throw err
    }
  }

  proto.queryObjects = async function (sql) {
    const stream = await this.executeQueryStream(sql)
    try {
      const firstChunk = await stream.nextChunk()
      if (!firstChunk) return []
      const schema = stream.getSchema()
      const names = schema ? schema.map(c => c.name) : firstChunk[0] ? Array.from({ length: firstChunk[0].columnCount }, (_, i) => `col${i}`) : []
      const toObj = (row) => {
        const obj = {}
        for (let i = 0; i < names.length; i++) obj[names[i]] = row.getString(i)
        return obj
      }
      const result = firstChunk.map(toObj)
      let chunk
      while ((chunk = await stream.nextChunk()) !== null) {
        for (const row of chunk) result.push(toObj(row))
      }
      return result
    } catch (err) {
      // Cancel the background reader task if we abort mid-stream
      try { stream.cancel() } catch {}
      throw err
    }
  }

  if (typeof Symbol.asyncDispose !== 'undefined') proto[Symbol.asyncDispose] = function () { return this.close() }
  // Symbol.dispose (sync) is intentionally omitted for Connection.
  // close() is async and cannot be awaited from a synchronous dispose.
  // Use `await using conn = ...` (Symbol.asyncDispose) for reliable cleanup.
}

if (nativeBinding.HyperProcess && typeof Symbol.dispose !== 'undefined') {
  nativeBinding.HyperProcess.prototype[Symbol.dispose] = function () { this.close() }
}

function getHyperdPath() {
  if (process.env.HYPERD_PATH) return process.env.HYPERD_PATH
  const pkg = getPlatformPackage()
  if (pkg) {
    try {
      const pkgDir = dirname(require.resolve(`${pkg}/package.json`))
      const name = platform === 'win32' ? 'hyperd.exe' : 'hyperd'
      const candidate = join(pkgDir, name)
      if (existsSync(candidate)) return candidate
    } catch (_) {}
  }
  return null
}

nativeBinding.getHyperdPath = getHyperdPath

module.exports = nativeBinding
