# localhost — technical documentation

This is the deep reference for how `localhost` actually works: the
architecture, the Rust ideas it leans on and why, a module-by-module
walkthrough, a full request trace, the complete config file reference, the
testing strategy, and an honest list of where the design draws its
boundaries. Everything below is grounded in the current source under
`src/` — function and type names are real and greppable; nothing here is
aspirational.

If you want the practical quickstart (build/run/test commands) instead,
see [`README.md`](../README.md). For audit-Q&A-shaped prep notes, see
[`audit-talking-points.md`](audit-talking-points.md).

## Table of contents

1. [Architecture overview](#architecture-overview)
2. [Rust theory, taught through this codebase](#rust-theory-taught-through-this-codebase)
3. [Module-by-module walkthrough](#module-by-module-walkthrough)
4. [Request lifecycle, traced start to finish](#request-lifecycle-traced-start-to-finish)
5. [Config file full reference](#config-file-full-reference)
6. [Testing strategy](#testing-strategy)
7. [Known limitations, stated honestly](#known-limitations-stated-honestly)

---

## Architecture overview

`localhost` is one process, one thread, one `epoll` instance. `hub::run`
(`src/hub.rs`) creates that instance with `WaitSet::create()` exactly once,
then loops on `epoll_wait` forever. Every fd the process ever touches after
startup — every listening socket, every accepted client connection, every
CGI child's stdin/stdout pipe — is registered on that same instance and
handled only when `epoll_wait` reports it ready. There is no second event
loop, no worker thread, no thread pool, and no async runtime underneath it.

**Why single-threaded + epoll instead of a thread pool or `tokio`:**

- It's the project's explicit constraint (see `raw/requirements.md`): "You
  use only one process and one thread," `libc` for anything Rust doesn't
  expose natively, and "you can't use crates that already implement server
  features like `tokio` or `nix`" (this codebase also stays away from
  `mio` in that same spirit, even though the requirements doc doesn't name
  it specifically). The point of the exercise is to build and understand
  the multiplexing yourself, not to configure a library that already did
  it.
- It also happens to be a genuinely reasonable architecture for this
  workload: an HTTP server spends almost all its time waiting on I/O, not
  computing. A single thread that never blocks can service thousands of
  idle-ish connections with no context-switch overhead and no
  synchronization at all — see the [ownership/borrowing](#ownership-and-borrowing-why-no-locks)
  section below for what that actually buys you in the code.
- A thread-per-connection or thread-pool design would need to share the
  routing config, the session store, and (for CGI) the process's fork/exec
  machinery across threads — meaning `Arc`, `Mutex`/`RwLock` everywhere,
  and a much larger surface for subtle races. Single-threaded epoll sidesteps
  all of it by construction: there is never more than one piece of code
  running at a time, so "is this safe to share" has one boring answer:
  always, because nothing else could be touching it concurrently.

**Trade-offs accepted:**

- No parallelism. A CPU-bound task (there aren't really any here — routing,
  parsing, and file I/O are all cheap) would serialize behind everything
  else. This project doesn't do CPU-heavy work, so it's a non-issue in
  practice, but it's a real limitation of the design in general.
- Every I/O operation must be non-blocking and driven by an event, which
  is significantly more bookkeeping than `std::net::TcpStream::read` in a
  thread-per-connection model. `src/peer/link.rs`'s `Peer` type exists
  entirely to hold the state that a blocking call would otherwise keep on
  its own thread's stack (partially-read request bytes, a partially-sent
  response, timestamps for timeout tracking).
- CGI is the hardest case: a `fork`/`exec`'d child process is a whole
  separate program the hub doesn't control, and the naive way to talk to
  it (write the body, then block reading its stdout until EOF) is exactly
  the single blocking call this architecture can't afford — it would stall
  every other client for as long as the script runs. `src/content/cgi.rs`
  and `src/hub.rs` solve this by treating a CGI child's pipes as first-class
  epoll citizens, described in detail in the [module walkthrough](#contentcgirs--the-cgi-subsystem).

---

## Rust theory, taught through this codebase

### Ownership and borrowing: why no locks

Because exactly one thread ever runs, and it runs one thing at a time, the
compiler's ordinary ownership/borrowing rules are already enough to prove
there's no data race — there's no *concurrent* access to prove safe in the
first place. `hub::run` owns `peers: HashMap<RawFd, Peer>` and
`jobs: HashMap<RawFd, CgiJob>` directly (not behind an `Arc` or a `Mutex`);
every function that touches them (`accept_drain`, `drop_peer`,
`start_cgi_job`, `handle_cgi_stdout`, `reap_cgi`, ...) just takes
`&mut HashMap<...>` and the borrow checker enforces that only one piece of
code has access at a time, at compile time, for free.

Contrast that with what a thread-per-connection or thread-pool version of
this server would need: the routing config would have to be `Arc<SiteBundle>`
(it already is one — see below — but for a different reason: cheap cloning
across CGI jobs, not cross-thread sharing), the session store would need
`Arc<Mutex<Vault>>` instead of `Rc<RefCell<Vault>>`, and the `peers`/`jobs`
maps themselves would need a lock (or a lock-free structure) since multiple
threads could be accepting/serving connections simultaneously. None of that
exists here, not because it was left out, but because the single-threaded
design makes it structurally unnecessary — there's a real difference between
"we didn't need to write a mutex" and "we wrote one badly."

### `OwnedFd`/`RawFd` and RAII cleanup via `Drop`

Every socket and pipe in this codebase is ultimately an OS file descriptor
— just an integer, with no compiler-enforced lifetime of its own. Rust's
`std::os::fd::OwnedFd` wraps that integer and *does* give it a lifetime: an
`OwnedFd` closes its fd exactly once, automatically, when it's dropped
(`OwnedFd: Drop`), and the type system prevents using it after that (it's
moved out, not just morally "freed"). `RawFd` is the bare integer form, used
only for the brief window where code needs to pass the fd to a `libc`
call without transferring ownership (e.g. `self.fd.as_raw_fd()` right
before a `libc::recv`).

Concretely: `Peer` (`src/peer/link.rs`) owns `fd: OwnedFd`. When
`hub::drop_peer` does `peers.remove(&fd)`, the returned `Peer` is dropped
at the end of that statement, which drops its `OwnedFd` field, which closes
the socket — there is no explicit `libc::close(fd)` call anywhere in
`drop_peer`. The original, pre-rewrite version of this project's CGI code
*did* call `libc::close` manually at every exit path (multiple times, for
multiple pipe fds); the current `content::cgi::spawn`/`hub::CgiJob` design
instead stores the child's pipe ends as `Option<OwnedFd>` and
`OwnedFd` fields and lets them close themselves — see `CgiJob.stdin_fd:
Option<OwnedFd>` (set to `None` to deliberately close-and-drop the moment
the body is fully written, which is also how EOF gets signaled to the CGI
child) and `CgiJob.stdout_fd: OwnedFd` (closes automatically when the job
is removed from the `jobs` map in `reap_cgi`). Same pattern for
`ingress::Listener` and `multiplex::WaitSet` (the latter closes the epoll
fd itself in its own `impl Drop for WaitSet`). The practical payoff: it's
structurally impossible to forget to close one of these and leak a file
descriptor, because "forgetting" would require *not* dropping a value,
which safe Rust doesn't let you do.

### Why `unsafe` appears at all

Everything this server does at the syscall level — `epoll_create1`,
`epoll_ctl`, `epoll_wait`, `socket`/`bind`/`listen`/`accept4`, `recv`/`send`,
`fork`/`execve`/`pipe2`/`waitpid`/`kill` — is a C API exposed through the
`libc` crate as raw FFI. Rust cannot verify that a foreign function upholds
Rust's safety invariants, so calling one is `unsafe` by definition; there's
no way to do real systems programming against POSIX from Rust without it.

The discipline used throughout this codebase: `unsafe` blocks are as small
as possible — typically just the single FFI call and the pointer/length
setup it needs — and the *result* is immediately brought back into safe
Rust and checked like any other fallible operation. For example,
`recv_once` in `peer/link.rs`:

```rust
fn recv_once(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}
```

The `unsafe` block is exactly the call; everything before and after it
(bounds-checked slice access, the `Result` construction, every caller of
`recv_once`) is ordinary safe Rust. The same shape repeats in
`multiplex::WaitSet` (epoll calls), `ingress::sock::Listener` (socket setup),
and `content::cgi` (fork/exec/pipe). Nowhere in the codebase is `unsafe`
used for convenience (to skip a bounds check, dodge the borrow checker via
raw pointers, etc.) — every occurrence is a libc call Rust has no safe
wrapper for.

### Enums as explicit state machines

Loose booleans (`is_done: bool`, `was_killed: bool`, `sent_kill: bool`, ...)
let invalid combinations exist by construction — nothing stops two flags
from disagreeing about what state you're actually in. This codebase uses
enums with named variants specifically to make invalid states
unrepresentable, or at least far harder to reach by accident. A few
examples:

- `Phase` (`peer/link.rs`): `Recv | Cgi | Send`. A `Peer` is in exactly one
  of these at a time (it's a plain field, not three separate flags), and
  `on_readable`/`on_writable` `match self.phase` exhaustively — the compiler
  refuses to compile if a phase is left unhandled.
- `PeerOutcome` (`peer/link.rs`): `Ok(PeerAction) | Drop | StartCgi(CgiHandoff)`
  — what a `Peer`'s `on_readable`/`on_writable` call tells the hub to do
  next. The hub's `match` on this is exhaustive too, including the CGI
  handoff case, so it's not possible to silently ignore "this connection
  now needs a CGI job spawned for it."
- `DecodeError` (`http/decode.rs`): `Incomplete | BadRequest(&'static str) | PayloadTooLarge`
  — three genuinely different situations (need more bytes vs. reject the
  request vs. reject for size) that a single "parse failed" boolean would
  have collapsed into one, losing the information `try_finish_request`
  needs to decide whether to keep waiting for more bytes or answer with an
  error.
- `Answer` (`dispatch/mod.rs`): `Done(Outbound) | Cgi(CgiPlan)` — routing
  either produces a response outright, or produces a *plan* that still
  needs to be executed asynchronously. Modeling this as an enum return
  value (rather than, say, an `Option<Outbound>` plus a separate
  "is this CGI" boolean) is what makes it structurally impossible for
  `Peer::try_finish_request` to accidentally treat a CGI plan as a
  finished response or vice versa — the `match` has to handle both arms.

**A concrete cautionary example — the case for splitting state instead of
overloading one flag.** `hub::CgiJob` originally had a single field,
`killed: bool`, meant to answer two different questions: "should this job
be torn down" and "has `SIGKILL` already been sent to it." Those aren't the
same question — see `git show 00c79eb` for the full story — and conflating
them was a real, shipped bug: `hub::drop_peer` set `killed = true` the
moment a client disconnected mid-CGI, and `reap_cgi`'s
`if !job.killed && now >= job.deadline { kill(...) }` guard read that same
flag to decide whether a `SIGKILL` was still owed. Because both meanings
lived in one bit, a disconnected client's job looked *already killed* to
that guard and never actually got signaled — the CGI process would run to
its own natural completion (or the 60-second idle timeout, worst case)
instead of being cleaned up promptly. The fix split it into two booleans
with distinct meanings, `abandoned` (we've decided this job is unwanted —
set on client disconnect *or* deadline) and `kill_sent` (SIGKILL has
actually been issued, so don't send it twice), and rewrote `reap_cgi` to
check the right one for each question. Two fields that can't be confused
for each other, instead of one field asked to mean two things depending on
who's reading it.

### `Rc<RefCell<Vault>>` for the shared session store

`session::Vault` (`src/session/mod.rs`) is the one piece of state every
`Peer` needs shared, mutable access to (to look up or create a session on
every response). It's held as `Rc<RefCell<Vault>>`, constructed once in
`hub::run` and cloned (`Rc::clone`) into each `Peer` at accept time.

`Rc<T>` is a reference-counted pointer, and `RefCell<T>` moves Rust's
borrow-checking from compile time to run time (`borrow_mut()` panics if
something else already holds a borrow). Neither type is thread-safe —
`Rc`'s reference count isn't atomic, and two threads calling `borrow_mut()`
concurrently could both believe they have exclusive access. That's fine
*here* specifically because the whole program is single-threaded: there is
never a moment when two pieces of code are running simultaneously, so the
"two threads racing on the refcount" scenario `Rc`/`RefCell` can't defend
against simply cannot occur. If this server ever grew a second thread,
`Rc<RefCell<Vault>>` would need to become `Arc<Mutex<Vault>>` —
`Arc` (atomic reference counting) instead of `Rc`, and `Mutex` (which
actually blocks/coordinates between threads via the OS, rather than just
panicking on a same-thread double-borrow) instead of `RefCell`. Using the
lighter-weight single-threaded types here isn't a shortcut; it's the
correct choice for what the program's concurrency model actually is, and
the compiler enforces the boundary — `Rc<RefCell<_>>` doesn't implement
`Send`, so it would fail to compile the moment it was moved across an
actual thread spawn.

### `Result`-based error handling, no panics in the hot path

Every fallible operation in the request-handling path returns a `Result`
or an `Option` and is matched explicitly — there is no `.unwrap()` or
`.expect()` anywhere in `hub.rs`, `peer/link.rs`, `http/decode.rs`, or
`content/`'s handlers (a `panic!` would unwind, and since this is a single
thread with no per-connection isolation, an unhandled panic while serving
one client would take down every other connection with it — "never
crashes" is one of the project's explicit hard requirements). A couple of
concrete examples: `recv_once`/`send_once` (`peer/link.rs`) turn every
`libc::recv`/`libc::send` return value into an `io::Result`, and their only
caller, `Peer::on_readable`/`on_writable`, matches `Ok(0)` (peer closed),
`Ok(n)` (n bytes), `Err(WouldBlock)`, `Err(Interrupted)`, and `Err(_)` (real
failure) as five distinct, handled outcomes — nothing is assumed to
succeed. `http::decode::try_parse` returns
`Result<(Inbound, usize), DecodeError>`, and every caller
(`Peer::try_finish_request`) matches all three `DecodeError` variants
explicitly rather than calling `.unwrap()` and trusting the input was
well-formed. `.unwrap()`/`.expect()` do appear, but only in `#[cfg(test)]`
modules, where a panic just fails that one test.

### Why no `tokio`/async

Two reasons, one a project constraint and one a genuine trade-off. The
constraint: the assignment explicitly disallows crates that already
implement server features, naming `tokio` and `nix` specifically (this
codebase also avoids `mio` in the same spirit), precisely because the
point is to build and understand `epoll` multiplexing by hand, not
configure a library that already solved it. The trade-off, independent
of the assignment: manual `epoll_wait` polling is considerably less
ergonomic than `async`/`.await` — every piece of in-flight state that an
`async fn`'s compiler-generated state machine would hold on your behalf
(how much of the request has arrived, how much of the response has been
sent, whether a CGI job is in flight and where it's up to) has to be
modeled and stored explicitly by hand (that's exactly what `Peer` and
`CgiJob` *are*). What it buys back: total, literal visibility into every
single I/O operation the process performs — there is no hidden reactor,
no runtime thread pool, no scheduling you can't see — which is exactly
what the audit's "trace the code from `epoll_wait` to a client read/write"
question is asking you to be able to do. With `tokio`, that trace would
disappear into the runtime; here, it's `hub::run`'s `for ev in ready` loop,
in full, in one file.

---

## Module-by-module walkthrough

### `main.rs`

The entire CLI entrypoint. Reads exactly one argument (the config path),
calls `settings::load` to parse+validate it into a `SiteBundle`, prints how
many sites loaded, and calls `hub::run(bundle)`, which runs forever. Any
error at either step is printed to stderr and exits the process with
status 1 — this is the only place the process ever exits non-zero for a
config problem.

### `hub.rs` — the event loop

Owns the single `WaitSet` (epoll instance) and everything registered on
it: `listeners_by_fd: HashMap<RawFd, Listener>`, `peers: HashMap<RawFd, Peer>`,
and `jobs: HashMap<RawFd, CgiJob>` (keyed by the job's stdout fd) plus
`stdin_index: HashMap<RawFd, RawFd>` (a CGI job's stdin fd, when it has
one, mapped back to that same job key). `hub::run`'s loop: block on
`wait.wait(...)`, then for each ready fd, check which of the four
categories it belongs to (listener → `accept_drain`; a job's stdout →
`handle_cgi_stdout`; a job's stdin → `handle_cgi_stdin`; otherwise a peer →
`Peer::on_readable`/`on_writable`) and handle exactly that one event.
After the ready list is drained, it runs two per-tick sweeps every
iteration regardless of what was ready: `reap_timeouts` (drops any peer
past its idle/request timeout) and `reap_cgi` (kills any CGI job past its
deadline, reaps any exited child with a non-blocking `waitpid(...,
WNOHANG)`, and finalizes — delivers a response and tears down fds — any
job that's both reaped and either finished normally or been killed).

`CgiJob` is the struct that makes a forked CGI child a full participant in
the epoll loop instead of a blocking operation: it holds the child's `pid`,
the owning peer's fd, its stdin/stdout pipe fds, the body still to be
written and the output collected so far, and the `abandoned`/`kill_sent`
pair described in the [enums section](#enums-as-explicit-state-machines)
above.

### `settings/` — config loading

Three files with a clean split: `schema.rs` defines the typed config shape
(`SiteBundle`, `SiteBlock`, `PathRule`, `BodyLimit`, `HttpMethod`,
`RedirectRule`, `CgiProg`) with no parsing logic in it at all — just data
and a couple of small helpers (`BodyLimit::parse`, `HttpMethod::parse`,
`PathRule::cgi_for`). `load.rs` is a hand-written tokenizer
(`tokenize` → `Vec<Tok>`, where `Tok` is `Ident | Str | Sym(char)`) plus a
small recursive-descent parser (`parse_source` → `parse_site` →
`parse_path`) that turns config text into a `SiteBundle` with no semantic
validation at all — a syntactically well-formed but semantically nonsense
config (no `bind`, a `path` with neither `root` nor `redirect`, ...) parses
fine at this stage. `verify.rs` is where semantics are checked; see the
[full config reference](#config-file-full-reference) below for exactly
what it does and, importantly, the fatal-vs-per-site-dropped split that's
the whole point of `verify::validate`'s current design. `mod.rs` ties the
three together: `settings::load(path)` reads the file, calls
`load::parse_source`, then `verify::validate(&mut bundle)`, and logs any
per-site warnings the latter returns before handing the (possibly reduced)
bundle back to `main`.

### `multiplex/` — the epoll wrapper

One type, `WaitSet` (`multiplex/waitset.rs`), wrapping exactly the three
epoll syscalls the whole server needs: `create` (`epoll_create1`), `add`/
`modify`/`remove` (all three funnel through one private `ctl` helper
calling `epoll_ctl`), and `wait` (`epoll_wait`, translating the raw
`libc::epoll_event` array into a `Vec<Ready>` of plain booleans —
`readable`/`writable`/`hangup`/`error` — so nothing outside this module
ever touches a raw `epoll_event` or an `EPOLLIN`-style bitmask directly).
`Interest` is the small struct (`read: bool, write: bool`) callers build to
say what they want to be notified about; `Interest::as_events()` always
ORs in `EPOLLERR | EPOLLHUP` regardless of what was asked for, and
everything is registered level-triggered (no `EPOLLET`) — deliberately,
per the comment on `as_events`, because level-triggered is what lets the
hub honor "at most one read/write attempt per wake" without ever silently
losing bytes that were ready but not yet read.

### `ingress/` — listening sockets

`ingress::sock::Listener` is a non-blocking, `accept4`'d-with-`SOCK_NONBLOCK`
TCP listener: `Listener::bind(addr)` does the raw `socket`/`setsockopt(SO_REUSEADDR)`/
`fcntl(O_NONBLOCK)`/`bind`/`listen` sequence by hand (IPv4 and IPv6 both
handled), and `accept_nonblocking()` does one `accept4` call, returning
`WouldBlock` when there's nothing pending rather than blocking. The
module-level `open_listeners(addrs)` function is what `hub::run` actually
calls: it dedupes addresses, tries to bind each one, and — this is
deliberate, not an oversight — only *requires* that at least one bind
succeeds; individual bind failures are logged as warnings and skipped, so
one already-in-use address doesn't prevent every other listener from
coming up.

### `http/` — message parsing and encoding

`decode.rs`'s `try_parse(buf, max_body) -> Result<(Inbound, usize), DecodeError>`
is the request parser, written to be called repeatedly against a growing
buffer (`Peer` calls it again each time more bytes arrive) rather than
assuming the whole request is already present: it returns
`DecodeError::Incomplete` — not an error the caller should give up on, just
"come back with more bytes" — whenever it can't yet find the end of the
headers, or (for a `Content-Length` body) doesn't yet have that many body
bytes buffered, or (for a chunked body) the current chunk isn't fully
buffered yet. It fully decodes `Transfer-Encoding: chunked` bodies into a
plain byte buffer right here (`decode_chunked`), rejects a request that
sends both `Transfer-Encoding: chunked` and `Content-Length` (ambiguous per
spec), and enforces `max_body` against both encodings before ever handing
a body to the rest of the server. `encode.rs`'s `Outbound` is the response
side — a status code, a header list, and a body — with `to_bytes()` doing
the one-shot serialization into wire format (auto-filling `Content-Length`
and `Connection: close` if the caller didn't set them). `codes.rs` is just
the `Status` constants and the reason-phrase/default-error-HTML tables.

### `content/` — where the actual work happens

Everything that turns a matched route into an `Outbound` lives here:
`serve.rs` (`serve_get` — static files, directory index resolution,
`autoindex` listings), `upload.rs` (`handle_post` — `multipart/form-data`
*and* raw-body uploads, byte-level throughout so binary uploads round-trip
correctly instead of being mangled by a UTF-8 string pass), `remove.rs`
(`handle_delete`), `errpage.rs` (`site_error` — configured error page if
one exists and is readable, otherwise a built-in fallback), `mime.rs` (a
small extension → `Content-Type` table), `map.rs` (`resolve` — turns a URL
path plus a route's `root` into a filesystem path, rejecting `..`
traversal both lexically and via `canonicalize()`), and `cgi.rs`.

#### `content/cgi.rs` — the CGI subsystem

This is the most architecturally involved part of the codebase, because
it's the one place where "the rest of the story" isn't inside this
process — it's a forked child running an arbitrary interpreter. The module
is deliberately split into three functions with three different jobs,
because only one of them is allowed to touch the event loop:

- **`plan(site, rule, req, url_path, listen, head_only) -> Result<CgiPlan, Outbound>`**
  resolves the script path (via `content::map::resolve`, same
  traversal-safe resolution static files use), checks it exists, and
  builds the full CGI environment variable list (`REQUEST_METHOD`,
  `QUERY_STRING`, `CONTENT_LENGTH`, `PATH_INFO`, `SCRIPT_NAME`,
  `SERVER_NAME`, `SERVER_PORT`, `HTTP_COOKIE` when present, ...). This is
  pure local filesystem work — no process or socket I/O — so it stays
  synchronous, the same way `content::serve`'s file reads do; there's
  nothing here that could block waiting on a *client*.
- **`spawn(plan: &CgiPlan) -> Result<SpawnedCgi, ()>`** is the only function
  that actually forks: `pipe2` for stdin and stdout, `fork`, and in the
  child, `dup2` the pipes onto fd 0/1 and `execve` the interpreter
  (`enter_child`, which never returns to Rust). In the parent, it sets
  both pipe ends non-blocking and returns `SpawnedCgi { pid, stdin:
  Option<OwnedFd>, stdout: OwnedFd }` — `stdin` is `None` when the request
  had no body, because there's nothing to write and the pipe was already
  closed to signal immediate EOF to the child. Critically, `spawn` never
  writes the body or reads any output itself.
- **`finish(out_buf: &[u8], head_only: bool) -> Outbound`** parses whatever
  bytes were collected from the child's stdout (`Header: value` lines, a
  blank line, then the body — the CGI convention) into a response. It's
  pure, synchronous, in-memory parsing, called only once the hub has
  already collected all the output.

The fds `spawn` hands back are registered on the *same* `WaitSet` as every
client socket by `hub::start_cgi_job`, keyed in `hub::run`'s `jobs`/
`stdin_index` maps. From there, `hub::handle_cgi_stdout`/`handle_cgi_stdin`
are called only from the same `for ev in ready` dispatch as peer fds, and
each does exactly one `libc::read`/`libc::write` per call — identical
discipline to `Peer::on_readable`/`on_writable`. This is what lets a CGI
script run for its full `CGI_TIMEOUT` (5 seconds) without stalling any
other client: the fork/exec is instantaneous, and every byte after that
flows through the ordinary epoll dispatch, one syscall at a time, same as
everything else. (This design replaced an earlier, fully synchronous
version — fork, write the whole body, then block in a `poll()`/`waitpid()`
loop until the child finished — that blocked the entire single-threaded
event loop for as long as the script ran; see the git history around
"Run CGI through the main epoll loop instead of blocking it" for that
rewrite.)

### `dispatch/` — routing

`vhost.rs`'s `select_site(bundle, listen, host_header)` picks which
`SiteBlock` a request belongs to: filter to sites bound to the exact
`listen` address, then look for one whose `name` list contains the
normalized `Host` header (port stripped, case-insensitive), falling back
to the first site declared for that address if nothing matches (or no
`Host` header was sent) — this is what "the first server for a host:port
is the default" means in practice. `route.rs`'s `match_route(site, path)`
does longest-prefix matching over that site's `path` blocks — no regular
expressions, just the longest configured prefix that the request path
starts on a `/` boundary with. `mod.rs`'s `answer(listen, req, bundle)` is
where these come together with method-allowlist checking, redirects,
`max_body` enforcement, and the CGI-vs-static-vs-upload-vs-delete decision,
returning an `Answer` (`Done(Outbound)` or `Cgi(CgiPlan)`) rather than
running CGI itself — see the [request lifecycle](#request-lifecycle-traced-start-to-finish)
below for the full sequence.

### `peer/` — per-client connection state

`Peer` (`peer/link.rs`) is the state machine for one accepted client
connection: its fd, buffers (`inbuf`/`outbuf`), a `Phase` (`Recv`/`Cgi`/`Send`),
timing bookkeeping for idle/request timeouts, and `Arc<SiteBundle>` +
`Rc<RefCell<Vault>>` handles it needs to route and stamp responses.
`on_readable`/`on_writable` are the only two entry points the hub ever
calls, and each performs exactly one `recv`/`send` syscall before
returning a `PeerOutcome` telling the hub what to do next (keep reading,
switch to writing, close the connection, or start a CGI job). See the
[request lifecycle](#request-lifecycle-traced-start-to-finish) section for
the full flow through this type.

### `session/` — cookie sessions

`session::Vault` is a `HashMap<String, Slot>` (`Slot` = last-touched
timestamp + a `Bag` of arbitrary string key/value pairs) shared across
every `Peer` via `Rc<RefCell<_>>` (see the [Rust theory section](#rcrefcellvault-for-the-shared-session-store)
above for why that's sufficient here). `resume_or_mint(cookie_header)`
looks up an existing, still-fresh session from the `Cookie` header or mints
a fresh 32-hex-character id (`mint_token`, mixing the current time,
process id, and a per-process counter — explicitly not cryptographically
secure, which is fine for a demo session store, not appropriate for
anything handling real secrets). `sweep()` evicts sessions idle longer
than 30 minutes; it's called once per `hub::run` iteration rather than on
every single request specifically to avoid an O(n) scan per request once
the table has many entries (this was a real, measured performance issue
under load from clients that don't return a cookie, fixed by moving the
sweep to the hub's per-tick cadence instead of inside `resume_or_mint`).
Every response gets a `Set-Cookie` and an `X-Session-Hits` header via
`Peer::stamp_session`, which is real, working per-session storage (not
just an opaque id) — the hit counter is read from and written back into
the session's `Bag` on every request.

---

## Request lifecycle, traced start to finish

1. **Accept.** `epoll_wait` reports a listener fd readable. `hub::run`
   recognizes it via `listeners_by_fd` and calls `accept_drain`, which
   loops calling `Listener::accept_nonblocking()` until it gets
   `WouldBlock` (draining the whole backlog in one wake, since a listener
   can have many pending connections at once — this is the one place
   the codebase intentionally does more than "one accept per wake",
   because accepting doesn't touch a *client's* data). Each accepted fd is
   registered on the same `WaitSet` (`Interest::read_only()`) and wrapped
   in a new `Peer`, inserted into `peers`.
2. **Read.** On a later wake, `epoll_wait` reports that peer fd readable.
   `hub::run` calls `peer.on_readable()`, which does exactly one
   `recv_once` call, appends whatever bytes came back to `self.inbuf`
   (after checking against the pre-parse buffering ceiling — the site's
   `max_body` plus a fixed header-size slack), and calls
   `try_finish_request()`.
3. **Parse.** `try_finish_request` calls `http::try_parse(&self.inbuf,
   self.max_body)`. If it returns `Incomplete`, the hub just goes back to
   waiting for more readable events on this fd — nothing else happens yet.
   A `BadRequest`/`PayloadTooLarge` error immediately produces an error
   `Outbound` via `parse_error` and moves to step 6.
4. **Route.** Once a full request is parsed, `try_finish_request` calls
   `dispatch::answer(self.listen_addr, &msg, &self.sites)`. This picks the
   site (`vhost::select_site`), checks the body against that site's
   `max_body`, matches the longest route prefix (`route::match_route`),
   checks the method against that route's allowlist, handles a configured
   redirect, and finally decides between CGI and everything else.
5. **Dispatch.** Non-CGI routes resolve immediately to an `Answer::Done`:
   `content::serve_get` (GET/HEAD), `content::handle_post` (POST —
   `multipart/form-data` or raw body, if the route has an `upload`
   directory configured), or `content::handle_delete` (DELETE). A CGI
   route instead calls `content::plan_cgi`, and on success returns
   `Answer::Cgi(plan)` — at which point `try_finish_request` builds a
   `CgiHandoff`, sets `self.phase = Phase::Cgi`, and returns
   `PeerOutcome::StartCgi(handoff)` instead of a finished response.
   `hub::run` then calls `content::spawn_cgi`, registers the child's pipe
   fds on the `WaitSet`, and keeps the peer's own interest at
   `read_only()` (so a client giving up early is still noticed — see the
   `abandoned`/`kill_sent` fix discussed [above](#enums-as-explicit-state-machines)).
   From there, `hub::handle_cgi_stdin`/`handle_cgi_stdout` feed/drain the
   pipes one syscall per wake until the child's stdout hits EOF (or the
   job is killed for exceeding `CGI_TIMEOUT`), at which point `reap_cgi`
   calls `content::finish_cgi` to build the response and delivers it back
   to the peer via `Peer::finish_cgi`.
6. **Respond.** Whichever path produced it, the `Outbound` is passed
   through `Peer::stamp_session` (adds `Set-Cookie`/`X-Session-Hits`),
   serialized to bytes (`Outbound::to_bytes`), stored in `self.outbuf`, and
   the peer's `Phase` becomes `Send` with its epoll interest switched to
   `write_only()`.
7. **Write.** On the next writable wake, `hub::run` calls
   `peer.on_writable()`, which does exactly one `send_once` call, advances
   `out_off` by however many bytes actually went out, and reports back
   whether the whole response has now been sent.
8. **Close.** Once the response is fully written (or if any step along the
   way hit an unrecoverable I/O error), `hub::drop_peer` removes the
   `Peer` from `peers` and calls `wait.remove(fd)`; the socket closes
   automatically when the removed `Peer` (and its `OwnedFd`) is dropped.
   This server does not keep connections alive across requests — every
   response includes `Connection: close`.

---

## Config file full reference

Config text is a small, hand-rolled brace-delimited format (see
`settings::load::tokenize`/`parse_site`/`parse_path`) — not an attempt to
copy nginx's syntax. Comments start with `//` and run to end of line.
Nothing needs to be quoted unless it contains whitespace or a reserved
character.

### `site { ... }` — one virtual server

| Directive | Syntax | Meaning | Default / notes |
|---|---|---|---|
| `bind` | `bind <ip:port>;` | One listen address for this site. Repeatable — a site can listen on multiple addresses/ports. | Required; a site with none is dropped (see validation below). |
| `name` | `name <host> [<host> ...];` | Hostname(s) this site answers to via the `Host` header. Repeatable names on one line. | Optional. Empty means "match any Host" — and required to be non-empty if this site shares a `bind` with another site (see validation). |
| `max_body` | `max_body <n>[k\|K\|m\|M\|g\|G];` | Per-site request body size ceiling. | `1M` if omitted (`BodyLimit::default`). |
| `errpage` | `errpage <code> <path>;` | Custom HTML file to serve for that status code on this site. Repeatable, one per code. | Optional; falls back to a built-in page if missing/unreadable. |
| `path` | `path <prefix> { ... }` | A route block (see below). Repeatable. | At least one required. |

### `path <prefix> { ... }` — one route

| Directive | Syntax | Meaning | Default / notes |
|---|---|---|---|
| `methods` | `methods <M> [<M> ...];` | Allowed HTTP methods for this route (`GET`, `POST`, `DELETE`, `HEAD`, ...). | Required, non-empty. A method not in the list gets `405` with an `Allow` header listing what is. |
| `root` | `root <dir>;` | Filesystem directory this route's URLs resolve under. | Required unless `redirect` is set. |
| `index` | `index <filename>;` | File served when the resolved path is a directory. | Optional — if unset (or missing on disk) and `autoindex` is off, a directory request gets `403`. |
| `autoindex` | `autoindex on\|off;` (also accepts `true`/`false`/`1`/`0`) | Generate a directory listing when there's no usable index file. | `off`. |
| `redirect` | `redirect <301\|302> <target>;` | Respond with a redirect instead of serving anything. | Optional; mutually available alongside `root` but at least one of the two is required. Only `301`/`302` accepted. |
| `cgi` | `cgi <ext> <interpreter-path>;` | Run `<interpreter-path>` for requests whose resolved file has extension `<ext>` (leading `.` optional, matching is case-insensitive). Repeatable — multiple interpreters per route are supported. | Optional. Extensions within one route must be unique. |
| `upload` | `upload <dir>;` | Enables POST uploads on this route and sets the destination directory (created if missing). | Optional — POST without this returns `403`. |

### Validation (`settings::verify::validate`)

Validation happens once, synchronously, at startup — never through epoll
(per the assignment: "no need to pass through epoll when reading the
configuration file"). It draws an explicit line between two very different
kinds of "invalid config," documented directly in the module doc comment
of `src/settings/verify.rs`:

- **Fatal — aborts the entire process, no site starts:**
  - A single site listing the same `bind` address twice.
  - Two sites sharing a `bind` address where at least one has no `name` —
    ambiguous which one should answer.
  - Two sites sharing a `bind` address with the same `name` (case-insensitive)
    — ambiguous again.
  - Two different addresses claiming the same numeric port (e.g.
    `0.0.0.0:9000` and `127.0.0.1:9000`) — this is what "configure the same
    port multiple times" in the audit checklist is testing, and it's
    unchanged, still fatal.
  - An empty config (no `site` blocks at all), or every site having been
    dropped by the next category below (nothing left to serve).
- **Non-fatal — that one site is dropped, a warning is logged, every other
  site loads and runs normally:**
  - A site with no `bind` at all.
  - A site with no `path` blocks.
  - A `path` whose prefix doesn't start with `/`.
  - A `path` with an empty `methods` list.
  - A `path` with neither `root` nor `redirect`.
  - A `path` with an empty or duplicate `cgi` extension, or an empty
    interpreter path.

The reasoning for the split (and the audit requirement each half satisfies)
is spelled out in `verify.rs`'s doc comment: a bind/port conflict is a
relationship *between* sites with no sane way to keep "half" of it running,
so it stays fatal; an individual site's own broken schema can't affect any
other site, so audit.md's "your server should continue to function for the
other configurations" requirement applies and that site is simply dropped
instead. `settings::tests::drops_invalid_site_keeps_valid_sibling_on_shared_port`
is the regression test for exactly this behavior.

---

## Testing strategy

Three layers, deliberately separate, because each answers a different
question and mixing them would make failures harder to localize:

- **`cargo test` (unit tests, one per module under `#[cfg(test)] mod tests`)**
  — is a single piece of logic correct in isolation, with no process, no
  sockets, and no filesystem beyond what an individual test sets up
  itself? Chunked/`Content-Length` parsing edge cases, config tokenizing
  and validation rules, longest-prefix route matching, CGI environment
  variable construction, upload filename sanitization and path-traversal
  rejection, session cookie parsing — all fast, all deterministic, all
  runnable without starting the server.
- **`tests/integration.sh` (black-box HTTP behavior)** — does the actual
  compiled binary, driven over real sockets exactly like a real client
  would, behave correctly end to end? It boots the server against
  `tests/integration.conf`, then uses `curl` and raw Python sockets to
  check status codes, headers, redirects, uploads round-tripping bytes
  correctly, sessions persisting across requests, CGI with both chunked
  and unchunked bodies, malformed requests, bad configs being rejected at
  startup, a slow CGI script not blocking a concurrent static request, and
  a client disconnecting mid-CGI still getting its child process reaped.
  This is the layer that would have caught the fully-synchronous-CGI
  design (a unit test on `plan`/`spawn`/`finish` individually wouldn't
  have proven the *event loop* doesn't block; a live concurrent-request
  test does).
- **`tests/audit-dry-run.sh` (audit-checklist-shaped verification)** —
  does the server satisfy the *specific* checklist the official audit will
  walk through, section by section? It mirrors `raw/audit.md`'s own
  section headings, boots its own server instance(s) against real
  `example.conf` values, and prints `PASS`/`FAIL` with actual
  expected-vs-actual for everything scriptable, or `MANUAL` with a
  one-line instruction for anything that genuinely needs a human (browser
  devtools, a verbal source-code walkthrough) — nothing is faked as a
  pass. This layer exists because "the unit tests pass and the
  integration suite passes" doesn't by itself answer "does this satisfy
  audit question 47" — someone still has to map checklist items to
  concrete checks, and this script is that mapping made executable.

`docs/audit-talking-points.md` is the non-executable fourth layer: prep
notes for the audit's Socratic questions that no script can check
("explain how X works," "trace this code with me"), citing the real
current function/struct names for each so the answer is a two-second
lookup instead of something to work out live during the audit.

---

## Known limitations, stated honestly

- **`content::cgi::plan()` is synchronous, local work — not routed through
  epoll.** Resolving the script path, checking it exists, and building the
  environment variable list all happen as ordinary in-process function
  calls before anything is forked. This is a deliberate boundary, not an
  oversight or a violation of "always through epoll": the constraint that
  matters for this project is that *client-facing* I/O and CGI *process*
  I/O never block the event loop, and `plan()` does neither — it's local
  filesystem metadata lookups, the same category of work
  `content::serve::serve_get` already does synchronously for every static
  file request (a `fs::metadata`/`fs::read` call isn't asynchronous either,
  and no version of this codebase, before or after the CGI rewrite, has
  ever routed local disk reads through epoll). The actual blocking-risk
  boundary — talking to the CGI *child process* — starts at `spawn()`, and
  everything from there on (feeding stdin, draining stdout, reaping the
  child) goes through the hub's epoll dispatch, one syscall per wake, with
  no exceptions.
- **CGI output is buffered fully in memory** (`CgiJob.out_buf: Vec<u8>`,
  grown by `extend_from_slice` on every readable wake) rather than streamed
  to the client as it arrives. For the interpreters and scripts this
  project targets that's a non-issue, but it means an unusually large CGI
  response is held entirely in RAM before any of it is written back to the
  client.
- **Sessions are in-memory only** (`session::Vault`) — they don't survive a
  process restart, and the id-generation in `mint_token` mixes wall-clock
  time, pid, and a counter, which is fine for demo/testing session
  tracking but is explicitly not a cryptographically secure token and
  shouldn't be treated as one.
- **No HTTP/1.1 persistent connections (keep-alive).** Every response is
  sent with `Connection: close` and the socket is closed once the response
  is fully written (`Peer::on_writable` returns `PeerAction::Close` the
  moment `out_off` reaches the end of `outbuf`). Each request pays the
  cost of a fresh TCP handshake; this keeps the per-connection state
  machine considerably simpler at the cost of some throughput under
  benchmarks that reuse connections.
- **The multi-site "drop the invalid one" behavior applies only to a
  site's own schema, not to `root` paths that don't exist on disk.**
  `verify::check_path` checks that a `root`/`redirect` directive is
  *present*, not that the directory it names actually exists — a route
  whose `root` points nowhere starts up fine and simply 404s at request
  time for that one route. This is intentional (path existence is a
  runtime, not a config-schema, question) but worth knowing if you're
  expecting config validation to catch it.
