# Audit talking points

Prep notes for the verbal/Socratic parts of `raw/audit.md` — the questions
where the auditor wants you to explain something and trace it in the actual
source, not a curl output. Headers match `tests/audit-dry-run.sh`'s MANUAL
lines so you can jump straight from a failed/manual check to the relevant
explanation.

Function and struct names below are cited instead of line numbers on
purpose — line numbers drift with every edit, names don't (barring a
rename). If a name below doesn't exist anymore, the codebase moved since
this was written; grep for the old name's neighbors to find where it went.

---

## Functional

### How does an HTTP server work?

At the top: `main()` in `src/main.rs` reads the config path from argv,
calls `settings::load()` to parse+validate it into a `SiteBundle`, then
hands that to `hub::run(bundle)` in `src/hub.rs`, which never returns
under normal operation — it's the whole server, one function, one loop.

The shape: `hub::run` opens every configured listen address
(`ingress::open_listeners`), creates one epoll instance (`WaitSet::create`),
registers the listeners, then loops on `epoll_wait` forever. Each iteration:
accept any pending connections (`accept_drain`), and for every other fd that
became ready, dispatch to whichever of three things it is — a client peer
(`peer::Peer`), a CGI job's stdout pipe, or a CGI job's stdin pipe — doing
at most one read or write for that fd this iteration. A `Peer` is a small
state machine (`peer::link::Phase`: `Recv` -> `Cgi` (optional) -> `Send`)
that accumulates request bytes, parses once a full request has arrived
(`http::try_parse`), routes it (`dispatch::answer`), and produces a response
(`http::Outbound`) to write back out.

### I/O multiplexing

`epoll`, via the `libc` crate — `epoll_create1`, `epoll_ctl`, `epoll_wait`.
Wrapped in `src/multiplex/waitset.rs`'s `WaitSet` type: `WaitSet::create()`
calls `epoll_create1`; `WaitSet::add`/`modify`/`remove` all funnel through
one private `ctl()` helper calling `epoll_ctl`; `WaitSet::wait()` calls
`epoll_wait` and translates the raw `epoll_event` array into a `Vec<Ready>`
(readable/writable/hangup/error booleans) for the rest of the codebase to
consume without touching libc types directly.

Registration is level-triggered (no `EPOLLET`) — see the comment on
`Interest::as_events()` in `waitset.rs`: staying level-triggered is what
lets the hub honor "one read/write attempt per wake" without ever losing
track of unread bytes, since the fd stays ready until actually drained.

### Single epoll instance

Yes. `WaitSet::create()` is called exactly once, at the top of `hub::run`,
bound to the local `wait: WaitSet` variable. Every place that touches epoll
— `accept_drain`, `drop_peer`, `start_cgi_job`, `handle_cgi_stdout`,
`handle_cgi_stdin`, `reap_cgi` — takes `wait: &WaitSet` as a parameter and
reuses that same instance; nothing in the codebase calls
`epoll_create1`/`WaitSet::create` a second time. Listener sockets, client
peer sockets, and (as of the CGI rewrite — see below) CGI stdin/stdout pipe
fds are all registered on this one instance, distinguished by the `u64`
token carried in each `epoll_event` (each registration uses its own fd
value as the token, and `hub::run`'s dispatch loop checks `ev.fd` against
`listeners_by_fd`, then `jobs` (keyed by CGI stdout fd), then `stdin_index`,
then falls through to `peers`).

### Why one epoll instance, and how was it achieved

One instance means one wait point for the whole process's I/O, which is
what makes single-threaded, non-blocking service of many connections
possible at all — with N separate blocking waits you'd need N threads (or
you'd starve most connections behind whichever one you happened to be
blocked on). "How was it achieved" here is really "how do you avoid
accidentally creating a second one" — the answer is the `wait: &WaitSet`
threading described above: nothing that needs to register or wait on an fd
owns its own epoll instance, everything borrows the hub's.

### Tracing epoll_wait to a client read/write, one read/write per wake

Concrete trace, client socket:
1. `hub::run`'s loop calls `wait.wait(&mut events, WAIT_SLICE_MS)` ->
   `WaitSet::wait` -> `libc::epoll_wait` (waitset.rs).
2. Back in `hub::run`, `for ev in ready` — for an fd found in `peers`, if
   `ev.readable`, calls `peer.on_readable()`.
3. `Peer::on_readable` (`src/peer/link.rs`) does **exactly one**
   `recv_once(self.fd.as_raw_fd(), &mut tmp)` call — a single `libc::recv`
   — then returns immediately, whatever the result. It does not loop trying
   to drain the socket.
4. Symmetrically, `ev.writable` -> `peer.on_writable()` -> exactly one
   `send_once(...)` -> one `libc::send`.
