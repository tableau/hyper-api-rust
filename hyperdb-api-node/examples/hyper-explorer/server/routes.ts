// Copyright (c) 2026, Salesforce, Inc. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR MIT

import { Express, Request, Response } from 'express';
import { createRequire } from 'module';
import { readdir, stat } from 'fs/promises';
import { existsSync, unlinkSync } from 'fs';
import { join, resolve, dirname, sep } from 'path';
import { homedir } from 'os';
import {
  buildCreateTableSql,
  buildInsertSql,
  SUPPORTED_TYPES,
  DISTRIBUTIONS_BY_TYPE,
  type GenerateSpec,
} from './generator.js';
import { fftMagnitude } from './fft.js';
import {
  buildBasicStatsQuery,
  buildTopValuesQuery,
  buildColumnDetailQuery,
  buildHistogramQuery,
  parseStatsRow,
  isNumericType,
  isTextType,
  type ColumnMeta,
} from './profiler.js';

const require = createRequire(import.meta.url);
const { Connection, Catalog, CreateMode } = require('hyperdb-api-node');

function quoteIdent(name: string): string {
  return `"${name.replace(/"/g, '""')}"`;
}

/**
 * Project a RowData into a `{ name: stringValue }` plain object. Replaces
 * the previous `row.setColumnNames(names); row.toJSON()` dance that used to
 * live on the RowData prototype before the native binding slimmed down.
 */
function rowToObject(row: any, colNames: string[]): Record<string, string | null> {
  const obj: Record<string, string | null> = {};
  for (let i = 0; i < colNames.length; i++) {
    obj[colNames[i]] = row.isNull(i) ? null : row.getString(i);
  }
  return obj;
}

interface TrackedQuery {
  sql: string;
  rowCount: number | null;
  durationMs: number;
  source: string;
  connectionId: number;
  queryStats?: any;
}

function getQueryStats(conn: any): any | undefined {
  try { return conn.lastQueryStats() ?? undefined; } catch (_) { return undefined; }
}

function trackedConn(conn: any, queries: TrackedQuery[], source: string, connectionId: number) {
  return {
    get isAlive() { return conn.isAlive; },
    querySchema: (sql: string) => conn.querySchema(sql),
    executeQuery: async (sql: string) => {
      const start = Date.now();
      const rows = await conn.executeQuery(sql);
      const queryStats = getQueryStats(conn);
      queries.push({ sql, rowCount: rows.length, durationMs: Date.now() - start, source, connectionId, queryStats });
      return rows;
    },
    executeCommand: async (sql: string) => {
      const start = Date.now();
      const affected = await conn.executeCommand(sql);
      const queryStats = getQueryStats(conn);
      queries.push({ sql, rowCount: affected, durationMs: Date.now() - start, source, connectionId, queryStats });
      return affected;
    },
  };
}

// =============================================================================
// Connection Pool — allows concurrent requests on the same database
// =============================================================================

const MAX_POOL_SIZE = 5;

interface PoolEntry {
  idle: any[];   // connections available for use
  busy: Set<any>; // connections currently in use
}

export class ConnectionPool {
  private pools = new Map<string, PoolEntry>();
  private hyper: any;
  private nextConnId = 1;
  private connIds = new Map<any, number>();
  private hyperLogPath: string | null = null;

  constructor(hyper: any) {
    this.hyper = hyper;
    try { this.hyperLogPath = hyper.logPath ?? null; } catch (_) {}
  }

  /** Returns the unique ID assigned to a connection. */
  getConnectionId(conn: any): number {
    return this.connIds.get(conn) ?? 0;
  }

  private getOrCreateEntry(dbPath: string): PoolEntry {
    let entry = this.pools.get(dbPath);
    if (!entry) {
      entry = { idle: [], busy: new Set() };
      this.pools.set(dbPath, entry);
    }
    return entry;
  }

