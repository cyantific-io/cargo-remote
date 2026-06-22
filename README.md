# Rustle

Probably reinventing the wheel for fun. Build, test and check Rust cargo projects on a remote host — from the `cargo rustle` CLI, or
from an MCP server that an AI assistant (e.g. Claude) can drive.

***Use with caution on hosts you trust. The remote build directory is reused per project and
the tool runs `cargo` commands over SSH on the build host.***

## Why

 `cargo rustle` syncs your project to a beefier build host, runs cargo there, and (optionally) copies artifacts 
 back — keeping a warm, incremental build cache on the remote between runs.

## Architecture

```
crates/
  core/   rustle-core   (lib)  domain (models, ports, Service) + outbound adapters
  cli/    cargo-rustle  (bin)  clap CLI  → `cargo rustle …`
  mcp/    rustle-mcp    (bin)  MCP server (stdio) exposing remote builds as tools
  agent/  rustle-agent  (bin)  std-only sync helper, bundled into cli/mcp
```

The domain `Service` orchestrates four outbound ports — `SourceTransfer`, `RemoteExecutor`,
`RemoteRepository`, `ProjectMetadata`. The CLI and MCP server are inbound adapters that
both drive the same `Service`.

### Transfer: pure-Rust SSH (no `ssh`/`rsync` binaries)

Everything runs over a pure-Rust SSH stack ([`russh`] + [`russh-sftp`]).
Sources are synced over **SFTP** with file-level incremental semantics:

- only **changed/new files** are uploaded (compared by size + mtime against the remote),
- extraneous remote files are **pruned**,
- the remote `target/` (and hidden files, unless `--transfer-hidden`) is **never touched**, so
  cargo's incremental compilation cache stays warm between runs.

Transfers are **streamed** (no whole-file buffering) and run with **bounded concurrency** — the
SFTP session multiplexes requests, so many files transfer in parallel over one connection, and
the remote tree is traversed concurrently too. Parent directories are created once per push,
not per file. A **single SSH connection is reused** across a command's sync → build → copy-back
(and, for the MCP server, across tool calls), so each command pays one TCP+auth handshake. The
build runs over a russh exec channel on that same connection.

**Consistent incremental diff.** To work out which files changed, each push compares the local
tree against the remote build tree's **actual current state** (never a cached view), so the diff
and prune stay correct even if the remote drifted out-of-band — edited by hand, cleaned up, or
last synced from another machine. `target/` and hidden files (unless `--transfer-hidden`) are
excluded, so those subtrees are never walked.

*How* that comparison is done is selectable with `--sync-mode` (a global client setting, like
`--jobs`):

- **`agent`** — a tiny **sync agent** does the whole remote side in **one round-trip**. The host
  ships its local manifest; the agent (running on the remote) reads its own filesystem and, in a
  single exchange, diffs, prunes stale files, pre-creates the needed directories, and recreates
  changed symlinks — then replies with just the list of files whose bytes the host must upload.
  This collapses the per-directory listing, the prune, and the directory-creation round-trips
  into one. The agent is a **std-only, dependency-free binary compiled when you build rustle**
  (a `build.rs` compiles it for the target and `include_bytes!`s it into the cli/mcp). It is
  **deployed as a prebuilt binary** over SFTP into `<temp_dir>/.rustle/` (content-hash
  versioned) and run as-is — **nothing is ever compiled on the remote**, no toolchain required
  there. `rm -rf <temp_dir>` removes it; the remote stays a generic host.
- **`sftp`** — the same reconciliation done natively over SFTP: list the remote tree with
  structured `readdir` attributes (traversed concurrently), diff, prune. No remote footprint at
  all; works against any SFTP server. A few round-trips instead of one.
- **`auto`** (default) — prefer the agent; if it can't deploy/run for any reason, log a warning
  and fall back to `sftp`. The agent is always a pure optimization, never a hard dependency.

Either way the file **contents** are uploaded over SFTP — streamed, concurrent, one connection.

Requirements: SSH access to the remote (an **ssh-agent** identity or an unencrypted key in
`~/.ssh`), and a Rust toolchain on the remote **to build your project** (cargo). The sync agent
is *not* compiled there — it ships as a prebuilt binary — so the remote needs only an SSH server
with SFTP plus whatever your `cargo` build itself requires. No client-side binaries beyond this
tool.

Host keys are verified against `~/.ssh/known_hosts`: a new host is recorded on first connect,
and a **changed** key is refused (the MITM case). Override per remote with `host_key_check` /
`--host-key-check`: `accept-new` (default), `strict` (refuse hosts not already in
`known_hosts`), or `accept-all` (skip verification — insecure).

