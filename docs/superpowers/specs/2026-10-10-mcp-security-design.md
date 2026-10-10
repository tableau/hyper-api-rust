# hyperdb-mcp security fixes — design

Status: approved direction (pre-1.0 audit decision 9), revised after the
Phase-2 dual review. Breaking changes are acceptable: this lands before 1.0.0.

Findings covered:

| Finding | Problem |
|---|---|
| `A1:X-sec-b-01` | `--read-only` is bypassed by `query_data`, `query_file` and `export` |
| `A1:X-sec-b-06`, `A2:mc11-03` | `export` overwrites by default and `remove_dir_all`s any directory |
| `A2:mc9-01` / `F1:mc9-03` | the daemon's health protocol is unauthenticated loopback TCP, reachable by every local user |
| `F1:mc9-01` | a version takeover usually fails: the new daemon dies on the old hyperd's socket |

Decisions taken with the user: the health channel moves to a **per-user Unix
domain socket** (Unix) and a **named pipe with an owner-only DACL** (Windows),
replacing loopback TCP; the takeover race is fixed in the same change.

## Measured Hyper behaviour

Probed against the pinned hyperd (`0.0.26359`) on 2026-10-10. The design below
leans on every row, so a hyperd bump should re-check them (the iteration-1
tests pin the ones that matter end to end).

