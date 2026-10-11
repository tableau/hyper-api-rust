/**
 * Connection pool for hyperdb-api-node.
 *
 * Manages a pool of reusable database connections with configurable
 * min/max size, idle timeout, and automatic health checking.
 *
 * @module hyperdb-api-node/pool
 *
 * @example
 * ```js
 * import { ConnectionPool } from 'hyperdb-api-node/pool.mjs';
 *
 * const pool = new ConnectionPool(hyper.endpoint, 'data.hyper', {
 *   min: 2,
 *   max: 10,
 *   idleTimeoutMs: 30_000,
 * });
 *
 * // Auto acquire/release
 * const rows = await pool.query('SELECT * FROM users WHERE id = $1', [42]);
 *
 * // Manual acquire/release
 * const conn = await pool.acquire();
 * try {
 *   await conn.executeCommand('INSERT INTO logs VALUES (1, \'hello\')');
 * } finally {
 *   pool.release(conn);
 * }
 *
 * await pool.close();
 * ```
 */

import { createRequire } from 'module';
const require = createRequire(import.meta.url);
const { Connection, ConnectionBuilder, CreateMode } = require('./index.js');

/**
 * A connection pool that manages reusable Hyper database connections.
 */
export class ConnectionPool {
  #endpoint;
  #databasePath;
  #createMode;
  #tls;
  #min;
  #max;
  #idleTimeoutMs;
  #idle = [];          // { conn, lastUsed }
  #active = new Set();
  #waiting = [];       // { resolve, reject, timer } waiting for a connection
  #closed = false;
  #idleTimer = null;
  #acquireTimeoutMs;

  /**
   * Creates a new connection pool.
   *
   * Connections are created lazily on first `acquire()` call.
   *
   * @param {string} endpoint - Hyper server endpoint (e.g., "localhost:7483").
   * @param {string} databasePath - Path to the .hyper database file.
   * @param {object} [options] - Pool configuration.
   * @param {number} [options.min=0] - Minimum idle connections to maintain.
   * @param {number} [options.max=10] - Maximum total connections.
   * @param {number} [options.idleTimeoutMs=30000] - Close idle connections after this many ms.
   * @param {number} [options.acquireTimeoutMs=30000] - Max ms to wait for a connection (0 = no limit).
   * @param {string} [options.createMode='CreateIfNotExists'] - Database creation mode.
   * @param {import('./index.js').TlsOptions} [options.tls] - TLS options for every
   *   connection, as for `ConnectionBuilder.tls()`. Errors surface from `acquire()`.
   */
  constructor(endpoint, databasePath, options = {}) {
    this.#endpoint = endpoint;
    this.#databasePath = databasePath;
    this.#min = options.min ?? 0;
    this.#max = options.max ?? 10;
    this.#idleTimeoutMs = options.idleTimeoutMs ?? 30_000;
    this.#acquireTimeoutMs = options.acquireTimeoutMs ?? 30_000;
    this.#createMode = options.createMode ?? CreateMode.CreateIfNotExists;
    this.#tls = options.tls;

    if (this.#idleTimeoutMs > 0) {
      this.#idleTimer = setInterval(() => this.#evictIdle(), this.#idleTimeoutMs / 2);
      this.#idleTimer.unref(); // Don't prevent process exit
    }
  }

  /**
   * Total number of connections (idle + active).
   * @type {number}
   */
  get size() {
    return this.#idle.length + this.#active.size;
  }

  /**
   * Number of idle (available) connections.
   * @type {number}
   */
  get idle() {
    return this.#idle.length;
  }

  /**
   * Number of active (in-use) connections.
   * @type {number}
   */
  get active() {
    return this.#active.size;
  }

  /**
   * Number of callers waiting for a connection.
   * @type {number}
   */
  get pending() {
    return this.#waiting.length;
  }