5. The hub then calls `wait.modify(...)` once to set the fd's next interest
   (read-only while still receiving, write-only while a response is
   queued) based on what the one read/write just decided.

Same shape for CGI pipes since the rewrite (see "Is reading/writing ALWAYS
through epoll" below): `handle_cgi_stdout`/`handle_cgi_stdin` in `hub.rs`
each do exactly one `libc::read`/`libc::write` per call, called once per
ready event for that fd.

### Are I/O return values checked?

Yes, everywhere a syscall can fail. Pattern used throughout: the raw
`libc::` call's return value is matched — negative means check
`std::io::Error::last_os_error()` and branch on
`ErrorKind::WouldBlock`/`Interrupted` (retry-later, not an error) vs.
anything else (real failure). Concrete examples: `recv_once`/`send_once`
(peer/link.rs), `WaitSet::add`/`modify`/`remove` (multiplex/waitset.rs, all
return `io::Result<()>` and every call site matches on `Err`),
`ingress::sock::Listener::bind`/`accept_nonblocking` (checks `socket`,
`setsockopt`, `bind`, `listen`, `accept4` individually), and the CGI path
(`content::cgi::spawn` checks `pipe2`, `fork`, `dup2`; `hub::reap_cgi`
checks `waitpid`).

### If a socket errors, is the client removed?

Yes — `hub::drop_peer(wait, peers, jobs, fd)`. Called from: the main
dispatch loop when `ev.error || (ev.hangup && !ev.readable && !ev.writable)`
(hub.rs, right after resolving which peer the event is for); when
`on_readable`/`on_writable` return `PeerOutcome::Drop` (both do this on a
hard `recv`/`send` error, and `on_readable` also does it on `Ok(0)` — a
clean disconnect); when `wait.modify` itself fails after deciding a peer's
next interest; and from `reap_timeouts` for a peer that's exceeded its
idle/request timeout (`Peer::timed_out`). `drop_peer` removes the peer from
the `peers` map (the `OwnedFd` closes automatically via `Drop` — no manual
`close()` call needed) and calls `wait.remove(fd)`. It also marks any
CGI job that peer had started as `abandoned` (see the CGI-fix note below)
so an orphaned job doesn't run unsupervised.

### Is reading/writing ALWAYS through epoll?

Yes now, but this is worth walking through carefully because it wasn't
always true and is a great "trace the code with me" question. CGI used to
be a synchronous `fork()` -> write full body -> `poll()`/`waitpid()` loop
-> read all output, done inline inside request handling — blocking the
*entire* single-threaded event loop for however long the script ran. That
was rewritten (commit "Run CGI through the main epoll loop instead of
blocking it") specifically because it broke this exact audit answer.

Current shape: `content::cgi::plan()` resolves the script and builds the
env (pure local filesystem work, no process/socket I/O — same category as
`content::serve`'s file reads, not subject to "goes through epoll").
`content::cgi::spawn()` forks + execs and hands back non-blocking pipe fds
*without touching them*. `hub::start_cgi_job` registers those fds on the
same `WaitSet` as client sockets. From there, `hub::handle_cgi_stdout` /
`handle_cgi_stdin` are called only from the same `for ev in ready` loop as
peer fds, each doing one syscall per wake — identical discipline. Process
reaping (`hub::reap_cgi`) uses `libc::waitpid(pid, ..., WNOHANG)` only,
attempted once per event-loop tick (never a blocking wait), and a job only
gets its SIGKILL-then-finalize treatment once `abandoned` (deadline passed,
or the owning peer disconnected — see `hub::drop_peer`) — the previous
version's exact bug here (a `killed` flag conflating "should be killed"
with "SIGKILL already sent", which meant a disconnected client's CGI job
was never actually killed) was found and fixed by splitting it into
`abandoned` and `kill_sent`.

If asked to prove it live: `tests/integration.sh`'s "[14] Concurrent CGI
vs. static file" test fires a script that sleeps 2.5s and a concurrent
static request, and asserts the static one returns in well under a second
— that would take ~2.5s (or fail outright) under the old blocking
implementation.

---

## Configuration file

Two-level block syntax, not an NGINX copy: `site { ... }` blocks each
define one virtual server (`bind`, `name`, `max_body`, `errpage`, one or
more `path { ... }` blocks); `path /prefix { ... }` blocks define one
route (`methods`, `root` or `redirect`, `index`, `autoindex`, `cgi`,
`upload`). See `example.conf` for the real, working reference.

Load path: `settings::load()` (`src/settings/mod.rs`) reads the file, calls
`load::parse_source()` (`src/settings/load.rs` — hand-written tokenizer in
`tokenize()` plus a small recursive parser: `parse_site`/`parse_path`) to
get a `SiteBundle`, then `verify::validate()` (`src/settings/verify.rs`)
for semantic checks: every site needs a `bind` and a `path`; every `path`
needs a `/`-prefixed prefix, non-empty `methods`, and `root` or `redirect`
(`check_path`); sites sharing a bind address need distinct, non-empty
`name`s (`check_shared_binds`); no two different addresses may claim the
same numeric port (`check_port_collisions`). None of this touches epoll —
config loading is synchronous, before `hub::run` starts (matches
audit.md's own note: "There is no need to pass through epoll when reading
the configuration file").

Real values worth citing from `example.conf` when demonstrating each
sub-question (see `tests/audit-dry-run.sh` for the scripted checks that
exercise every one of these against a live instance):
- single server/port: site `solo.local`, `127.0.0.1:9090`.
- multiple ports: site `localhost` binds both `127.0.0.1:8080` and `:8081`.
- multiple hostnames, one port: `localhost` and `alt.local` both bind
  `127.0.0.1:8080`; `dispatch::select_site` (`src/dispatch/vhost.rs`) picks
  by the `Host` header, falling back to the first site declared for that
  address when the header doesn't match anything (also the correct
  behavior for "first server is default" from audit.md's config section).
- custom error pages: `errpage 404 www/errors/404.html` etc. on
  `localhost` — served by `content::site_error` (`src/content/errpage.rs`),
  which reads `site.errpages: HashMap<u16, PathBuf>` and falls back to a
  built-in page (`http::codes::default_error_html`) only if the configured
  file is missing/unreadable.
- body limit: `localhost` sets `max_body 1M` (1,048,576 bytes). Enforced
  twice: `http::decode::try_parse` rejects an over-limit `Content-Length`
  or chunked body before it's even fully buffered
  (`DecodeError::PayloadTooLarge`), and `dispatch::answer` double-checks
  `req.body.len() as u64 > site.max_body.bytes()` after parsing. There's
  also a *pre-parse* buffering ceiling in `Peer::on_readable`
  (`max_body` + a fixed header slack, `HEAD_SLACK` in peer/link.rs) so a
  client can't force unbounded buffering before the real limit check ever
  runs — that ceiling is derived from the site's configured `max_body`,
  not a separate hardcoded number.
- routes: `/`, `/old` (redirect), `/cgi-bin` (CGI) on `localhost` —
  longest-prefix matched by `dispatch::route::match_route`.
- default file for a directory: `index index.html` on `localhost`'s `/`,
  served by `content::serve::serve_directory`, which tries `rule.index`
  first and falls back to `rule.autoindex` (directory listing) only if
  there's no index file.
- accepted methods per route: `/old` allows only `GET`; `/` allows `GET
  POST DELETE`. Checked in `dispatch::answer` against `PathRule.methods`;
  mismatch is a 405 with an `Allow` header listing what *is* permitted.

### Port issues — schema-invalid sites vs. port/bind conflicts

audit.md says two things that sound similar but aren't: "Configure the
same port multiple times — the server should find the error" (should be
fatal), and "if one of these configurations isn't valid... your server
should continue to function for the other configurations" (should *not*
be fatal for the others). `verify::validate` (`src/settings/verify.rs`)
draws that line explicitly — see the module doc comment there:

- **Fatal, whole-process**: a site binding the same address twice, or two
  sites disagreeing over a shared address/port (`check_shared_binds`,
  `check_port_collisions`). These are relationships *between* sites, not
  a single site's own problem — there's no sane way to keep "half" of a
  port conflict running, so `validate` still aborts the entire load for
  these, unchanged from before.
- **Non-fatal, site-scoped**: an individual site's own schema
  (`check_site_schema`/`check_path` — missing `bind`, no `path` blocks, a
  path with neither `root` nor `redirect`, a duplicate/empty CGI
  extension, etc.). `validate` drops that one site from the `SiteBundle`
  and returns a warning string for it instead of an `Err`;
  `settings::load` logs each warning (`localhost: config warning: ...`)
  and returns the bundle with every other site intact.  If *every* site
  turns out to be invalid, `validate` still returns `Err` — there's
  nothing left to serve, same as an empty config file.

One thing this does *not* catch, on purpose: a directive that's
syntactically fine but points at nothing real (e.g. `root` set to a
directory that doesn't exist) isn't a schema error at all — `check_path`
never calls `stat`/`canonicalize` on `root`. A site like that starts up
fine and just 404s at request time for that one site; there was never
anything to drop.

Tests: `settings::tests::drops_invalid_site_keeps_valid_sibling_on_shared_port`
covers the fix directly (two sites sharing a bind, one missing
`root`/`redirect`, the valid one survives). `settings::tests::rejects_conflicting_addresses_on_same_port`
and `rejects_shared_bind_without_names`/`rejects_shared_bind_with_duplicate_hostname`
cover the still-fatal cross-site checks, unchanged.

---

## Methods and cookies

- GET/POST/DELETE handlers: `content::serve::serve_get`,
  `content::upload::handle_post`, `content::remove::handle_delete` — all
  dispatched from `dispatch::answer` based on `HttpMethod::parse(&req.method)`.
- Wrong request: `http::decode::try_parse` returns
  `DecodeError::BadRequest(&'static str)` for anything malformed (bad
  request line, missing method/target/version, unsupported HTTP version,
  malformed header). `Peer::try_finish_request`/`parse_error` turn that
  into a 400 and the connection is still served normally afterward — the
  bad request doesn't corrupt any shared state (each `Peer` is
  independent, keyed by fd in `hub::run`'s `peers` map).
- Upload integrity: `content::upload::save_multipart`/`save_raw` operate
  on raw `&[u8]` throughout — no UTF-8 string conversion of the body at
  any point — specifically so binary uploads (images, etc.) round-trip
  byte-for-byte. Boundary/header parsing does use UTF-8 (`Content-Disposition`
  lines are text by spec), but the actual file payload never is.
- Sessions/cookies: `session::Vault` (`src/session/mod.rs`) is the
  process-wide session table, `Rc<RefCell<Vault>>` shared by every `Peer`
  (single-threaded, so `RefCell` is enough — no locking needed).
  `Peer::stamp_session` (peer/link.rs) calls `Vault::resume_or_mint` on
  every response to read-or-create a session, and demonstrates it's a real
  key/value store (not just an opaque id) via a hit counter stored in each
  session's `Bag` and echoed back as `X-Session-Hits`. Session ids are
  minted by `session::mint_token`; the cookie itself is written by
  `session::set_cookie_header`.

---

## Interaction with the browser

Everything in this audit.md section except the CGI chunked/unchunked
question is inherently a "look at it in a real browser" check — see
`tests/audit-dry-run.sh` for the exact URLs to visit
(`http://127.0.0.1:8080/`, `/does-not-exist`, `/old`) with `example.conf`
loaded.

### CGI, chunked vs. unchunked

Scripted in the dry run (no browser needed) because it's fully
mechanical: `http::decode::decode_chunked` (`src/http/decode.rs`) fully
decodes a chunked request body into a plain byte buffer *during*
`try_parse`, before routing ever happens. By the time
`content::cgi::plan`/`spawn` build `CONTENT_LENGTH` and feed the CGI's
stdin, a chunked and an unchunked request with the same logical body are
byte-identical from the CGI script's point of view — there is no
CGI-specific chunked-handling code at all, because chunked framing never
reaches that layer.

---

## Siege & stress test

Real numbers, not estimates — see `tests/audit-dry-run.sh`'s output for the
latest run, and re-run `siege -b -c 50 -t 30S http://127.0.0.1:8080/`
yourself for a longer sample during the actual audit (the dry run uses a
shorter `-t 8S` burst to stay quick). Prior full runs against this
codebase: 100.00% availability, 0 failed transactions across 215,356 and
66,656-transaction runs. `valgrind --leak-check=full` under mixed traffic
(static files, both CGI interpreters including POST bodies, uploads,
deletes, chunked requests, malformed requests, a client disconnecting
mid-CGI): 0 bytes definitely lost, no invalid-read/write/use-after-free
errors. "No hanging connections" is checked via `ss -tan` after a burst —
only `LISTEN` and `TIME-WAIT` (normal, kernel-managed, self-clearing)
should remain, no lingering `ESTABLISHED` entries.

---

## Unit tests

`cargo test` — 46 tests as of this writing, all passing. Coverage the
audit specifically asks about: HTTP request parsing
(`http::decode::tests`, e.g. `parses_chunked_body`,
`parses_content_length_body`, `rejects_bad_request_line`), config
validation (`settings::tests`, e.g.
`rejects_conflicting_addresses_on_same_port`,
`rejects_shared_bind_without_names`, `rejects_unknown_directive`), and
route matching (`dispatch::tests`, e.g. `longest_prefix_wins`,
`cgi_extension_routes_to_handler`, `method_not_allowed`).

---

## General / bonus

- More than one CGI interpreter: `example.conf`'s `/cgi-bin` route
  configures both `cgi .py /usr/bin/python3` and `cgi .sh /bin/bash`
  against `www/cgi/hello.py` and `www/cgi/hello.sh`.
- Second-language implementation: `cpp/` is a separate C++ epoll rewrite
  (own `Makefile`, `src/`, `include/`) — a distinct build/runtime from the
  Rust server. Repeat the practical tests against `cpp/`'s binary
  separately; this dry run and its talking points are Rust-only.
