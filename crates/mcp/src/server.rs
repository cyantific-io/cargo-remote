//! MCP inbound adapter: an rmcp server exposing remote cargo builds as tools.
//!
//! Tool argument structs are transport DTOs (serde + JsonSchema); each converts into validated
//! domain models via `into_domain` before touching the [`RemoteBuildService`].

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;

use rustle_core::domain::{
    enrollment_target, BuildEnv, BuildRequest, CargoCommand, CopyBack, EnvProfile, ExtraPath, Host,
    HostKeyCheck, OutputMode, Port, RemoteBuildService, RemoteDir, RemoteName, RemoteOverrides,
    RemoteSelector, RemoteValidationError, Service, TargetSelection, Toolchain,
};
use rustle_core::outbound::{
    build_transfer, CargoMetadataAdapter, FileRemoteRepository, SshExecutor, SshPool, SyncMode,
};

/// Arguments shared by the build-style tools.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct BuildArgs {
    /// The cargo command to run (build, test, check, clippy, run, …). Only honoured by the
    /// generic `build` tool; the `check`/`test`/`clippy` tools always run their own command.
    #[serde(default)]
    pub command: Option<String>,
    /// Name of a configured remote to use.
    #[serde(default)]
    pub remote: Option<String>,
    /// Explicit ssh host as user@host (ssh-config aliases are not resolved), overriding config.
    #[serde(default)]
    pub remote_host: Option<String>,
    /// SSH username (when not embedded in `remote_host` as user@host).
    #[serde(default)]
    pub user: Option<String>,
    /// SSH host-key check: `accept-new` (default), `strict`, or `accept-all` (insecure).
    #[serde(default)]
    pub host_key_check: Option<String>,
    /// ssh port override.
    #[serde(default)]
    pub port: Option<u16>,
    /// Remote build directory base (overrides config).
    #[serde(default)]
    pub temp_dir: Option<String>,
    /// Shell profile to `source` on the remote (overrides config).
    #[serde(default)]
    pub env: Option<String>,
    /// Shell command to run on the remote before the build (overrides config).
    #[serde(default)]
    pub setup: Option<String>,
    /// Extra paths to sync to the remote (overrides config).
    #[serde(default)]
    pub extra_paths: Option<Vec<ExtraPathArg>>,
    /// Path to the Cargo.toml to build. Defaults to `Cargo.toml`.
    #[serde(default)]
    pub manifest_path: Option<String>,
    /// Build only this package (`cargo -p <name>`).
    #[serde(default)]
    pub package: Option<String>,
    /// Build the whole workspace (`cargo --workspace`).
    #[serde(default)]
    pub workspace: Option<bool>,
    /// Build in release mode (adds `--release`).
    #[serde(default)]
    pub release: Option<bool>,
    /// Extra cargo arguments/flags.
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// Rustup toolchain (stable|beta|nightly|…). Defaults to `stable`.
    #[serde(default)]
    pub toolchain: Option<String>,
    /// Copy artifacts back: a path within `target/`, or `target`/empty for the whole dir.
    #[serde(default)]
    pub copy_back: Option<String>,
    /// Also copy the resolved Cargo.lock back to the client. Defaults to false.
    #[serde(default)]
    pub copy_lock: Option<bool>,
}

/// An extra path to sync to the remote (MCP DTO).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExtraPathArg {
    /// Local file or directory (absolute, or relative to the server's working directory).
    pub local: String,
    /// Destination on the remote (relative to the login home dir, or absolute).
    pub remote: String,
}

fn invalid(e: RemoteValidationError) -> McpError {
    McpError::invalid_params(e.to_string(), None)
}

/// Map MCP extra-path DTOs into domain models, rejecting empty fields.
fn map_extras(extras: Option<Vec<ExtraPathArg>>) -> Result<Option<Vec<ExtraPath>>, McpError> {
    let Some(extras) = extras else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(extras.len());
    for extra in extras {
        if extra.local.trim().is_empty() || extra.remote.trim().is_empty() {
            return Err(McpError::invalid_params(
                "extra_paths entries require non-empty local and remote".to_string(),
                None,
            ));
        }
        out.push(ExtraPath {
            local: PathBuf::from(extra.local),
            remote: extra.remote,
        });
    }
    Ok(Some(out))
}