  /**
   * Acquires a connection from the pool.
   *
   * If an idle connection is available, it is returned immediately.
   * If the pool is at max capacity, the call waits until a connection
   * is released. Otherwise, a new connection is created.
   *
   * @returns {Promise<import('./index.js').Connection>} A database connection.
   */
  async acquire() {
    if (this.#closed) throw new Error('Pool is closed');

    // Try to reuse an idle connection
    while (this.#idle.length > 0) {
      const { conn } = this.#idle.pop();
      try {
        if (conn.isAlive) {
          this.#active.add(conn);
          return conn;
        }
      } catch {
        // Connection is dead, discard and try next
      }
      try { await conn.close(); } catch {}
    }

    // Create a new connection if below max
    if (this.size < this.#max) {
      const conn = await this.#connect();
      this.#active.add(conn);
      return conn;
    }

    // At max capacity — wait for a release (with timeout)
    return new Promise((resolve, reject) => {
      const entry = { resolve, reject, timer: null };
      if (this.#acquireTimeoutMs > 0) {
        entry.timer = setTimeout(() => {
          if (this.#closed) {
            reject(new Error('Pool is closed'));
            return;
          }
          const idx = this.#waiting.indexOf(entry);
          if (idx !== -1) this.#waiting.splice(idx, 1);
          reject(new Error(`Pool acquire timeout after ${this.#acquireTimeoutMs}ms`));
        }, this.#acquireTimeoutMs);
      }
      this.#waiting.push(entry);
    });
  }

  /**
   * Returns a connection to the pool.
   *
   * The connection becomes available for reuse by other callers.
   *
   * @param {import('./index.js').Connection} conn - The connection to release.
   */
  release(conn) {
    if (!this.#active.has(conn)) {
      if (process.env.NODE_ENV !== 'production') {
        console.warn('hyperdb-api-node/pool: release() called on a connection not owned by this pool (possible double-release)');
      }
      return;
    }
    this.#active.delete(conn);

    if (this.#closed) {
      conn.close().catch(() => {});
      return;
    }

    // If someone is waiting, hand the connection directly to them
    if (this.#waiting.length > 0) {
      const waiter = this.#waiting.shift();
      if (waiter.timer) clearTimeout(waiter.timer);
      this.#active.add(conn);
      waiter.resolve(conn);
      return;
    }

    // Return to idle pool
    this.#idle.push({ conn, lastUsed: Date.now() });
  }

  /**
   * Executes a callback with an auto-managed connection.
   *
   * The connection is automatically acquired before and released after
   * the callback completes (or throws).
   *
   * @template T
   * @param {(conn: import('./index.js').Connection) => Promise<T>} fn - Callback receiving the connection.
   * @returns {Promise<T>} The callback's return value.
   *
   * @example
   * ```js
   * const rows = await pool.use(async (conn) => {
   *   return conn.executeQuery('SELECT * FROM users');
   * });
   * ```
   */
  async use(fn) {
    const conn = await this.acquire();
    try {
      return await fn(conn);
    } finally {
      this.release(conn);
    }
  }

  /**
   * Shorthand: execute a query using a pooled connection.
   *
   * @param {string} sql - SQL query.
   * @returns {Promise<import('./index.js').RowData[]>} Query results.
   */
  async query(sql) {
    return this.use((conn) => conn.executeQuery(sql));
  }

  /**
   * Shorthand: run a parameterized query via a server-side prepared
   * statement on a pooled connection.
   *
   * @param {string} sql - SQL with $1, $2 placeholders.
   * @param {Array} params - Parameter values (bool / number / bigint / string / null).
   * @returns {Promise<import('./index.js').RowData[]>} Query results.
   */
  async queryParams(sql, params) {
    return this.use(async (conn) => {
      const stmt = conn.prepare(sql);
      try {
        return await stmt.query(params);
      } finally {
        await stmt.close().catch(() => {});
      }
    });
  }

  /**
   * Shorthand: execute a command using a pooled connection.
   *
   * @param {string} sql - SQL command.
   * @returns {Promise<number>} Affected row count.
   */
  async command(sql) {
    return this.use((conn) => conn.executeCommand(sql));
  }

  /**
   * Closes all connections and shuts down the pool.
   *
   * After calling this, no further operations are allowed.
   */
  async close() {
    this.#closed = true;

    if (this.#idleTimer) {
      clearInterval(this.#idleTimer);
      this.#idleTimer = null;
    }

    // Reject waiting callers
    for (const waiter of this.#waiting) {
      if (waiter.timer) clearTimeout(waiter.timer);
      waiter.reject(new Error('Pool is closed'));
    }
    this.#waiting = [];

    // Close all idle connections
    const closePromises = [];
    for (const { conn } of this.#idle) {
      closePromises.push(conn.close().catch(() => {}));
    }
    this.#idle = [];

    // Close all active connections
    for (const conn of this.#active) {
      closePromises.push(conn.close().catch(() => {}));
    }
    this.#active.clear();

    await Promise.all(closePromises);
  }

  /**
   * Opens a new connection. Only a TLS pool goes through the builder:
   * `Connection.connect` ignores the create mode on a read-only gRPC
   * endpoint, while the builder rejects any but `DoNotCreate`.
   */
  #connect() {
    if (this.#tls == null) {
      return Connection.connect(this.#endpoint, this.#databasePath, this.#createMode);
    }
    return new ConnectionBuilder(this.#endpoint)
      .database(this.#databasePath)
      .createMode(this.#createMode)
      .tls(this.#tls)
      .build();
  }

  /** Evicts idle connections that have been unused longer than idleTimeoutMs. */
  #evictIdle() {
    const now = Date.now();
    const keep = [];
    const evict = [];
    for (const entry of this.#idle) {
      if (now - entry.lastUsed > this.#idleTimeoutMs) {
        evict.push(entry);
      } else {
        keep.push(entry);
      }
    }
    // Only evict excess connections — always retain at least #min idle
    while (evict.length > 0 && keep.length + evict.length > this.#min) {
      const entry = evict.pop();
      entry.conn.close().catch(() => {});
    }
    // Remaining stale connections that can't be evicted go back to keep
    keep.push(...evict);
    this.#idle = keep;
  }
}