  async acquire(dbPath: string, createMode?: any): Promise<any> {
    const entry = this.getOrCreateEntry(dbPath);

    // Try to reuse an idle connection
    while (entry.idle.length > 0) {
      const conn = entry.idle.pop()!;
      if (conn.isAlive) {
        entry.busy.add(conn);
        return conn;
      }
      // Dead connection, discard it
    }

    // Create a new connection if under the limit
    const mode = createMode ?? CreateMode.DoNotCreate;
    const conn = await Connection.connect(this.hyper.endpoint, dbPath, mode);
    this.connIds.set(conn, this.nextConnId++);
    // Enable query stats if we know the hyperd log path
    if (this.hyperLogPath) {
      try { conn.enableQueryStats(this.hyperLogPath); } catch (_) {}
    }
    entry.busy.add(conn);
    return conn;
  }

  release(dbPath: string, conn: any): void {
    const entry = this.pools.get(dbPath);
    if (!entry) return;
    entry.busy.delete(conn);
    if (conn.isAlive && entry.idle.length < MAX_POOL_SIZE) {
      entry.idle.push(conn);
    } else {
      this.connIds.delete(conn);
      try { conn.close(); } catch (_) {}
    }
  }

  /** Remove a connection from the pool and close it unconditionally.
   *  Use after an error — the protocol state may be desynchronized even
   *  though the TCP socket is still open (`isAlive` would return true). */
  destroy(dbPath: string, conn: any): void {
    const entry = this.pools.get(dbPath);
    if (entry) entry.busy.delete(conn);
    this.connIds.delete(conn);
    try { conn.close(); } catch (_) {}
  }

  async closeAll(dbPath?: string): Promise<void> {
    const paths = dbPath ? [dbPath] : Array.from(this.pools.keys());
    for (const p of paths) {
      const entry = this.pools.get(p);
      if (!entry) continue;
      for (const conn of entry.idle) {
        this.connIds.delete(conn);
        try { conn.close(); } catch (_) {}
      }
      for (const conn of entry.busy) {
        this.connIds.delete(conn);
        try { conn.close(); } catch (_) {}
      }
      this.pools.delete(p);
    }
  }
}