impl BuildArgs {
    fn into_domain(
        self,
        command: &str,
    ) -> Result<(BuildRequest, RemoteSelector), McpError> {
        let selection = if self.workspace.unwrap_or(false) {
            TargetSelection::Workspace
        } else if let Some(package) = self.package {
            TargetSelection::Package(package)
        } else {
            TargetSelection::Default
        };

        let mut options = self.options.unwrap_or_default();
        if self.release.unwrap_or(false) {
            options.insert(0, "--release".to_string());
        }

        let copy_back = match self.copy_back {
            None => CopyBack::None,
            Some(s) if s.is_empty() || s == "target" => CopyBack::Target,
            Some(s) => CopyBack::Path(s),
        };

        let manifest_path =
            PathBuf::from(self.manifest_path.unwrap_or_else(|| "Cargo.toml".to_string()));

        let request = BuildRequest {
            manifest_path,
            selection,
            command: CargoCommand::new(command.to_string()).map_err(invalid)?,
            options,
            build_env: BuildEnv::new("RUST_BACKTRACE=1"),
            toolchain: Toolchain::new(self.toolchain.unwrap_or_else(|| "stable".to_string()))
                .map_err(invalid)?,
            copy_back,
            copy_lock: self.copy_lock.unwrap_or(false),
            transfer_hidden: false,
            // MCP callers need the build output captured and returned.
            output: OutputMode::Capture,
        };

        let selector = RemoteSelector {
            name: self.remote.map(RemoteName::new).transpose().map_err(invalid)?,
            overrides: RemoteOverrides {
                host: self.remote_host.map(Host::new).transpose().map_err(invalid)?,
                user: self.user,
                port: self.port.map(Port::new).transpose().map_err(invalid)?,
                temp_dir: self.temp_dir.map(RemoteDir::new).transpose().map_err(invalid)?,
                env: self.env.map(EnvProfile::new).transpose().map_err(invalid)?,
                host_key_check: match self.host_key_check.as_deref() {
                    None => None,
                    Some(s) => Some(HostKeyCheck::parse(s).ok_or_else(|| {
                        McpError::invalid_params(
                            format!("invalid host_key_check {s:?}; expected accept-new, strict, or accept-all"),
                            None,
                        )
                    })?),
                },
                setup: self.setup,
                extra_paths: map_extras(self.extra_paths)?,
            },
        };

        Ok((request, selector))
    }
}

/// The rustle MCP server.
#[derive(Clone)]
pub struct RustleServer {
    service: Service,
    // Part of rmcp's #[tool_router]/#[tool_handler] machinery; not read directly by us.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

impl Default for RustleServer {
    fn default() -> Self {
        Self::new(
            rustle_core::outbound::DEFAULT_CONCURRENCY,
            SyncMode::default(),
        )
    }
}

/// Returned by the `cargo_help` tool: a self-contained guide an agent can read to help a user
/// configure rustle (config file, per-call overrides, and the one-time auth setup).
const HELP_TEXT: &str = r#"# Configuring rustle

rustle builds/tests/checks a local Rust project on a remote host over SSH. This MCP server
exposes: cargo_build, cargo_check, cargo_test, cargo_clippy (run a cargo command remotely),
cargo_list_remotes (show configured hosts), and cargo_help (this).

## 1. Tell it which remote to use — two ways

(a) A config file (best for a stable setup). Project-local `.cargo/rustle.toml` (searched up the
    directory tree; takes precedence), or global `~/.cargo/rustle.toml`:

    [[remote]]
    name = "devbox"          # optional; omit if you only have one
    host = "172.13.1.232"    # required (may be user@host)
    user = "echo"            # ssh user (omit if embedded in host)
    port = 6922              # default 22
    temp_dir = "~/remote-builds"   # default ~/remote-builds
    env = "~/.cargo/env"     # shell profile sourced before the build (default /etc/profile)
    host_key_check = "accept-new"  # accept-new (default) | strict | accept-all
    # setup = "export PKG_CONFIG_PATH=..."        # optional shell run before the build
    # extra_paths = [{ local = "/opt/lib", remote = "lib" }]   # optional extra files to sync

(b) Per-call tool arguments (override or replace config). Every cargo_* build tool accepts:
    remote (name), remote_host, user, port, temp_dir, env, host_key_check, setup, extra_paths,
    plus build options: manifest_path, package, workspace, release, options (extra cargo flags),
    toolchain, copy_back (path within target/, or "target"), copy_lock.
    Example: cargo_build { remote_host: "172.13.1.232", user: "echo", port: 6922, release: true }.

Use cargo_list_remotes to see what's already configured.

## 2. Passwordless auth — REQUIRED ONE-TIME SETUP (a human must do this)

The tool authenticates only with SSH keys (an unencrypted ~/.ssh key or ssh-agent), never
passwords. A freshly targeted host must have the user's public key installed. The MCP server
CANNOT do this — it needs a password typed into a real terminal, which must not go through the
agent. So instruct the USER to run this once, in their own terminal:

