/**
 * TLS test for hyperdb-api-node against a real `hyperd` started with
 * `ssl_key` / `ssl_certificate` / `ssl_force`.
 *
 * `ssl_force` makes hyperd reject any plaintext startup; the first case
 * proves that, which is what makes every later successful connection
 * evidence of TLS. Certificates come from the `openssl` CLI; without it the
 * test is skipped, except under CI, where it fails.
 *
 * Run with: node __test__/tls.mjs
 */

import { strict as assert } from 'assert';
import { execFileSync } from 'child_process';
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'fs';
import { createRequire } from 'module';
import { tmpdir } from 'os';
import { dirname, join } from 'path';
import { fileURLToPath } from 'url';
const require = createRequire(import.meta.url);

const __dirname = dirname(fileURLToPath(import.meta.url));
const { HyperProcess, Connection, ConnectionBuilder, CreateMode } = require('../index.js');
const { ConnectionPool } = await import(join(__dirname, '..', 'pool.mjs'));

function openssl(dir, ...args) {
  execFileSync('openssl', args, { cwd: dir, stdio: 'pipe' });
}

function hasOpenssl() {
  try {
    execFileSync('openssl', ['version'], { stdio: 'pipe' });
    return true;
  } catch {
    return false;
  }
}

/** A CA, a server certificate it signs, and an unrelated CA. */
function generateCertificates(dir) {
  const newCa = (name, cn) =>
    openssl(dir, 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
      '-keyout', `${name}.key`, '-out', `${name}.pem`, '-subj', `/CN=${cn}`);
  newCa('ca', 'hyperdb-api-node test CA');
  newCa('other_ca', 'unrelated test CA');

  openssl(dir, 'req', '-newkey', 'rsa:2048', '-nodes', '-keyout', 'server.key',
    '-out', 'server.csr', '-subj', '/CN=localhost');
  writeFileSync(join(dir, 'san.ext'), 'subjectAltName=DNS:localhost,IP:127.0.0.1\n');
  openssl(dir, 'x509', '-req', '-in', 'server.csr', '-CA', 'ca.pem', '-CAkey', 'ca.key',
    '-CAcreateserial', '-days', '1', '-out', 'leaf.pem', '-extfile', 'san.ext');
  // hyperd also uses this file as its client trust store, so it holds the
  // CA after the leaf.
  writeFileSync(
    join(dir, 'server.pem'),
    readFileSync(join(dir, 'leaf.pem'), 'utf8') + readFileSync(join(dir, 'ca.pem'), 'utf8'),
  );
  // hyperd refuses a key file that group or other can read.
  chmodSync(join(dir, 'server.key'), 0o600);
}

/** `endpoint`'s port behind `host`. */
function onHost(endpoint, host) {
  return `${host}:${endpoint.slice(endpoint.lastIndexOf(':') + 1)}`;
}

function connect(endpoint, tls) {
  return new ConnectionBuilder(endpoint).tls(tls).build();
}

async function main() {
  console.log('=== hyperdb-api-node TLS test ===\n');
  if (!hasOpenssl()) {
    if (process.env.CI) throw new Error('openssl not found; the TLS test needs it on CI');
    console.log('openssl not found; skipping the TLS test.');
    return;
  }

  const dir = mkdtempSync(join(tmpdir(), 'hyperdb-node-tls-'));
  let hyper;
  try {
    generateCertificates(dir);
    hyper = new HyperProcess(undefined, {
      transport: 'tcp',
      parameters: {
        ssl_key: join(dir, 'server.key'),
        ssl_certificate: join(dir, 'server.pem'),
        ssl_force: 'true',
      },
    });
    const endpoint = hyper.endpoint;
    const rootCert = join(dir, 'ca.pem');
    console.log(`   Endpoint: ${endpoint}`);

    console.log('1. Plaintext is rejected by the ssl_force server...');
    await assert.rejects(Connection.withoutDatabase(endpoint), /SSL/);
    await assert.rejects(connect(endpoint, { mode: 'disable' }), /SSL/);

    console.log('2. require: connects over TLS and runs a query...');
    {
      const conn = await connect(endpoint, { mode: 'require' });
      assert.equal(conn.isTls, true);
      const rows = await conn.executeQuery('SELECT 41 + 1');
      assert.equal(rows[0].getInt32(0), 42);
      await conn.close();
    }

    console.log('3. verify-full: host name and IP literal both match the SANs...');
    for (const host of ['localhost', '127.0.0.1']) {
      const conn = await connect(onHost(endpoint, host), { mode: 'verify-full', rootCert });
      assert.equal(conn.isTls, true, host);
      await conn.close();
    }

    console.log('4. Verification failures...');
    await assert.rejects(
      connect(endpoint, { mode: 'verify-ca', rootCert: join(dir, 'other_ca.pem') }),
      /TLS error/,
    );
    await assert.rejects(
      connect(endpoint, { mode: 'verify-full', rootCert, serverName: 'wrong.example' }),
      /TLS error/,
    );

    console.log('5. Invalid options fail build()...');
    await assert.rejects(connect(endpoint, { mode: 'verify_full' }), /verify_full/);
    await assert.rejects(
      connect(endpoint, { mode: 'require', clientCert: join(dir, 'leaf.pem') }),
      /clientCert and clientKey must be set together/,
    );

    console.log('6. ConnectionPool with tls...');
    {
      const pool = new ConnectionPool(endpoint, join(dir, 'pool.hyper'), {
        createMode: CreateMode.CreateAndReplace,
        tls: { mode: 'verify-full', rootCert },
      });
      const conn = await pool.acquire();
      assert.equal(conn.isTls, true);
      pool.release(conn);
      assert.equal((await pool.query('SELECT 1')).length, 1);
      await pool.close();

      const plaintext = new ConnectionPool(endpoint, join(dir, 'pool.hyper'), {
        createMode: CreateMode.DoNotCreate,
      });
      await assert.rejects(plaintext.acquire(), /SSL/);
      await plaintext.close();

      // `tls: null` means no TLS, like leaving it out.
      const nullTls = new ConnectionPool(endpoint, join(dir, 'pool.hyper'), {
        createMode: CreateMode.DoNotCreate,
        tls: null,
      });
      await assert.rejects(nullTls.acquire(), /SSL/);
      await nullTls.close();
    }

    console.log('7. An unknown HyperProcess transport is rejected...');
    assert.throws(() => new HyperProcess(undefined, { transport: 'udp' }), /unknown transport `udp`/);

    if (process.platform !== 'win32') {
      console.log('8. IPC: the endpoint is a socket path; prefer is plaintext, require fails...');
      const ipc = new HyperProcess(undefined, { transport: 'ipc' });
      try {
        assert.match(ipc.endpoint, /^\//);
        const conn = await Connection.withoutDatabase(ipc.endpoint);
        assert.equal(conn.isTls, false);
        await conn.close();
        const prefer = await connect(ipc.endpoint, { mode: 'prefer' });
        assert.equal(prefer.isTls, false);
        await prefer.close();
        await assert.rejects(connect(ipc.endpoint, { mode: 'require' }));
      } finally {
        ipc.close();
      }
    }
  } finally {
    hyper?.close();
    rmSync(dir, { recursive: true, force: true });
  }

  console.log('\n=== All TLS tests passed! ===');
}

main().catch((err) => {
  console.error('\nTLS test FAILED:', err);
  process.exit(1);
});