| Input | Result |
|---|---|
| Two statements through `prepare` or `execute_query` (Parse) | rejected, `42601` |
| Two statements through `execute_command` (simple query) | rejected, `0A000` "Multi-part queries are disabled" |
| One statement with a trailing `;`, or a trailing `--` comment, through `prepare` | accepted |
| An unbalanced `)` through `prepare` | rejected, `42601` |
| `WITH x AS (SELECT 1) DELETE FROM t ...` through `execute_query` | **runs the `DELETE`** |
| `WITH x AS (SELECT ...) INSERT INTO t ...` | **runs the `INSERT`** |
| `WITH d AS (DELETE ... RETURNING *) SELECT ...` | rejected, syntax error |
| `SELECT ... INTO t2` | rejected, "SELECT INTO is deprecated" |
| `EXPLAIN ANALYZE DELETE ...`, `EXPLAIN VERBOSE ...`, `EXPLAIN (VERBOSE) ...` | rejected, syntax error / unknown option |
| `EXPLAIN (ANALYZE) DELETE ...`, `EXPLAIN (ANALYZE) WITH ... DELETE ...` | **runs the `DELETE`** |
| `BEGIN READ ONLY` then `DELETE` | **deletes**: read-only transactions are not enforced |
| `SET default_transaction_read_only = on` | rejected, cannot change parameter |
| `/* a /* b */ c */` | the comment ends at the first `*/`: block comments do **not** nest |
| `E'a\'b'`, `$$a)b$$`, `$q$a)b$q$`, `'a\'` | all accepted; a plain string keeps `\` literal |

## 1. Read-only SQL

### Problem

`query` and `chart` reject non-read-only SQL with `engine::is_read_only_sql`
(server.rs ~2697, ~2945). `query_data` and `query_file` (server.rs ~1945,
~2008) and `export` (server.rs ~3252) do not, and a qualified name
(`persistent.public.t`, `<alias>.public.t`) reaches the read-write persistent
attachment, so `DROP TABLE persistent.public.t` succeeds under `--read-only`.

The classifier itself is also weaker than the tools assume. It reads only the
first keyword, so `WITH ... DELETE`, `WITH ... INSERT` and
`EXPLAIN (ANALYZE) DELETE` count as read-only and run (table above), through
`query` too. Its comment stripper nests block comments while Hyper does not,
so a comment Hyper closes early can hide a statement the stripper believes is
commented out.

`export` has one more hole: it splices the caller's SQL into
`COPY ({sql}) TO <path> WITH (...)` (export.rs ~257, ~392). A single statement
that starts with `SELECT` and closes the parenthesis itself can choose its own
`TO` target. That passes any keyword check and also defeats the overwrite and
protected-path checks of section 2, which look only at `params.path`.
(Hyper refuses multiple statements on both protocols, so it is the one-statement
form that matters.)

### Design

1. **A classifier that follows Hyper's grammar** (new module
   `hyperdb-mcp/src/sql_classify.rs`, re-exported from `engine`).
   `classify_statement` is rebuilt on a small tokenizer that matches Hyper's
   lexer (`hyper/parser/sql/SQLLexer.cpp`): ASCII whitespace plus Hyper's
   Unicode whitespace list (NBSP, U+2000-200F, U+2028/2029, U+3000, BOM);
   `--` to `\r` or `\n`; `/* ... */` **without nesting** (an unterminated one
   is a lex error); `'...'` with `''` (backslash literal); `E'...'` that also
   skips the byte after a backslash; a string followed by whitespace holding
   a newline (or a comment) and another `'` continues in the same escape mode
   (`checkStringContinuation`); `"..."` with `""`; `$` plus a digit is a
   parameter, any other `$` opens a dollar quote whose tag is **everything**
   up to the next `$`, closed by that exact marker; `\x` is a client command;
   `(`, `)`, `;`; words (ASCII letters, `_` or any non-whitespace non-ASCII
   character to start, uppercased); everything else is opaque. `B'`, `N'`,
   `X'`, `U&'` and `UESCAPE` do not move a string boundary, so they need no
   case of their own. Classification works on the depth-0 tokens:
   - A lex error, an unbalanced parenthesis, nesting deeper than 256 (the
     item tree is recursive, so unbounded depth would overflow the stack),
     or any token after a depth-0 `;` gives `Other` (not read-only).
   - `SELECT`, `VALUES`, `TABLE`, `SHOW` give `ReadOnly`, except that a
     depth-0 `INTO` (a reserved word, so only `SELECT ... INTO new_table`)
     gives `Dml`; Hyper refuses it unless `enable_select_into` is set. The
     same applies to the main query of a `WITH`. A statement that
     starts with `(` can only be a query (`select_with_parens`), so the
     group's contents are classified.
   - `WITH`: the statement's kind is the kind of the first depth-0 word among
     `SELECT VALUES TABLE INSERT UPDATE DELETE MERGE COPY` (a CTE cannot be
     named `SELECT`, `VALUES` or `TABLE` unquoted). Depth-0 groups before it
     (CTE bodies, column lists, a parenthesized main query) are classified
     too: a group that is a write gives that kind (a writable CTE, which
     Hyper rejects today anyway), a read-only group is remembered. With no
     such word, a remembered read-only group gives `ReadOnly`
     (`WITH x AS (...) (SELECT ...)`), otherwise `Other`. A CTE *named*
     `insert` or `delete` makes a read-only query look like DML; that false
     refusal is accepted.
   - `EXPLAIN`: Hyper's option list accepts any option name, quoted ones
     included (`EXPLAIN ("analyze") ...`), so **any** parenthesized option
     list gives the kind of the statement after it, and so do the bare
     PostgreSQL words `ANALYZE` / `ANALYSE` / `VERBOSE`. That also covers the
     explainable `TRUNCATE` (DDL) and `EXECUTE` (`Other`). A plain `EXPLAIN`
     only plans and stays `ReadOnly`; a bare `EXPLAIN` is `Other`.
   - The DDL / DML / transaction-control keyword lists stay as they are. The
     `execute` batch validator (server.rs ~2788) uses the same function and
     gets the same, now correct, answers.
   `strip_leading_sql_comments` gets the non-nesting rule too (it has other
   callers). The old doc comments claiming "Hyper rejects data-modifying CTEs"
   are replaced by the measured facts.