    cargo rustle setup-key -H <host> -u <user> -p <port>

It prompts for the remote password once and installs the key (a pure-Rust ssh-copy-id). After
that, builds authenticate by key with no prompts. If a build tool returns a "not enrolled" error,
that's the fix to relay to the user. (Equivalent: `ssh-copy-id -p <port> <user>@<host>`.)

## 3. Remote host requirements

- An SSH server with SFTP, and a Rust toolchain (rustc/cargo) on PATH to build the project.
- `--sync-mode agent` (default auto) deploys a small prebuilt helper binary to
  <temp_dir>/.rustle/ (nothing is compiled on the remote; removed with the temp dir).
- Client/remote CPU arch + libc are assumed to match (no cross-compiling).

## 4. Server launch options (set in the MCP launcher command, not per call)

  --sync-mode <agent|sftp|auto>  how a push reconciles remote state (default auto)
  --jobs <n>                     max concurrent file transfers (default 16)
  --log-level <level>            error|warn|info|debug|trace (to stderr)

Register the server, e.g.:  claude mcp add rustle -- /path/to/rustle-mcp --sync-mode agent

## Auto-configure checklist for an agent
1. Ask the user for the build host, ssh user, and port (and whether to scope it per-project).
2. Write a `.cargo/rustle.toml` (project-local) or `~/.cargo/rustle.toml` with a [[remote]] block.
3. Tell the user to run `cargo rustle setup-key -H <host> -u <user> -p <port>` once in a terminal.
4. Verify with cargo_list_remotes, then a cargo_check.
"#;

#[tool_router]
impl RustleServer {
    pub fn new(concurrency: usize, sync_mode: SyncMode) -> Self {
        let start_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // Pooled SSH connections, reused across each build's push → exec → pull (and, for the
        // long-lived MCP server, across tool calls — dead connections are re-dialed).
        let pool = Arc::new(SshPool::new());
        let service = Service::new(
            Arc::new(build_transfer(sync_mode, concurrency, pool.clone())),
            Arc::new(SshExecutor::new(pool)),
            Arc::new(FileRemoteRepository::new(start_dir)),
            Arc::new(CargoMetadataAdapter::new()),
        );
        Self {
            service,
            tool_router: Self::tool_router(),
        }
    }

