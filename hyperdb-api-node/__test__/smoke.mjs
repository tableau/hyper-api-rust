/**
 * Smoke test for hyperdb-api-node.
 *
 * Requires a running Hyper server or HYPERD_PATH set.
 * Run with: node __test__/smoke.mjs
 */

import { strict as assert } from 'assert';
import { createRequire } from 'module';
import { mkdirSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
const require = createRequire(import.meta.url);

// Ensure test_results directory exists
const __dirname = dirname(fileURLToPath(import.meta.url));
const TEST_DIR = join(__dirname, '..', 'test_results');
mkdirSync(TEST_DIR, { recursive: true });

// Load the native addon (after building with `npm run build:debug`)
let mod;
try {
  mod = require('../index.js');
} catch (e) {
  console.error('Failed to load native addon. Did you run `npm run build:debug`?');
  console.error(e.message);
  process.exit(1);
}

const {
  HyperProcess,
  Connection,
  ConnectionBuilder,
  CreateMode,
  Catalog,
  TableDefinition,
  SqlType,
  RowInserter,
  RowData,
} = mod;

async function main() {
  console.log('=== hyperdb-api-node smoke test ===\n');

  // 1. Start Hyper server
  console.log('1. Starting HyperProcess...');
  const hyper = new HyperProcess();
  console.log(`   Endpoint: ${hyper.endpoint}`);
  console.log(`   Is open: ${hyper.isOpen}`);

  // 2. Connect to a database
  console.log('\n2. Connecting to database...');
  const conn = await Connection.connect(
    hyper.endpoint,
    join(TEST_DIR, 'smoke_test.hyper'),
    CreateMode.CreateIfNotExists
  );
  console.log(`   Database: ${conn.database}`);
  console.log(`   Is alive: ${conn.isAlive}`);

  // 3. Create a table
  console.log('\n3. Creating table...');
  const tableDef = new TableDefinition('test_users');
  tableDef.addColumn('id', SqlType.int(), false);
  tableDef.addColumn('name', SqlType.text(), true);
  tableDef.addColumn('score', SqlType.double(), true);
  tableDef.addColumn('active', SqlType.bool(), true);

  console.log(`   Table: ${tableDef.name}`);
  console.log(`   Columns: ${tableDef.columnCount}`);
  console.log(`   SQL: ${tableDef.toCreateSql()}`);

  const catalog = new Catalog(conn);
  await catalog.dropTableIfExists('test_users');
  await catalog.createTable(tableDef);
  console.log('   Table created.');

  // 4. Check catalog
  console.log('\n4. Catalog operations...');
  const hasTable = await catalog.hasTable('test_users');
  console.log(`   Has table 'test_users': ${hasTable}`);
  const tables = await catalog.getTableNames('public');
  console.log(`   Tables in 'public': ${JSON.stringify(tables)}`);

  // 5. Insert data
  console.log('\n5. Inserting data...');
  const inserter = new RowInserter(conn, tableDef);
  inserter.addRow([1, 'Alice', 95.5, true]);
  inserter.addRow([2, 'Bob', 88.0, true]);
  inserter.addRow([3, 'Charlie', null, false]);
  console.log(`   Buffered rows: ${inserter.bufferedRowCount}`);

  const insertCount = await inserter.execute();
  console.log(`   Inserted: ${insertCount} rows`);

  // Insert more with addRows
  inserter.addRows([
    [4, 'Diana', 92.3, true],
    [5, 'Eve', 77.1, false],
  ]);
  const insertCount2 = await inserter.execute();
  console.log(`   Inserted: ${insertCount2} more rows`);

  // 6. Query data
  console.log('\n6. Querying data...');
  const rows = await conn.executeQuery('SELECT * FROM test_users ORDER BY id');
  console.log(`   Got ${rows.length} rows:`);
  for (const row of rows) {
    const id = row.getInt32(0);
    const name = row.getString(1);
    const score = row.getFloat64(2);
    const active = row.getBool(3);
    console.log(`   id=${id}, name=${name}, score=${score}, active=${active}`);
  }

  // 7. Test executeCommand with DML
  console.log('\n7. Execute command (DML)...');
  const affected = await conn.executeCommand(
    "UPDATE test_users SET score = 100.0 WHERE name = 'Alice'"
  );
  console.log(`   Rows affected: ${affected}`);

  // Verify update
  const updated = await conn.executeQuery(
    "SELECT score FROM test_users WHERE name = 'Alice'"
  );
  console.log(`   Alice's new score: ${updated[0].getFloat64(0)}`);

  // 8. Query schema
  console.log('\n8. Query schema...');
  const schema = await conn.querySchema('SELECT * FROM test_users');
  for (const col of schema) {
    console.log(`   Column: ${col.name} (${col.typeName}) [index=${col.index}]`);
  }

  // 9. Null handling
  console.log('\n9. Null handling...');
  const nullRow = await conn.executeQuery(
    "SELECT * FROM test_users WHERE name = 'Charlie'"
  );
  if (nullRow.length > 0) {
    console.log(`   Charlie score isNull: ${nullRow[0].isNull(2)}`);
    console.log(`   Charlie score value: ${nullRow[0].getFloat64(2)}`);
  }

  // 10. QueryStream
  console.log('\n10. QueryStream...');
  const stream = conn.executeQueryStream('SELECT * FROM test_users ORDER BY id');
  let streamCount = 0;
  for await (const row of stream) {
    streamCount++;
  }
  console.log(`   Streamed ${streamCount} rows via for-await-of`);

  // 11. ConnectionBuilder
  console.log('\n11. ConnectionBuilder...');
  const conn2 = await new ConnectionBuilder(hyper.endpoint)
    .database(join(TEST_DIR, 'builder_test.hyper'))
    .createMode(CreateMode.CreateIfNotExists)
    .loginTimeout(5000)
    .build();
  console.log(`   Connected via builder, db: ${conn2.database}`);
  await conn2.executeCommand('CREATE TABLE IF NOT EXISTS t (id INT)');
  await conn2.close();

  // 12. Parameterized queries via PreparedStatement (server-side binding)
  console.log('\n12. Parameterized queries...');
  {
    const stmt = await conn.prepare(
      'SELECT * FROM test_users WHERE name = $1 AND score > $2'
    );
    const paramRows = await stmt.query(['Diana', 90]);
    console.log(`   Query with params: ${paramRows.length} row(s)`);
    assert(paramRows.length === 1, `Expected 1 row, got ${paramRows.length}`);
    assert(paramRows[0].getString(1) === 'Diana', 'Expected Diana');
    await stmt.close();
  }

  {
    const update = await conn.prepare(
      'UPDATE test_users SET score = $1 WHERE name = $2'
    );
    await update.execute([99.9, 'Bob']);
    await update.close();
  }

  {
    const lookup = await conn.prepare(
      'SELECT score FROM test_users WHERE name = $1'
    );
    const bobRow = await lookup.query(['Bob']);
    console.log(`   Bob's updated score: ${bobRow[0].getFloat64(0)}`);
    assert(bobRow[0].getFloat64(0) === 99.9, 'Expected 99.9');

    // SQL injection prevention — parameter is bound as a TEXT literal,
    // never parsed as SQL.
    const injectionRows = await lookup.query(["'; DROP TABLE test_users; --"]);
    assert(injectionRows.length === 0, 'Injection should return 0 rows');
    console.log('   SQL injection safely escaped ✓');
    await lookup.close();
  }

  // 13. Connection pool
  console.log('\n13. Connection pool...');
  const { ConnectionPool } = await import('../pool.mjs');
  const pool = new ConnectionPool(hyper.endpoint, join(TEST_DIR, 'smoke_test.hyper'), {
    min: 1,
    max: 3,
    idleTimeoutMs: 5000,
    createMode: CreateMode.DoNotCreate,
  });

  // pool.query shorthand
  const poolRows = await pool.query('SELECT COUNT(*) FROM test_users');
  console.log(`   Pool query: ${poolRows[0].getInt32(0)} users`);

  // pool.use with auto acquire/release
  const result = await pool.use(async (c) => {
    const r = await c.executeQuery('SELECT name FROM test_users ORDER BY id LIMIT 1');
    return r[0].getString(0);
  });
  console.log(`   Pool use: first user = ${result}`);

  // Concurrent pool usage
  const concurrent = await Promise.all([
    pool.query('SELECT 1'),
    pool.query('SELECT 2'),
    pool.query('SELECT 3'),
  ]);
  console.log(`   Concurrent queries: ${concurrent.length} completed`);
  console.log(`   Pool stats: size=${pool.size}, idle=${pool.idle}, active=${pool.active}`);

  await pool.close();
  console.log('   Pool closed ✓');

  // 14. Tagged template literals
  console.log('\n14. Tagged template literals...');
  const name = 'Diana';
  const minScore = 90;
  const taggedRows = await conn.sql`SELECT * FROM test_users WHERE name = ${name} AND score > ${minScore}`;
  console.log(`   conn.sql\`: ${taggedRows.length} row(s)`);
  assert.equal(taggedRows.length, 1);

  await conn.command`UPDATE test_users SET score = ${77.7} WHERE name = ${'Eve'}`;
  const eveRow = await conn.sql`SELECT score FROM test_users WHERE name = ${'Eve'}`;
  console.log(`   conn.command\`: Eve score = ${eveRow[0].getFloat64(0)}`);
  assert.equal(eveRow[0].getFloat64(0), 77.7);

  // 15. BigInt support
  console.log('\n15. BigInt support...');
  await conn.executeCommand('CREATE TABLE bigint_test (id BIGINT NOT NULL)');
  await conn.executeCommand("INSERT INTO bigint_test VALUES (9007199254740993)");
  const bigRows = await conn.executeQuery('SELECT id FROM bigint_test');
  const bigVal = bigRows[0].getBigInt(0);
  console.log(`   getBigInt: ${bigVal} (type: ${typeof bigVal})`);
  assert.equal(typeof bigVal, 'bigint');
  assert.equal(bigVal, 9007199254740993n);
  await conn.executeCommand('DROP TABLE bigint_test');

  // 16. Row JSON projection (user-land — native binding keeps RowData minimal)
  console.log('\n16. Row JSON projection...');
  const jsonRows = await conn.executeQuery('SELECT id, name, score FROM test_users ORDER BY id LIMIT 2');
  const jsonSchema = await conn.querySchema('SELECT id, name, score FROM test_users');
  const plain = Object.fromEntries(
    jsonSchema.map((c, i) => [c.name, jsonRows[0].isNull(i) ? null : jsonRows[0].getString(i)])
  );
  console.log(`   projection: ${JSON.stringify(plain)}`);
  assert(plain.id !== undefined && plain.name !== undefined);

  // 17. Date/Timestamp/JSON types
  console.log('\n17. Date/Timestamp/JSON types...');
  await conn.executeCommand('DROP TABLE IF EXISTS type_test');
  await conn.executeCommand(
    'CREATE TABLE type_test (d DATE, ts TIMESTAMP, j TEXT)'
  );
  // Insert a date and timestamp via tagged template with JS Date
  const testDate = new Date('2024-06-15T14:30:00.000Z');
  await conn.command`INSERT INTO type_test VALUES (DATE '2024-06-15', TIMESTAMP '2024-06-15 14:30:00', '{"key":"val"}')`;
  const typeRows = await conn.executeQuery('SELECT * FROM type_test');

  // getDateMs → JS Date
  const dateMs = typeRows[0].getDateMs(0);
  const jsDate = new Date(dateMs);
  console.log(`   getDateMs → new Date(): ${jsDate.toISOString().split('T')[0]}`);
  assert.equal(jsDate.toISOString().split('T')[0], '2024-06-15');

  // getTimestampMs → JS Date
  const tsMs = typeRows[0].getTimestampMs(1);
  const jsTs = new Date(tsMs);
  console.log(`   getTimestampMs → new Date(): ${jsTs.toISOString()}`);
  assert(jsTs.getUTCFullYear() === 2024);
  assert(jsTs.getUTCMonth() === 5); // June = 5

  // getString still works (backward compat)
  const dateStr = typeRows[0].getString(0);
  console.log(`   getString(date): ${dateStr}`);
  assert(dateStr.includes('2024'));

  // getJSON
  const jsonStr = typeRows[0].getJson(2);
  console.log(`   getJSON: ${jsonStr}`);
  const parsed = JSON.parse(jsonStr);
  assert.equal(parsed.key, 'val');

  // Insert JS Date via tagged template
  const insertDate = new Date('2025-01-01T00:00:00Z');
  await conn.command`INSERT INTO type_test VALUES (DATE '2025-01-01', ${insertDate}, '{}')`;
  const dateRows = await conn.executeQuery('SELECT ts FROM type_test ORDER BY ts DESC LIMIT 1');
  const insertedMs = dateRows[0].getTimestampMs(0);
  const insertedDate = new Date(insertedMs);
  console.log(`   Insert JS Date via template: ${insertedDate.toISOString()}`);
  assert(insertedDate.getUTCFullYear() === 2025);

  await conn.executeCommand('DROP TABLE type_test');
  console.log('   All type tests passed ✓');

  // 18. ArrowInserter — raw Arrow IPC stream path
  console.log('\n18. ArrowInserter...');
  const { ArrowInserter } = mod;
  const { tableFromArrays, tableToIPC } = await import('apache-arrow');

  await catalog.dropTableIfExists('arrow_t');
  const arrowTableDef = new TableDefinition('arrow_t');
  arrowTableDef.addColumn('id', SqlType.int(), false);
  arrowTableDef.addColumn('v', SqlType.double(), true);
  await catalog.createTable(arrowTableDef);

  const arrowTable = tableFromArrays({
    id: Int32Array.from([10, 20, 30]),
    v: Float64Array.from([1.5, 2.5, 3.5]),
  });

  const arrowInserter = ArrowInserter.create(conn, arrowTableDef);
  await arrowInserter.insertRaw(Buffer.from(tableToIPC(arrowTable, 'stream')));
  const arrowCount = await arrowInserter.execute();
  console.log(`   ArrowInserter rows inserted: ${arrowCount}`);

  const verify = await conn.executeQuery('SELECT COUNT(*) FROM arrow_t');
  console.log(`   Verified COUNT(*): ${verify[0].getBigInt(0)}`);

  // 18b. NUMERIC decoding — sign, scale, precision (row-wise + columnar)
  console.log('\n18b. NUMERIC types...');
  await conn.executeCommand('DROP TABLE IF EXISTS numeric_test');
  await conn.executeCommand(
    'CREATE TABLE numeric_test (id INT NOT NULL, n NUMERIC(20,4), hp NUMERIC(38,12))'
  );
  await conn.executeCommand(
    `INSERT INTO numeric_test VALUES
       (1, 123.4500, 0),
       (2, -67.8900, 0),
       (3, -0.5000, 0),
       (4, -0.9990, 0),
       (5, 0.0000, 123456789.123456789012)`
  );

  const numRows = await conn.executeQuery(
    'SELECT id, n, hp FROM numeric_test ORDER BY id'
  );

  // Exact decimal text via getString (preserves scale + sign).
  const nStrings = numRows.map((r) => r.getString(1));
  console.log(`   getString(n): ${JSON.stringify(nStrings)}`);
  assert.equal(nStrings[0], '123.4500', 'positive numeric exact text');
  assert.equal(nStrings[1], '-67.8900', 'negative numeric exact text');
  // Regression: sub-unit negatives must keep their sign (issue #84).
  assert.equal(nStrings[2], '-0.5000', 'sub-unit negative keeps sign');
  assert.equal(nStrings[3], '-0.9990', 'sub-unit negative keeps sign');

  // getFloat64 must be the real value, not a reinterpreted-bytes garbage/NaN.
  const nFloats = numRows.map((r) => r.getFloat64(1));
  console.log(`   getFloat64(n): ${JSON.stringify(nFloats)}`);
  assert.ok(Math.abs(nFloats[0] - 123.45) < 1e-9, 'positive f64');
  assert.ok(Math.abs(nFloats[1] - -67.89) < 1e-9, 'negative f64');
  assert.ok(Math.abs(nFloats[2] - -0.5) < 1e-12, 'sub-unit negative f64 keeps sign');
  assert.ok(nFloats[2] < 0, 'sub-unit negative f64 is negative');
  assert.ok(nFloats.every((v) => !Number.isNaN(v)), 'no NaN from NUMERIC decode');

  // High precision (>15 significant digits): exact via string, lossy via f64.
  const hp = numRows[4].getString(2);
  console.log(`   getString(hp): ${hp}`);
  assert.equal(hp, '123456789.123456789012', 'high-precision exact text');

  // AVG over a numeric column returns a numeric — decode it correctly.
  const avgRow = await conn.executeQuery(
    'SELECT AVG(n) FROM numeric_test WHERE id <= 4'
  );
  const avg = avgRow[0].getFloat64(0);
  console.log(`   AVG(n) where id<=4: ${avg}`);
  // (123.45 - 67.89 - 0.5 - 0.999) / 4 = 13.51525
  assert.ok(Math.abs(avg - 13.51525) < 1e-6, 'AVG numeric decodes correctly');

  // Columnar path surfaces NUMERIC as f64 — must match row-wise, not garbage.
  const numColStream = conn.executeQueryColumnar(
    'SELECT id, n FROM numeric_test ORDER BY id'
  );
  const numChunk = await numColStream.nextChunk();
  const nCol = numChunk.getFloat64Column(1);
  console.log(`   columnar getFloat64Column(n): [${Array.from(nCol).join(', ')}]`);
  assert.ok(Math.abs(nCol[0] - 123.45) < 1e-9, 'columnar positive');
  assert.ok(Math.abs(nCol[2] - -0.5) < 1e-12, 'columnar sub-unit negative keeps sign');
  assert.ok(Array.from(nCol).every((v) => !Number.isNaN(v)), 'columnar no NaN');

  // NUMERIC(p, 0) integer-shaped paths: getInt32 / getInt64 / getBigInt.
  // Use a separate table so we can exercise scale=0 specifically.
  await conn.executeCommand('DROP TABLE IF EXISTS numeric_int_test');
  await conn.executeCommand(
    'CREATE TABLE numeric_int_test (id INT NOT NULL, big NUMERIC(38,0))'
  );
  // Includes a value > 2^53 to exercise the BigInt path's precision
  // preservation (i64::MAX = 9223372036854775807 > Number.MAX_SAFE_INTEGER).
  await conn.executeCommand(
    `INSERT INTO numeric_int_test VALUES
       (1,  42),
       (2, -42),
       (3, 9223372036854775807)`
  );

  const intRows = await conn.executeQuery(
    'SELECT id, big FROM numeric_int_test ORDER BY id'
  );

  // getInt32 / getInt64 narrow through f64 — fine for small values.
  assert.equal(intRows[0].getInt32(1), 42, 'NUMERIC(p,0) -> Int32 positive');
  assert.equal(intRows[1].getInt32(1), -42, 'NUMERIC(p,0) -> Int32 negative');
  assert.equal(intRows[0].getInt64(1), 42, 'NUMERIC(p,0) -> Int64 positive');
  assert.equal(intRows[1].getInt64(1), -42, 'NUMERIC(p,0) -> Int64 negative');

  // getBigInt preserves full precision for NUMERIC(p, 0); a value above
  // Number.MAX_SAFE_INTEGER must round-trip exactly.
  const bigSmall = intRows[0].getBigInt(1);
  const bigNeg = intRows[1].getBigInt(1);
  const bigLarge = intRows[2].getBigInt(1);
  console.log(
    `   getBigInt(NUMERIC(38,0)): ${bigSmall}, ${bigNeg}, ${bigLarge}`
  );
  assert.equal(bigSmall, 42n, 'BigInt small positive');
  assert.equal(bigNeg, -42n, 'BigInt small negative');
  assert.equal(
    bigLarge,
    9223372036854775807n,
    'BigInt preserves precision above 2^53'
  );

  // getBigInt on a non-zero-scale NUMERIC must return null (the cell is
  // not an integer; callers should use getString or getFloat64).
  const decBigInt = numRows[0].getBigInt(1);
  assert.equal(
    decBigInt,
    null,
    'getBigInt on NUMERIC(p, scale>0) returns null'
  );

  await conn.executeCommand('DROP TABLE numeric_int_test');
  await conn.executeCommand('DROP TABLE numeric_test');
  console.log('   All NUMERIC tests passed ✓');

  // 19. Clean up
  console.log('\n19. Cleaning up...');
  await catalog.dropTable('test_users');
  await catalog.dropTable('arrow_t');
  await conn.close();
  hyper.close();

  console.log('\n=== All smoke tests passed! ===');
}

main().catch((err) => {
  console.error('Smoke test failed:', err);
  process.exit(1);
});