### Key enrollment (passwordless setup, built in)

The tool authenticates only with **keys** (ssh-agent or an unencrypted `~/.ssh` key) — never
passwords. If a host rejects your key but offers password login, it simply isn't set up for
passwordless access yet, and the **CLI offers to enroll it for you** — a pure-Rust `ssh-copy-id`:
it asks once for the remote password (no echo, never stored or logged), installs your public key
into the remote's `~/.ssh/authorized_keys` (idempotent, correct permissions), verifies key auth
works, and continues. You can also do it explicitly, without a build:

```bash
cargo rustle setup-key -H 172.13.1.232 -u echo -p 6922   # prompts for the password once
```

By default it installs `~/.ssh/id_ed25519.pub`; override with `--identity <path>`.

The **MCP server** can't (and shouldn't) collect a password through the AI channel, so when it
hits an un-enrolled host it returns an error telling you to run `cargo rustle setup-key …` once
in a terminal — after which the server's builds authenticate by key like everything else.

**Symlinks** in the source tree are preserved — recreated as symlinks on the remote (their
targets carried verbatim, not followed), and recreated locally on copy-back.

#### Known limitations

- **`-c` (whole `target/` copy-back)** downloads every file each time (concurrently, but no
  incremental pull); prefer `--copy-back=<path>` to copy back just the artifact you need.
- **Architecture/libc parity** between client and remote is assumed (no cross-compiling) — this
  is also why the bundled agent binary, built for the client's target, runs on the remote.
- **Concurrent builds of the same project** are serialized *within one process* (so the
  long-lived MCP server is safe), but two separate `cargo rustle` invocations building the
  same project to the same host **at the same time** can still race on the shared remote build
  dir. Builds that merely *alternate* between machines are fine — each push reads the host's
  real state, so it always syncs against whatever the last machine left.
