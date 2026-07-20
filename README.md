# localhost

A from-scratch HTTP/1.1 server in Rust — single-threaded, driven entirely by
`epoll`, no `tokio`/`mio`/`async` runtime. Built for the 01-edu / Zone01 / 42
**Localhost** project: implement enough of the HTTP protocol, a config file,
CGI, and virtual hosting to stand in for a small slice of nginx, with every
read and write going through one non-blocking event loop.

For the deep technical write-up (architecture, Rust concepts taught through
this codebase, module-by-module walkthrough, full config reference) see
[`docs/documentation.md`](docs/documentation.md).

## Authors


- 🗂️ Iana Kopylova - [ikopylov](https://discordapp.com/users/1279339146833297509)
- 👩‍💻 Sofia Busho - [sbusho](https://discordapp.com/users/1276592724979613697)
- ✍️ Adriana Stas - [astas](https://discordapp.com/users/780150798927134740)


## Features

- `GET` / `POST` / `DELETE` (+ `HEAD`), with a per-route allowed-methods list
- Static file serving, directory listings (`autoindex`), configurable index
  files, redirects
- Chunked **and** unchunked (`Content-Length`) request bodies — chunked
  transfer-encoding is fully decoded before routing, so the rest of the
  server (including CGI) never has to think about it
- File uploads: `multipart/form-data` and raw request bodies, safe filename
  handling, path-traversal protection
- Cookies and in-memory sessions, with real per-session key/value storage
  (demoed via a hit counter on every response)
- CGI via `fork`/`exec`, multiple interpreters per route (Python and Bash
  configured out of the box) — driven through the *same* epoll instance as
  client sockets, so a slow script never blocks other connections
- Virtual hosting: multiple listening ports, multiple name-based sites
  sharing one address (`Host` header routing), first-declared site as the
  default when no name matches
- Custom error pages per status code, with built-in fallbacks when one isn't
  configured
- Per-site client body size limits
- Config validation that hard-fails startup on a real port/bind conflict,
  but drops (with a logged warning) an individual site that's misconfigured
  on its own — one bad site doesn't take the rest of the server down
- Client idle/request timeouts
- Single process, single thread, one `epoll` instance, non-blocking I/O
  throughout

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/localhost <config-file>
```

[`example.conf`](example.conf) is the working reference config — it binds
three sites across three ports and exercises most of the feature list above:

```bash
./target/release/localhost example.conf
```

## Config file quickstart

Config syntax is its own small brace-delimited format (not an nginx copy).
A `site { }` block is one virtual server; a `path { }` block inside it is
one route. From `example.conf`:

```
site {
    bind 127.0.0.1:8080;
    bind 127.0.0.1:8081;
    name localhost;
    max_body 1M;
    errpage 404 www/errors/404.html;

    path / {
        methods GET POST DELETE;
        root www;
        index index.html;
        autoindex on;
        upload www/uploads;
    }

    path /cgi-bin {
        methods GET POST;
        root www/cgi;
        cgi .py /usr/bin/python3;
        cgi .sh /bin/bash;
    }
}
```

See [`docs/documentation.md`](docs/documentation.md) for the full directive
reference (every `site`/`path` option, defaults, and validation rules).

## Testing & audit

Three layers, each answering a different question:

- **`cargo test`** — unit tests for parsing/routing/validation logic in
  isolation (request parsing, chunked decoding, config validation, route
  matching, CGI env building, upload/delete path safety, etc). 47 tests as
  of this writing, all passing.
- **`bash tests/integration.sh`** — black-box HTTP behavior against a real
  running instance: boots the server, drives it with `curl`/raw sockets,
  checks status codes, headers, redirects, uploads, sessions, CGI
  (chunked and unchunked), bad configs, and a client disconnecting
  mid-CGI. 42 checks as of this writing, all passing.
- **`bash tests/audit-dry-run.sh`** — mirrors the official audit checklist
  section by section (Functional, Configuration file, Methods and cookies,
  Interaction with the browser, Port issues, Siege & stress test, Unit
  Tests, General/bonus). Boots its own server instance(s), prints
  `PASS`/`FAIL` with the actual expected-vs-actual for anything a script can
  verify, and `MANUAL` with a one-line instruction for anything that
  genuinely needs a browser or a verbal walkthrough (nothing is faked as a
  pass). 49 scripted checks passing, 0 failing, 14 flagged `MANUAL` as of
  this writing.
- **[`docs/audit-talking-points.md`](docs/audit-talking-points.md)** — prep
  notes for the audit's Socratic questions ("explain how X works", "trace
  this code with me"), citing real current function/struct names for each.

To reproduce the stress-test and memory numbers manually:

```bash
siege -b -c 50 -t 30S http://127.0.0.1:8080/      # needs the server running first
valgrind --leak-check=full ./target/debug/localhost example.conf
```

## Project structure

```
src/main.rs        CLI entrypoint: load the config, hand off to hub::run
src/hub.rs         the event loop — owns the one epoll instance, drives
                    client peers and in-flight CGI jobs
src/settings/      config file: tokenizer/parser, typed schema, validation
src/multiplex/     thin epoll wrapper (WaitSet, Interest, Ready)
src/ingress/       non-blocking TCP listener setup (bind/listen/accept)
src/http/          HTTP/1.1 message parsing and response encoding
src/content/       the actual work: static files, uploads, deletes, CGI,
                    error pages
src/dispatch/      virtual-host selection + longest-prefix route matching
src/peer/          per-client connection state machine
src/session/       in-memory cookie session store
```

See [`docs/documentation.md`](docs/documentation.md) for the module-by-module
walkthrough (key types, functions, how each module talks to its neighbors)
and a full request-lifecycle trace.

## Bonus (incomplete)

A second implementation of the server in C++ lives in [`cpp/`](cpp/) (own
`Makefile`, `src/`, `include/`) — a separate build and runtime from the Rust
version above. It builds cleanly and handles static file serving, 404s, and
malformed requests correctly, but upload and CGI are currently broken:
uploads write the raw multipart body verbatim to a hardcoded filename
instead of parsing it, and CGI requests resolve the script path incorrectly
(so they return 200 with an empty body instead of running the script). Not
ready to demonstrate as a working bonus.

## License

MIT — see [`LICENSE`](LICENSE).