2. **Every SQL-taking read tool checks.** `query_data`, `query_file` and
   SQL-mode `export` call `is_read_only_sql` **unconditionally**, exactly like
   `query`: the rule belongs to the tool, not the flag. Rejection is
   `ErrorCode::SqlError` with the `query` tool's wording, naming the tool
   (`engine::require_read_only_sql`). `query_data` and `query_file` check the
   caller's original SQL before ingest, and check again after
   `replace_identifier`: `table_name` is caller-controlled, so substituting it
   can change how the SQL tokenizes. `copy_query` and the saved-query re-run
   already call `is_read_only_sql`; they inherit the stronger classifier.
3. **Export embeds only a statement Hyper parsed on its own.** Before
   splicing, `export` sends the caller's SQL alone through
   `Connection::prepare` (Parse/Describe, never executed, then dropped). Hyper
   rejects unbalanced or multi-statement input there (table above). The
   splice then puts the SQL on its own lines, `COPY (\n{sql}\n) TO ...`, so a
   trailing `--` comment cannot swallow the suffix. A statement that parses
   alone has balanced parentheses and no unterminated literal or comment, so
   it cannot close the wrapper. This guards CSV, Parquet, Arrow IPC and
   Iceberg (the two `COPY` sites); Hyper-format export copies whole tables and
   takes no SQL.
4. The internal writes these tools make (scratch-table ingest and `DROP`,
   `export_hyper`'s `CREATE DATABASE` / `ATTACH` / `DETACH` of its own
   target) are separate statements and are untouched. `export` stays allowed
   under `--read-only` (docs and `tool_schema_tests` pin that).

### Out of scope (recorded for later)

- Attaching the persistent database with `WITH (access_mode='readonly')`
  under `--read-only` would make hyperd enforce it (`extractDatabaseAttachOptions`
  in `hyper-db/hyper/cts/compiler/Compiler.cpp`), but the KV readers and the
  saved-query store run `CREATE TABLE IF NOT EXISTS` on it today, by design.
- Read-only *functions* with side effects, if Hyper has any, are not
  classified.

## 2. Export and chart never destroy what they did not write

### Problem

`ExportParams.overwrite` defaults to `true` (server.rs ~3311). With it,
`export_iceberg` calls `remove_dir_all` on whatever directory `path` names
(export.rs ~373-389), and `export_hyper` unlinks whatever file is there
(~484-508), including the session's own persistent database or an attached
`.hyper` file. `chart` with an explicit `output_path` and `overwrite=true`
writes through `write_chart_to_disk` (chart.rs ~335-364) and can clobber the
same files. `validate_output_path` (attach.rs ~721) requires an absolute path,
canonicalizes, rejects `..` and creates missing parent directories, but the
export handler then discards its result and passes the raw `params.path` on
(server.rs ~3262 vs ~3309).

### Design

1. **`overwrite` defaults to `false`** for `export` and for `chart`. An
   existing destination is then `PERMISSION_DENIED` with the existing "pass
   overwrite=true to replace it" message. Auto-generated chart paths are
   unique, so only an explicit `output_path` is affected.