- **Change detection uses size + mtime** (second granularity, like rsync's quick-check): a
  change that preserves both the file's size and its modification second won't be re-synced —
  save again (or `touch` it) to force it.

[`russh`]: https://crates.io/crates/russh
[`russh-sftp`]: https://crates.io/crates/russh-sftp

## CLI usage

```bash
cargo rustle [OPTIONS] <command> [-- <cargo options>...]
```

It syncs the current project to `<temp_dir>/<hash>/` on the remote (incrementally), runs
`cargo <command>` there with the remote's toolchain, and optionally (`-c`) copies the resulting
target folder (or a file within it) back. Artifacts copied back assume the remote and client
share the same CPU architecture and libc.

To pass flags through to the remote cargo, end the options with `--`. For example, to build in
release mode and copy back the result:

```bash
cargo rustle -c -- build --release
```

Build a single crate or the whole workspace:

```bash
cargo rustle --package my-crate -- build
cargo rustle --workspace -- test
```

### Configuration

Place a `.cargo/rustle.toml` in your project (searched up the directory tree, like cargo's own
`.cargo/config.toml`), or a global one at `~/.cargo/rustle.toml`. A project-local file takes
precedence.

```toml
[[remote]]
name = "myRemote"        # optional; not needed for a single remote
host = "myServer"        # required (may be user@host to include the user inline)
user = "myUser"          # ssh user (omit if already embedded in `host`)
port = 42                # default 22
temp_dir = "~/rust"      # default "~/remote-builds"
env = "~/.profile"       # default "/etc/profile"
host_key_check = "strict"  # accept-new (default) | strict | accept-all

# Arbitrary shell run on the remote just before the build, in the project dir and the same
# shell as cargo (so its `export`s reach the build). Use it to shape the remote environment.
setup = "export PKG_CONFIG_PATH=$RUSTLE_EXTRA/libfoo/lib/pkgconfig"

# Extra local paths to sync to the remote, beyond the project tree — e.g. a prebuilt
# .so/.a or header dir that a build.rs links against. `remote` is a path WITHIN the extra
# store. Synced incrementally; NOT pruned.
extra_paths = [
  { local = "/opt/vendor/libfoo", remote = "libfoo" },        # a directory
  { local = "libbar.so",          remote = "libbar.so" },     # a single file
]
```

`extra_paths` are synced into a per-project **extra store** under the rustle temp dir
(`<temp_dir>/extra/<project-hash>/`) — *outside* the build directory (so the source sync never
prunes them) and never scattered through the remote's `$HOME`. The build is given that store as
the env var **`$RUSTLE_EXTRA`** (visible to `setup`, `build.rs`, and `RUSTFLAGS`), so each
entry's `remote` is just a path inside it. Point the build at them via `RUSTFLAGS`:

```bash
cargo rustle -b 'RUSTFLAGS="-L native=$RUSTLE_EXTRA/libfoo"' -- build
```

`-b/--build-env` is injected verbatim into the remote shell, so quote values that contain
spaces (note the inner double quotes above, which survive to the remote). The whole footprint
lives under `<temp_dir>`, so `rm -rf <temp_dir>` on the remote cleans everything rustle
created.

Note: prebuilt objects must match the remote's architecture/libc, and a library needed only
at link time must be on the remote, while one needed at runtime must be wherever you run the
binary.

### Flags

```
USAGE:
    cargo rustle [OPTIONS] <command> [remote options]...

OPTIONS:
    -r, --remote <name>              Name of the remote defined in the config
    -H, --remote-host <host>         ssh build server, user@host (ssh-config aliases not resolved)
    -u, --user <user>                ssh username (if not embedded in the host as user@host)
        --host-key-check <mode>      accept-new (default) | strict | accept-all
    -p, --remote-port <port>         ssh port (default 22)
    -t, --remote-temp-dir <dir>      Remote build directory base (default ~/remote-builds)
    -e, --env <profile>              Shell profile to source on the remote (default /etc/profile)
        --setup <cmd>                Shell command to run on the remote before the build
        --extra-path <local:remote>  Extra path to sync to the remote (repeatable)
    -b, --build-env <env>            Remote env vars, e.g. RUST_BACKTRACE=1 (default RUST_BACKTRACE=1)
    -d, --rustup-default <channel>   Rustup toolchain (default stable)
    -c, --copy-back[=<path>]         Copy target/ back; -c alone = whole target, --copy-back=<p> a file
        --no-copy-lock               Don't copy Cargo.lock back
        --manifest-path <path>       Manifest to build (default Cargo.toml)
        --transfer-hidden            Also transfer hidden files/directories
        --package <name>             Build only this package (cargo -p)
        --workspace                  Build the whole workspace
    -j, --jobs <n>                   Max concurrent file transfers (default 16)
        --sync-mode <mode>           agent|sftp|auto (default auto): how a push reconciles remote state
        --identity <path>            Public key to enroll (default ~/.ssh/id_ed25519.pub)
        --log-level <level>          error|warn|info|debug|trace (default info; logs to stderr)
    -V, --version                    Print version
        --help                       Print help

ARGS:
    <command>              cargo command to run remotely (build, test, check, clippy, …),
                           or `setup-key` to enroll this machine for passwordless auth
    <remote options>...    cargo options/flags applied remotely (after `--`)
```

## MCP server

`rustle-mcp` is a stdio MCP server exposing tools (all prefixed `cargo_`):
* `cargo_build`
* `cargo_check`
* `cargo_test`
* `cargo_clippy`
* `cargo_list_remotes`
* `cargo_help` — returns a guide for configuring rustle (config file, per-call overrides,
  the one-time passwordless-auth setup); call it when asked how to set up or auto-configure the
  server for a user.

Every config field is also a tool argument (matching the CLI flags) —
`remote`, `remote_host`, `user`, `host_key_check`, `port`, `temp_dir`, `env`, `setup`,
`extra_paths` — plus
the build options `manifest_path`, `package`/`workspace`, `release`, `options`, `toolchain`,
`copy_back`, and `copy_lock`. Each is an override: when present it replaces the config value.

Register it with Claude Code:

```bash
claude mcp add rustle -- /path/to/rustle-mcp
```

or in `.mcp.json`:

```json
{
  "mcpServers": {
    "rustle": { "command": "/path/to/rustle-mcp" }
  }
}
```

The server resolves remotes from the same config files as the CLI (searching from its working
directory). Build output is captured and returned in the tool result. It accepts `--jobs <n>`
(transfer concurrency), `--sync-mode <agent|sftp|auto>`, and `--log-level <level>` (logs to
stderr); pass these in the launcher args, e.g.
`claude mcp add rustle -- /path/to/rustle-mcp --sync-mode agent --log-level warn`.

## Install

```bash
git clone https://github.com/cyantific-io/rustle
cargo install --path rustle/crates/cli   # the `cargo rustle` subcommand
cargo install --path rustle/crates/mcp   # the MCP server (optional)
```

Both **bundle the sync agent automatically** — a build script compiles it for your target and
embeds the binary — so there is nothing extra to install or deploy by hand.

## Development

```bash
cargo build --workspace
cargo clippy --workspace -- -D warnings
cargo test --workspace
```
