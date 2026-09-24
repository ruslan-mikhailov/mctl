# mctl

An interactive shell for Memcached's text protocol. Run cache commands from a terminal, inspect responses, and use tab completion and command history while you work.

## Install

Install directly from GitHub with Rust and Cargo:

```sh
cargo install --git https://github.com/ruslan-mikhailov/mctl.git
```

This installs the `mctl` executable in Cargo's binary directory (normally `~/.cargo/bin`). The repository must be reachable over HTTPS by the installer.

## Releases

Push a `v<package-version>` tag matching `Cargo.toml` to start a release. After the builds and tests pass, [GitHub Releases](https://github.com/ruslan-mikhailov/mctl/releases) receives Linux (x86-64 and ARM64), macOS (Intel and Apple Silicon), and Windows (x86-64) archives plus SHA-256 checksums. Extract the archive for your platform and put `mctl` (or `mctl.exe`) on your `PATH`.

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

## License

GPL-3.0-only. See [LICENSE](LICENSE).
