# localhost

Single-threaded HTTP/1.1 server in Rust (epoll + non-blocking I/O).

## Build

```bash
cargo build --release
```

## Run

```bash
./target/release/localhost example.conf
```

## Config sketch

```
site {
    bind 127.0.0.1:8080;
    name localhost;
    max_body 1M;
    path / {
        methods GET POST DELETE;
        root www;
        index index.html;
        autoindex on;
    }
}
```

Duplicate ports across the file are rejected at startup.