export function registerRoutes(app: Express) {
  // GET /api/browse?dir=... — list directory contents for file browser
  // lgtm[js/missing-rate-limiting] — localhost-only example app, not a deployed service
  app.get('/api/browse', async (req: Request, res: Response) => {
    try {
      const dir = typeof req.query.dir === 'string' && req.query.dir
        ? resolve(req.query.dir)
        : homedir();

      const entries = await readdir(dir, { withFileTypes: true }); // lgtm[js/path-injection] — intentional: this is a local filesystem browser
      const items: { name: string; path: string; isDir: boolean; isHyper: boolean; size: number | null; lastModified: string | null }[] = [];

      // Add parent directory entry
      const parent = dirname(dir);
      if (parent !== dir) {
        items.push({ name: '..', path: parent, isDir: true, isHyper: false, size: null, lastModified: null });
      }

      for (const entry of entries) {
        if (entry.name.startsWith('.')) continue; // skip hidden files
        const fullPath = join(dir, entry.name);
        const isDir = entry.isDirectory();
        const isHyper = !isDir && entry.name.endsWith('.hyper');
        if (isDir || isHyper) {
          let size: number | null = null;
          let lastModified: string | null = null;
          try {
            const st = await stat(fullPath); // lgtm[js/path-injection]
            size = st.size;
            lastModified = st.mtime.toISOString();
          } catch {}
          items.push({ name: entry.name, path: fullPath, isDir, isHyper, size, lastModified });
        }
      }

      // Sort: directories first, then .hyper files, both alphabetical
      items.sort((a, b) => {
        if (a.name === '..') return -1;
        if (b.name === '..') return 1;
        if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
        return a.name.localeCompare(b.name);
      });

      res.json({ dir, items });
    } catch (err: any) {
      console.error('[browse]', err);
      res.status(500).json({ error: err.message });
    }
  });

  // POST /api/open — open a database and return its schema tree
  app.post('/api/open', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let rawConn: any = null;
    let dbPath: string | undefined;
    let errored = false;
    try {
      ({ path: dbPath } = req.body);
      if (!dbPath || typeof dbPath !== 'string') {
        res.status(400).json({ error: 'Missing "path" in request body' });
        return;
      }

      rawConn = await pool.acquire(dbPath);
      const connId = pool.getConnectionId(rawConn);
      const queries: TrackedQuery[] = [];
      const conn = trackedConn(rawConn, queries, 'open', connId);
      const catalog = new Catalog(rawConn);
      const schemas = await catalog.getSchemaNames();

      const tree: {
        schema: string;
        tables: {
          name: string;
          columns: { name: string; typeName: string }[];
        }[];
      }[] = [];

      for (const schema of schemas) {
        const tableNames = await catalog.getTableNames(schema);
        const tables: typeof tree[number]['tables'] = [];

        for (const tableName of tableNames) {
          const fqn = `${quoteIdent(schema)}.${quoteIdent(tableName)}`;
          const colInfos = await rawConn.querySchema(`SELECT * FROM ${fqn} LIMIT 0`);
          tables.push({
            name: tableName,
            columns: colInfos.map((c: any) => ({
              name: c.name,
              typeName: c.typeName,
            })),
          });
        }

        tree.push({ schema, tables });
      }

      res.json({ database: dbPath, schemas: tree, _queries: queries });
    } catch (err: any) {
      errored = true;
      console.error('[open]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (rawConn && dbPath) {
        if (errored) pool.destroy(dbPath, rawConn);
        else pool.release(dbPath, rawConn);
      }
    }
  });

  // POST /api/close — close all pooled connections for a database
  app.post('/api/close', async (req: Request, res: Response) => {
    try {
      const { path: dbPath } = req.body;
      const pool: ConnectionPool = app.locals.pool;
      await pool.closeAll(dbPath);
      res.json({ ok: true });
    } catch (err: any) {
      res.status(500).json({ error: err.message });
    }
  });

  // GET /api/preview/:schema/:table?limit=100&offset=0&db=...
  app.get('/api/preview/:schema/:table', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let rawConn: any = null;
    const dbPath = req.query.db as string;
    let errored = false;
    try {
      if (!dbPath) { res.status(400).json({ error: 'Missing db query param' }); return; }

      rawConn = await pool.acquire(dbPath);
      const connId = pool.getConnectionId(rawConn);
      const queries: TrackedQuery[] = [];
      const conn = trackedConn(rawConn, queries, 'preview', connId);
      const { schema, table } = req.params;
      const limit = Math.min(Number(req.query.limit) || 100, 1000);
      const offset = Math.max(Number(req.query.offset) || 0, 0);

      const fqn = `${quoteIdent(schema)}.${quoteIdent(table)}`;
      const colInfos = await rawConn.querySchema(`SELECT * FROM ${fqn} LIMIT 0`);
      const colNames = colInfos.map((c: any) => c.name);

      // Skip the COUNT(*) pass on follow-up pages — the client passes
      // ?withCount=1 on the initial load only. On paging requests we
      // page past the end gracefully (the LIMIT/OFFSET simply returns
      // fewer rows), which is much cheaper than scanning the full
      // table for every page turn.
      const wantCount = req.query.withCount === '1' || req.query.withCount === 'true';
      let totalRowCount: number | null = null;
      if (wantCount) {
        const countRows = await conn.executeQuery(`SELECT COUNT(*) AS cnt FROM ${fqn}`);
        totalRowCount = Number(countRows[0].getInt64(0));
      }

      // Build ORDER BY clause if sort params provided
      const sortColumn = req.query.sortColumn as string | undefined;
      const sortDir = (req.query.sortDir as string | undefined)?.toUpperCase() === 'DESC' ? 'DESC' : 'ASC';
      let orderClause = '';
      if (sortColumn && colNames.includes(sortColumn)) {
        orderClause = ` ORDER BY ${quoteIdent(sortColumn)} ${sortDir}`;
      }

      const rows = await conn.executeQuery(`SELECT * FROM ${fqn}${orderClause} LIMIT ${limit} OFFSET ${offset}`);
      const data = rows.map((row: any) => rowToObject(row, colNames));

      res.json({
        columns: colInfos.map((c: any) => ({ name: c.name, typeName: c.typeName })),
        rows: data,
        rowCount: data.length,
        totalRowCount,
        offset,
        _queries: queries,
      });
    } catch (err: any) {
      errored = true;
      console.error('[preview]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (rawConn && dbPath) {
        if (errored) pool.destroy(dbPath, rawConn);
        else pool.release(dbPath, rawConn);
      }
    }
  });

  // GET /api/stats/:schema/:table?db=...
  app.get('/api/stats/:schema/:table', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let rawConn: any = null;
    const dbPath = req.query.db as string;
    let errored = false;
    try {
      if (!dbPath) { res.status(400).json({ error: 'Missing db query param' }); return; }

      rawConn = await pool.acquire(dbPath);
      const connId = pool.getConnectionId(rawConn);
      const queries: TrackedQuery[] = [];
      const conn = trackedConn(rawConn, queries, 'stats', connId);
      const { schema, table } = req.params;

      const fqn = `${quoteIdent(schema)}.${quoteIdent(table)}`;
      const colInfos = await rawConn.querySchema(`SELECT * FROM ${fqn} LIMIT 0`);
      const columns: ColumnMeta[] = colInfos.map((c: any) => ({
        name: c.name,
        typeName: c.typeName,
        index: c.index,
      }));

      // Run basic stats query
      const sql = buildBasicStatsQuery(schema, table, columns);
      const statsRows = await conn.executeQuery(sql);
      queries.push({ sql, rowCount: statsRows.length, durationMs: 0, source: 'stats', connectionId: connId });
      if (statsRows.length === 0) {
        res.json({ stats: [], _queries: queries });
        return;
      }

      // Convert the single stats row to a plain object
      const colNames = await rawConn.querySchema(sql);
      const names = colNames.map((c: any) => c.name);
      const rawRow = rowToObject(statsRows[0], names);

      const stats = parseStatsRow(rawRow, columns);

      // Fetch top values for text columns
      for (const stat of stats) {
        const typeName = stat.typeName.toUpperCase();
        if (typeName.includes('TEXT') || typeName.includes('VARCHAR') || typeName.includes('CHAR')) {
          try {
            const tvSql = buildTopValuesQuery(schema, table, stat.name, 5);
            const tvRows = await conn.executeQuery(tvSql);
            queries.push({ sql: tvSql, rowCount: tvRows.length, durationMs: 0, source: 'stats', connectionId: connId });
            const tvSchema = await rawConn.querySchema(tvSql);
            const tvNames = tvSchema.map((c: any) => c.name);
            stat.topValues = tvRows.map((r: any) => {
              const obj = rowToObject(r, tvNames);
              return { value: obj.val ?? '', count: Number(obj.cnt ?? 0) };
            });
          } catch (_) {
            stat.topValues = [];
          }
        }
      }

      res.json({ stats, _queries: queries });
    } catch (err: any) {
      errored = true;
      console.error('[stats]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (rawConn && dbPath) {
        if (errored) pool.destroy(dbPath, rawConn);
        else pool.release(dbPath, rawConn);
      }
    }
  });

  // GET /api/column-detail/:schema/:table/:column?db=...
  app.get('/api/column-detail/:schema/:table/:column', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let rawConn: any = null;
    const dbPath = req.query.db as string;
    let errored = false;
    try {
      if (!dbPath) { res.status(400).json({ error: 'Missing db query param' }); return; }

      rawConn = await pool.acquire(dbPath);
      const connId = pool.getConnectionId(rawConn);
      const queries: TrackedQuery[] = [];
      const conn = trackedConn(rawConn, queries, 'column-detail', connId);
      const { schema, table, column } = req.params;

      // Get column type
      const fqn = `${quoteIdent(schema)}.${quoteIdent(table)}`;
      const colInfos = await rawConn.querySchema(`SELECT * FROM ${fqn} LIMIT 0`);
      const colMeta = colInfos.find((c: any) => c.name === column);
      if (!colMeta) {
        res.status(404).json({ error: `Column "${column}" not found` });
        return;
      }
      const typeName: string = colMeta.typeName;

      // Run detail stats query
      const sql = buildColumnDetailQuery(schema, table, column, typeName);
      const rows = await conn.executeQuery(sql);
      if (rows.length === 0) {
        res.json({ detail: null, _queries: queries });
        return;
      }
      const schemaInfo = await rawConn.querySchema(sql);
      const names = schemaInfo.map((c: any) => c.name);
      const raw = rowToObject(rows[0], names);

      const rowCount = Number(raw.rowCount ?? 0);
      const nullCount = Number(raw.nullCount ?? 0);
      const distinctCount = Number(raw.distinctCount ?? 0);

      const detail: any = {
        name: column,
        typeName,
        rowCount,
        nullCount,
        nullPercent: rowCount > 0 ? Math.round((nullCount / rowCount) * 10000) / 100 : 0,
        distinctCount,
        cardinality: rowCount > 0 ? Math.round((distinctCount / rowCount) * 10000) / 100 : 0,
      };

      if (isNumericType(typeName)) {
        detail.min = raw.min != null ? Number(raw.min) : null;
        detail.max = raw.max != null ? Number(raw.max) : null;
        detail.mean = raw.mean != null ? Number(raw.mean) : null;
        detail.stddev = raw.stddev != null ? Number(raw.stddev) : null;
        detail.cv = (detail.mean != null && detail.mean !== 0 && detail.stddev != null)
          ? Math.round((detail.stddev / Math.abs(detail.mean)) * 10000) / 100
          : null;
        detail.variance = raw.variance != null ? Number(raw.variance) : null;
        detail.sum = raw.sum != null ? Number(raw.sum) : null;
        detail.median = raw.median != null ? Number(raw.median) : null;
        detail.p10 = raw.p10 != null ? Number(raw.p10) : null;
        detail.p25 = raw.p25 != null ? Number(raw.p25) : null;
        detail.p75 = raw.p75 != null ? Number(raw.p75) : null;
        detail.p90 = raw.p90 != null ? Number(raw.p90) : null;

        // Histogram
        if (detail.min != null && detail.max != null) {
          try {
            const histSql = buildHistogramQuery(schema, table, column, detail.min, detail.max, 200);
            const histRows = await conn.executeQuery(histSql);
            const histSchema = await rawConn.querySchema(histSql);
            const histNames = histSchema.map((c: any) => c.name);
            detail.histogram = histRows.map((r: any) => {
              const obj = rowToObject(r, histNames);
              return {
                lo: Number(obj.bucket_lo ?? 0),
                hi: Number(obj.bucket_hi ?? 0),
                count: Number(obj.cnt ?? 0),
              };
            });
          } catch (e) {
            detail.histogram = [];
          }
        }

        // FFT of histogram distribution shape
        if (detail.histogram && detail.histogram.length > 1) {
          try {
            const counts = detail.histogram.map((b: any) => b.count);
            const magnitudes = fftMagnitude(counts);
            // Normalize to 0-1 range for display
            const maxMag = Math.max(...magnitudes, 1e-10);
            detail.fft = magnitudes.map((m: number, i: number) => ({
              frequency: i + 1,
              magnitude: m / maxMag,
              raw: m,
            }));
          } catch (_) {
            detail.fft = [];
          }
        }

      } else if (isTextType(typeName)) {
        detail.minLength = raw.minLength != null ? Number(raw.minLength) : null;
        detail.maxLength = raw.maxLength != null ? Number(raw.maxLength) : null;
        detail.avgLength = raw.avgLength != null ? Number(raw.avgLength) : null;
      } else if (typeName.toUpperCase() === 'BOOL' || typeName.toUpperCase() === 'BOOLEAN') {
        detail.trueCount = Number(raw.trueCount ?? 0);
        detail.falseCount = Number(raw.falseCount ?? 0);
        const nonNull = rowCount - nullCount;
        detail.truePercent = nonNull > 0 ? Math.round((detail.trueCount / nonNull) * 10000) / 100 : 0;
      } else if (typeName.toUpperCase().includes('DATE') || typeName.toUpperCase().includes('TIMESTAMP')) {
        detail.min = raw.min ?? null;
        detail.max = raw.max ?? null;
      }

      // Top values for all types (top 10)
      try {
        const tvSql = buildTopValuesQuery(schema, table, column, 10);
        const tvRows = await conn.executeQuery(tvSql);
        const tvSchema = await rawConn.querySchema(tvSql);
        const tvNames = tvSchema.map((c: any) => c.name);
        detail.topValues = tvRows.map((r: any) => {
          const obj = rowToObject(r, tvNames);
          return { value: obj.val ?? '', count: Number(obj.cnt ?? 0) };
        });
      } catch (_) {
        detail.topValues = [];
      }

      res.json({ detail, _queries: queries });
    } catch (err: any) {
      errored = true;
      console.error('[column-detail]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (rawConn && dbPath) {
        if (errored) pool.destroy(dbPath, rawConn);
        else pool.release(dbPath, rawConn);
      }
    }
  });

  // POST /api/query — execute ad-hoc SQL
  app.post('/api/query', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let rawConn: any = null;
    let dbPath: string | undefined;
    let errored = false;
    try {
      const { db, sql } = req.body;
      dbPath = db;
      if (!dbPath || !sql) {
        res.status(400).json({ error: 'Missing "db" or "sql" in request body' });
        return;
      }

      rawConn = await pool.acquire(dbPath);
      const trimmed = sql.trim().toUpperCase();
      const isSelect = trimmed.startsWith('SELECT') || trimmed.startsWith('WITH') || trimmed.startsWith('EXPLAIN');

      const start = Date.now();

      if (isSelect) {
        const colInfos = await rawConn.querySchema(sql);
        const colNames = colInfos.map((c: any) => c.name);
        const rows = await rawConn.executeQuery(sql);
        const queryStats = getQueryStats(rawConn);
        const data = rows.map((row: any) => rowToObject(row, colNames));
        const durationMs = Date.now() - start;

        res.json({
          type: 'query',
          columns: colInfos.map((c: any) => ({ name: c.name, typeName: c.typeName })),
          rows: data,
          rowCount: data.length,
          durationMs,
          queryStats,
        });
      } else {
        const affected = await rawConn.executeCommand(sql);
        const queryStats = getQueryStats(rawConn);
        const durationMs = Date.now() - start;
        res.json({
          type: 'command',
          rowsAffected: affected,
          durationMs,
          queryStats,
        });
      }
    } catch (err: any) {
      errored = true;
      console.error('[query]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (rawConn && dbPath) {
        if (errored) pool.destroy(dbPath, rawConn);
        else pool.release(dbPath, rawConn);
      }
    }
  });

  // GET /api/generate-meta — return supported types and distributions
  app.get('/api/generate-meta', (_req: Request, res: Response) => {
    res.json({
      types: SUPPORTED_TYPES,
      distributionsByType: DISTRIBUTIONS_BY_TYPE,
    });
  });

  // POST /api/generate — create a new .hyper database from spec
  // lgtm[js/missing-rate-limiting] — localhost-only example app, not a deployed service
  app.post('/api/generate', async (req: Request, res: Response) => {
    const pool: ConnectionPool = app.locals.pool;
    let conn: any = null;
    let dbPath: string | undefined;
    let errored = false;
    try {
      const spec: GenerateSpec = req.body;
      if (!spec.dbPath || !spec.tables || spec.tables.length === 0) {
        res.status(400).json({ error: 'Missing dbPath or tables' });
        return;
      }

      // Ensure path ends with .hyper
      dbPath = spec.dbPath;
      if (!dbPath.endsWith('.hyper')) dbPath += '.hyper';

      // Remove existing file if present — also drain any stale pooled connections
      if (existsSync(dbPath)) { // lgtm[js/path-injection] — intentional: user picks the output path in this local tool
        await pool.closeAll(dbPath);
        unlinkSync(dbPath); // lgtm[js/path-injection]
      }

      conn = await pool.acquire(dbPath, CreateMode.CreateIfNotExists);

      const results: { table: string; rowCount: number; durationMs: number }[] = [];

      for (const table of spec.tables) {
        // Create table
        const createSql = buildCreateTableSql(table);
        await conn.executeCommand(createSql);

        // Insert data
        const insertSql = buildInsertSql(table);
        const start = Date.now();
        await conn.executeCommand(insertSql);
        const durationMs = Date.now() - start;

        // Verify count
        const rows = await conn.executeQuery(`SELECT COUNT(*) FROM ${quoteIdent(table.name)}`);
        const count = Number(rows[0].getInt64(0));
        results.push({ table: table.name, rowCount: count, durationMs });

        console.log(`[generate] ${table.name}: ${count.toLocaleString()} rows (${(durationMs / 1000).toFixed(1)}s)`);
      }

      res.json({ dbPath, results });
    } catch (err: any) {
      errored = true;
      console.error('[generate]', err);
      res.status(500).json({ error: err.message });
    } finally {
      if (conn && dbPath) {
        if (errored) pool.destroy(dbPath, conn);
        else pool.release(dbPath, conn);
      }
    }
  });
}