2. **Use the validated path.** The path `validate_output_path` returns is the
   one that is checked and the one handed to hyperd or the filesystem; on
   Windows the export gives both the `\\?\`-free, forward-slash form
   ingest already uses, since hyperd cannot open an extended-length path.
   The response still reports the caller's `path` / `output_path`, not the
   canonical form.
3. **Never write over a file the session uses**, for every export format and
   for chart, even with `overwrite=true`. The server builds the protected set
   once per call: the persistent database, the ephemeral scratch database,
   and every `AttachSource::LocalFile` in the attach registry. When the
   destination exists, it is refused with `INVALID_ARGUMENT` if it is the
   *same file* as a protected one, compared by identity: `(dev, ino)` on Unix,
   (volume serial number, file index) on Windows. Identity sees through
   symlinks, hard links and case folding, which a path comparison does not. A
   destination that does not exist cannot be one of them. A directory
   destination that contains a protected file is refused the same way, and
   a protected file whose identity cannot be read (other than `NotFound`)
   fails the call with `INTERNAL_ERROR` rather than being skipped. The
   helper lives in a self-contained `src/file_identity.rs` (std and
   `windows-sys` only). The set is per MCP session: a file another client
   attached through the same daemon is not in it (accepted; documented).
4. **Iceberg replaces only an Iceberg table.** With `overwrite=true` and an
   existing `dest`:
   - it must be a directory that is empty, or whose top-level entries are
     only directories named `data/` and `metadata/`, with a
     `*.metadata.json` in `metadata/` (measured against hyperd 0.0.26359: an
     export writes exactly those two, with and without rows). Anything else,
     including a regular
     file or a directory holding a `.hyper` database, is refused with
     `INVALID_ARGUMENT`: "Refusing to replace '`<path>`': it is not an Iceberg
     table directory".
   - `dest` is renamed to a sibling backup `<name>.hyperdb-mcp-old-<pid>-<n>`
     and the export runs. On success the backup is removed. On failure,
     whatever this call created at `dest` is removed first (it is ours: the
     original was already moved away), then the backup is renamed back and
     the export error is returned. If the restore itself fails, the error
     names the backup path. A backup left behind by a crash is never deleted
     automatically; the docs say so.
5. **Hyper replaces only a Hyper file, and never touches it until the new one
   is complete.** With `overwrite=true` and an existing `path`, refuse unless
   it is a regular file (not a symlink, not a directory) whose first five
   bytes are `Hyper` (the file magic; verified against real files:
   `48 79 70 65 72 08 00 00 ...`). Every Hyper export, replacing or not,
   creates its database at a sibling temp path
   `<name>.hyperdb-mcp-tmp-<pid>-<n>` (hyperd accepts a database path without
   a `.hyper` extension), fills and detaches it, and renames it over `path`.
   On failure the temp file is removed and `path` was never touched. The
   existing `is_file_in_use` → `RESOURCE_BUSY` mapping applies to the final
   rename; on Windows a held file may instead fail it with
   `ERROR_ACCESS_DENIED`, which stays `PERMISSION_DENIED` (unverified
   locally; Windows CI decides).
6. CSV, Parquet and Arrow IPC keep letting hyperd's `COPY ... TO` replace a
   file when `overwrite=true`; points 1 to 3 cover them.

## 3. Daemon health channel: per-user local socket

### Problem

The health listener binds `127.0.0.1:<port>` (scanned from 7485). Every local
user can connect: `STOP` kills another user's daemon, `STATUS` leaks the
hyperd endpoint, `REPORT_HYPERD_ERROR` forces restarts. The module doc's
"at most one daemon per user" is really "per port". The bind is also the only
single-instance lock.

### State directory

Everything below lives in the state dir, and on Unix the state dir is what
keeps other users out, so it is checked rather than assumed. Today
`restrict_existing_dir` only warns when `chmod` fails (state_perms.rs
~106-122). New rule, Unix only, in both the daemon and the client: after
`ensure_owner_only_dir`, the dir must be owned by the effective uid and have
no group or other write bit. Otherwise the daemon refuses to start and the
client goes straight to local mode, each with a warning naming the path. (On
Windows the default `%LOCALAPPDATA%` location is per-user; the pipe DACL is the
boundary there.)

The daemon creates and checks the state dir **before** it opens the lock file.

### Transport

New module `daemon/control.rs` hides the platform split behind one surface
the rest of the daemon and every test uses:

```rust
/// Where a daemon's health channel listens.
pub struct HealthEndpoint(/* Unix: socket path; Windows: pipe name */);
impl HealthEndpoint {
    pub fn for_new_daemon(state_dir: &Path) -> io::Result<Self>;
    pub fn as_str(&self) -> &str;                // what daemon.json carries
    pub fn from_record(s: &str) -> Self;
}
pub struct ControlListener { .. }               // bind, accept (bounded wait), wake
pub struct ControlStream { .. }                 // Read + Write, per-operation timeouts
pub fn connect(ep: &HealthEndpoint, connect_timeout: Duration) -> io::Result<ControlStream>;
```

- **Unix:** `<state>/daemon.sock`, a `std::os::unix::net::UnixListener`,
  `chmod 0600` after `bind`. A path longer than `sun_path` (103 usable bytes
  on macOS, 107 on Linux) fails `bind` with a message naming
  `HYPERDB_STATE_DIR`; the client then falls back to local mode as it does for
  any daemon failure. A leftover socket file is removed before `bind` only
  while the daemon holds the lock and only if it is a socket (anything else
  at the path is an error). The accept loop keeps today's design:
  non-blocking listener, `libc::poll` with the 1 s cadence, accept then force
  blocking, the self-connect wake on shutdown. Per-connection read and write
  timeouts stay 5 s.
- **Windows:** `\\.\pipe\hyperdb-mcp-<pid>-<16 hex digits>`, the digits from a
  `RandomState` hash (seeded from OS randomness), so the name cannot be
  guessed or pre-created. Raw `windows-sys` 0.61 (already in the lock file):
  - every instance: `CreateNamedPipeW` with `PIPE_ACCESS_DUPLEX |
    FILE_FLAG_OVERLAPPED`, `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT |
    PIPE_REJECT_REMOTE_CLIENTS`, `PIPE_UNLIMITED_INSTANCES`, and the same
    security descriptor from SDDL `D:P(A;;GA;;;<current user SID>)` (SID from
    `GetTokenInformation(TokenUser)` + `ConvertSidToStringSidW`). The first
    instance adds `FILE_FLAG_FIRST_PIPE_INSTANCE`. Pipe names are enumerable,
    so the DACL, not the random name, keeps other users out; the random name
    stops squatting.
  - `bind` creates the first instance, so the pipe exists before
    `daemon.json` names it. The accept loop waits in an overlapped
    `ConnectNamedPipe` with a bounded `WaitForSingleObject` (1 s, rechecking
    the shutdown flag; `ERROR_PIPE_CONNECTED` counts as connected), so a lost
    wake cannot hang the listener join and with it the lock. It creates the
    next instance **before** handing the connected one to a handler, so a
    client never finds zero instances (which reads as `ERROR_FILE_NOT_FOUND`,
    "no daemon").
  - `ControlStream` does overlapped `ReadFile` / `WriteFile` with a
    per-operation timeout and `CancelIoEx` on expiry, on both the server and
    the client (the client opens with `FILE_FLAG_OVERLAPPED` through
    `OpenOptionsExt::custom_flags`). That gives Windows the same 5 s
    per-connection bound as Unix with no helper threads. `ERROR_BROKEN_PIPE`
    and `ERROR_NO_DATA` read as a closed connection. A handler calls
    `FlushFileBuffers` before closing its instance.
  - Clients retry `ERROR_PIPE_BUSY` until the connect timeout, like
    `hyperdb-api-core`'s `connect_named_pipe`; `ERROR_FILE_NOT_FOUND` means
    no daemon.
  - Known limitation: a daemon started elevated may label its pipe with a
    High integrity level that non-elevated clients cannot open. Not handled;
    recorded.

### Single-instance lock

New module `daemon/lock.rs`: `DaemonLock::try_acquire(state_dir) ->
io::Result<Option<DaemonLock>>` on `<state>/daemon.lock` (`0600`). Unix:
`flock(LOCK_EX | LOCK_NB)` through `libc`, opened with `O_NOFOLLOW |
O_CLOEXEC` (std's `File::try_lock` needs Rust 1.89; MSRV is 1.88). Windows:
open with `share_mode(0)`; a sharing violation (error 32) means held. The OS
releases both on process exit, so a crashed daemon never leaves the lock
behind. Both conflict within one process too (separate opens), which the
in-process `TestDaemon` relies on. **`daemon.lock` is never unlinked**: a
process holding the lock on a deleted inode would no longer exclude a newcomer.

The daemon holds the lock **for its whole life, until after hyperd has
exited**. That ordering is what fixes the takeover race (section 4), and the
lock state is also what clients read to tell "starting or stopping" from
"gone".

### Discovery record and `discover()`

`DaemonInfo.health_port: u16` becomes `health_endpoint: String` (the socket
path or pipe name). Everything that flows a port changes with it:
`Engine::daemon_health_port()` → `daemon_health_endpoint() -> Option<&str>`,
the status JSON key, `report_hyperd_error_to_daemon`, `send_command*`,
`ping_identified`, the doctor section and its warnings. The wake on shutdown
keeps the SeqCst pairing with the shutdown flag: `DaemonState` stores the
endpoint when it is built and an `AtomicBool` says whether the listener is
registered (a path cannot live in an atomic).

Removed outright: the port scan (`PortScan`, `resolve_port_scan`,
`resolve_port`, `scan_for_daemon`, `probe_port`, `ProbeResult`, `ScanOutcome`,
the off-base cleanup in `ensure_daemon`), `DEFAULT_DAEMON_BASE_PORT`,
`DAEMON_PORT_SCAN_SPAN`, `ENV_DAEMON_PORT` / `HYPERDB_DAEMON_PORT`,
`DaemonConfig::port`, and the CLI's `daemon --port`, `daemon stop --port`,
`daemon status --port`. The doctor's `LiveFromScan` state goes too. Isolation
between daemons is by state dir (`HYPERDB_STATE_DIR`), which tests already use.

`discover()` reads `daemon.json`, connects to its `health_endpoint` and
requires the identified `PONG`. That is the only way to find a daemon.

`ensure_daemon()`:

1. `discover()` finds a live daemon (and the version check passes): use it.
2. Otherwise probe the lock. **Held**: a daemon is starting (the record is
   written only after hyperd is up, about 6-7 s in) or stopping, so wait for
   it with `wait_for_daemon` instead of spawning a second one. **Free**: no
   daemon is alive; remove a stale record while holding the lock (closing
   today's read-then-delete race that could remove a newer daemon's record),
   release, spawn.

The doctor stays side-effect free: it reads the record and the lock state but
never cleans up.

### Legacy daemons (rc.5 and earlier)

A running pre-1.0 daemon publishes `health_port` and holds no lock. If the new
client simply spawned, the new daemon would get the lock and then fail its
hyperd-socket pre-flight against the old engine, and every new client would
sit in local mode until the old daemon exits, which by default is never. So
`daemon/legacy.rs` (marked for removal after 1.x), when `daemon.json` parses
only as the legacy shape:

1. sends an identified `PING` to `127.0.0.1:<health_port>` with the old
   protocol, then `STATUS`, and continues only if the reported pid equals the
   record's pid. Anything else is a stale record pointing at a port someone
   else may now hold, so it is removed (under the lock) without sending
   anything further. That keeps the new client from sending `STOP` to another
   user's daemon, the very bug this change fixes.
2. sends `STOP` and waits, at most 15 s, until the port stops answering. The
   rc.5 daemon removes `daemon.json` *before* joining its listener (run.rs
   ~172-174), so once the port is quiet the record is gone too and cannot
   later delete the new daemon's. The new daemon's startup wait (section 4)
   covers the old hyperd still exiting.
3. on timeout, goes to local mode and never spawns.

An old client meeting a new daemon cannot parse the record. It waits out its
10 s spawn timeout on every start and then runs in local mode; nothing is
destroyed. The CHANGELOG says so.

## 4. Takeover race

### Problem (timeline from the code)

A newer client sends `STOP`. The old daemon's `request_shutdown` sets a flag
and the health listener closes within milliseconds, but nothing in
`run_daemon`'s `select!` observes the flag except `hyperd_monitor`, every 5 s.
The client sees the health port free and spawns the new daemon at once. The
new daemon's `build_params` pre-flight connects to `<state>/sockets/hyper`,
which the old hyperd still serves, and exits with `AddrInUse` to a stderr
that is `/dev/null`. The old daemon then shuts down, the client waits out
`SPAWN_TIMEOUT` (10 s) and falls back to local mode, and no daemon runs.
Introduced by #304 (IPC engine at a fixed socket path); the comment at
run.rs ~251 claiming the pre-flight is reachable only with two daemons on
different ports is wrong.

### Design

1. **Act on STOP promptly.** `DaemonState` gains a `tokio::sync::Notify`;
   `request_shutdown` calls `notify_one()` (which stores a permit, so a STOP
   that lands before the branch is polled is not lost). `run_daemon`'s
   `select!` gets a branch awaiting it. Shutdown then runs immediately:
   remove the record, join the listener, drop hyperd (`HyperProcess::drop`,
   up to about 5.1 s), remove the socket file, and only then drop the lock.
2. **Whoever sends STOP waits for the lock.** `maybe_take_over` and
   `hyperdb-mcp daemon stop` send `STOP` and then wait until the lock is free
   (the old daemon and its hyperd are gone), bounded by `STOP_WAIT` = 15 s.
   Only then does the takeover spawn or the CLI report success. On timeout the
   takeover goes to local mode, and `daemon stop` says the daemon did not exit.
3. **The new daemon also waits at its initial start**, for every other path
   (a manual start right after a kill, a second client racing a spawn, a
   client's brief lock probe). `run_daemon` retries the lock and the
   hyperd-socket pre-flight within **one** `STARTUP_WAIT` budget of 10 s
   (`HyperProcess::drop`'s ~5.1 s, the listener join's ~1 s, and slack for a
   loaded CI runner). If the lock is held and the record's endpoint answers an
   identified `PING` on two attempts 1 s apart, a live daemon is serving, so
   fail with "already running" rather than wait the budget out (a daemon
   still answering in its last milliseconds does not count). The retry exists
   from the iteration that introduces the lock. `try_restart_hyperd` does not
   retry: there the daemon has just reaped its own child.
4. **Timeouts.** `SPAWN_TIMEOUT` rises from 10 s to 20 s to cover a
   `STARTUP_WAIT` plus hyperd's start.
5. **Make failures visible.** `main` logs a `run_daemon` error with
   `tracing::error!` to the daemon log before exiting (stderr is `/dev/null`
   when spawned); `engine.rs` logs the local-mode fallback at `warn`, not
   `debug` (it is `debug` today, engine.rs ~611).
6. Fix the wrong comments: run.rs ~246-256 (pre-flight reachability),
   ~237-242 (the obsolete `HyperProcess::drop` landmine; `HyperProcess` now
   tracks `owns_socket_directory`), spawn.rs ~157-158 and ~211-220.

Assumption, checked by hand in Phase 5: a hyperd orphaned by a `SIGKILL`ed
daemon exits on its own once its callback connection closes, so the
pre-flight retry eventually succeeds. If it does not, the new daemon fails
after `STARTUP_WAIT`, logs why, and clients use local mode; killing a stale
hyperd found through `hyper.pid` is out of scope.

### Tests

- **Takeover:** an in-process integration test, two daemons in sequence on
  **one** state dir: start daemon A, send `STOP`, immediately start daemon B on
  the same dir, and require B to become discoverable (new `started_at`), serve
  a query through its engine, and A's thread to exit cleanly, within 30 s.
  Red-before-green with `STARTUP_WAIT` set to zero (today's behaviour: B
  fails on the lock or the pre-flight).
- **Prompt STOP:** a unit test that a `STOP` makes the daemon remove its
  record within 1 s (the poll interval it replaces is 5 s), measured on the
  record, not on `run_daemon`'s return, which includes the hyperd drop.
  Red-before-green by disabling the `Notify` branch.
- **No pile-up:** a client that finds the lock held and no record waits and
  does not spawn.

## Non-goals

- `mc9-02` (server-side line length and thread count are unbounded) and
  `mc9-03` (the blocking restart inside `select!`). With the channel limited to
  the owning user, mc9-02 is a self-DoS; both stay open in the audit.
- TLS for the MCP's own connections (decision 8 deferred it).
- Fixing an old client against a new daemon.