    async fn run(
        &self,
        args: BuildArgs,
        default_command: &str,
    ) -> Result<CallToolResult, McpError> {
        let (request, selector) = args.into_domain(default_command)?;
        let outcome = self.service.build(&request, &selector).await.map_err(|e| {
            // Key enrollment needs an interactive password the MCP server can't (and shouldn't)
            // collect — point the human at the one-time CLI setup step instead.
            if let Some((user, host, port)) = enrollment_target(&e) {
                return McpError::internal_error(
                    format!(
                        "{user}@{host}:{port} is not enrolled for passwordless access. \
                         Run this once in a terminal: cargo rustle setup-key -H {host} -u {user} -p {port}"
                    ),
                    None,
                );
            }
            McpError::internal_error(format!("{e}"), None)
        })?;

        let text = format!(
            "exit_code: {}\nsuccess: {}\ncopied_artifacts: {:?}\n\n--- stdout ---\n{}\n--- stderr ---\n{}",
            outcome.exit_code,
            outcome.success,
            outcome.copied_artifacts,
            outcome.stdout,
            outcome.stderr,
        );
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        name = "cargo_list_remotes",
        description = "List the remote build hosts defined in the rustle configuration"
    )]
    async fn list_remotes(&self) -> Result<CallToolResult, McpError> {
        let remotes = self
            .service
            .list_remotes()
            .await
            .map_err(|e| McpError::internal_error(format!("{e}"), None))?;
        if remotes.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No remotes are configured. Pass `remote_host` explicitly, or add one to \
                 ~/.cargo/rustle.toml or a project .cargo/rustle.toml."
                    .to_string(),
            )]));
        }
        let text = remotes
            .iter()
            .map(|r| {
                format!(
                    "- {} host={} port={} temp_dir={} env={}",
                    r.name.as_ref().map(|n| n.as_str()).unwrap_or("(unnamed)"),
                    r.host.as_str(),
                    r.port,
                    r.temp_dir.as_str(),
                    r.env.as_str(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(
        name = "cargo_build",
        description = "Run an arbitrary cargo command (build/test/check/clippy/run/…) on a \
                       remote host for a workspace or a single crate, returning its output"
    )]
    async fn build(
        &self,
        Parameters(args): Parameters<BuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Only the generic `build` tool honours `command`; check/test/clippy force theirs.
        let command = args.command.clone().unwrap_or_else(|| "build".to_string());
        self.run(args, &command).await
    }

    #[tool(
        name = "cargo_check",
        description = "Run `cargo check` on a remote host (fast type-check, no codegen)"
    )]
    async fn check(
        &self,
        Parameters(args): Parameters<BuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(args, "check").await
    }

    #[tool(name = "cargo_test", description = "Run `cargo test` on a remote host")]
    async fn test(
        &self,
        Parameters(args): Parameters<BuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(args, "test").await
    }

    #[tool(name = "cargo_clippy", description = "Run `cargo clippy` on a remote host")]
    async fn clippy(
        &self,
        Parameters(args): Parameters<BuildArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(args, "clippy").await
    }

    #[tool(
        name = "cargo_help",
        description = "Explain how to configure rustle for a user: the config-file format \
                       and location, the per-call remote overrides, the one-time passwordless-auth \
                       (key enrollment) setup, and server launch options. Call this when asked how \
                       to set up or auto-configure rustle."
    )]
    async fn cargo_help(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(HELP_TEXT.to_string())]))
    }
}

#[tool_handler]
impl ServerHandler for RustleServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::LATEST;
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut implementation = Implementation::from_build_env();
        implementation.name = env!("CARGO_PKG_NAME").to_string();
        implementation.version = env!("CARGO_PKG_VERSION").to_string();
        info.server_info = implementation;
        info.instructions = Some(
            "Builds, tests and checks Rust cargo workspaces or individual crates on remote \
             hosts. Use `list_remotes` to see configured hosts, then `build`/`check`/`test`/\
             `clippy`. Pass `remote_host` to target a host not in the config."
                .to_string(),
        );
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustle_core::domain::{CopyBack, OutputMode, TargetSelection};

    fn empty_args() -> BuildArgs {
        BuildArgs {
            command: None,
            remote: None,
            remote_host: None,
            user: None,
            host_key_check: None,
            port: None,
            temp_dir: None,
            env: None,
            setup: None,
            extra_paths: None,
            manifest_path: None,
            package: None,
            workspace: None,
            release: None,
            options: None,
            toolchain: None,
            copy_back: None,
            copy_lock: None,
        }
    }

    #[test]
    fn defaults_capture_output_and_command_falls_back() {
        let (req, sel) = empty_args().into_domain("check").unwrap();
        assert_eq!(req.command.as_str(), "check");
        assert!(matches!(req.output, OutputMode::Capture));
        assert!(matches!(req.copy_back, CopyBack::None));
        assert!(!req.copy_lock, "MCP must not mutate Cargo.lock by default");
        assert!(matches!(req.selection, TargetSelection::Default));
        assert!(sel.name.is_none());
    }

    #[test]
    fn release_prepends_flag_and_package_selects() {
        let args = BuildArgs {
            release: Some(true),
            package: Some("mycrate".to_string()),
            options: Some(vec!["--locked".to_string()]),
            ..empty_args()
        };
        let (req, _) = args.into_domain("build").unwrap();
        assert_eq!(req.options, vec!["--release".to_string(), "--locked".to_string()]);
        assert!(matches!(req.selection, TargetSelection::Package(p) if p == "mycrate"));
    }

    #[test]
    fn specialised_tool_ignores_stray_command_arg() {
        // A `command` passed to `check`/`test`/`clippy` must NOT override the tool's command.
        let args = BuildArgs {
            command: Some("build".to_string()),
            ..empty_args()
        };
        let (req, _) = args.into_domain("check").unwrap();
        assert_eq!(req.command.as_str(), "check");
    }
}
