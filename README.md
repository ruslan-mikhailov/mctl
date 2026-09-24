# mctl

An interactive shell for Memcached's text protocol. Run cache commands from a terminal, inspect responses, and use tab completion and command history while you work.

## Install

Install from a checkout with Rust and Cargo:

```sh
cargo install --path .
```

This installs the `mctl` executable in Cargo's binary directory (normally `~/.cargo/bin`).

## Connect

```sh
mctl localhost:11211
```

Inside the shell:

```text
set greeting "hello world" --ttl=60 --flags=123
get greeting
help set
quit
```

The port defaults to `11211` if omitted. Run `mctl` without a host in a terminal to choose from recent hosts or enter a new address. Successful connections are saved locally; pass `--no-recent-hosts` to disable that list.

## Useful options

- `--tls`: connect with certificate-verified TLS; there is no plaintext fallback.
- `--readonly`: reject remote mutations before sending a request. Local history and recent-host changes still work.
- `--timeout SECONDS`: set the network command timeout (default: 5 seconds).
- `--no-recent-hosts`: do not read or update saved hosts.

Use `mctl --help` for startup options and `help` inside the shell for commands and their arguments. Input can also be piped to `mctl HOST:PORT` for batch commands; destructive `flush_all` requires interactive confirmation and is refused in batch mode.
