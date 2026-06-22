//! The agent-backed planner: deploy the bundled agent **binary** to the remote, then drive it
//! over an SSH exec channel to reconcile the build tree in a single round-trip.
//!
//! The agent binary is compiled when rustle itself is built (see this crate's `build.rs`)
//! and embedded via `include_bytes!`. Here it is uploaded over SFTP into our sandbox under
//! `<temp_dir>/.rustle/` (content-hash versioned) and executed as-is — **nothing is ever
//! compiled on the remote**. The remote stays a generic host; `rm -rf <temp_dir>` removes it.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::errors::{AgentError, TransferError};
use crate::domain::models::{Remote, TransferPlan};
use crate::domain::ports::PortFuture;
use crate::outbound::ssh::{SharedConnection, SshPool};

use super::planner::{LocalFile, LocalLink, RemotePlanner, Worklist};
use super::sftp::{ensure_dir, open_sftp, sftp_base, write_remote_executable};

// `wire` is the agent's own protocol module, included here (via `#[path]`) so the host and the
// embedded agent share one definition of the contract — no duplication, no drift. (The host
// exercises one direction of it; the unused half is dead here.)
#[allow(dead_code)]
#[path = "../../../../agent/src/proto.rs"]
mod wire;

use wire::{FileEntry, LinkEntry, Request, Response, PROTOCOL_VERSION};

/// The agent binary — compiled for this target by `build.rs` when rustle is built, and
/// embedded here. It is deployed to the remote and run as-is; nothing is ever compiled remotely.
const AGENT_BINARY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rustle-agent"));

/// Content hash of the embedded binary — the cache key and binary-name suffix, so a freshly built
/// rustle (with a new agent) deploys a new binary instead of reusing a stale one.
fn agent_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let mut h = DefaultHasher::new();
        AGENT_BINARY.hash(&mut h);
        format!("{:016x}", h.finish())
    })
}

/// The rustle temp dir: the parent of the per-project build dir `<temp_dir>/<hash>/`.
fn temp_dir_of(build_path: &str) -> &str {
    let trimmed = build_path.trim_end_matches('/');
    trimmed.rsplit_once('/').map(|(parent, _)| parent).unwrap_or(trimmed)
}

fn deploy_key(remote: &Remote, hash: &str) -> String {
    format!(
        "{}|{}|{}|{hash}",
        remote.host.as_str(),
        remote.port.get(),
        remote.user.as_deref().unwrap_or("")
    )
}

/// Plans a push by shipping the manifest to the remote agent. Holds an optional fallback planner
/// (the native SFTP planner) used in `auto` mode when the agent path fails.
pub(crate) struct AgentPlanner {
    pool: Arc<SshPool>,
    fallback: Option<Arc<dyn RemotePlanner>>,
    /// Cache of `(remote, agent-hash)` already deployed this process — so the long-lived MCP
    /// server pays the stat/compile cost once per session, not per build.
    deployed: Mutex<HashSet<String>>,
}

impl AgentPlanner {
    pub(crate) fn new(pool: Arc<SshPool>, fallback: Option<Arc<dyn RemotePlanner>>) -> Self {
        Self {
            pool,
            fallback,
            deployed: Mutex::new(HashSet::new()),
        }
    }

    /// Ensure the embedded agent binary is present on the remote, returning its remote path (in
    /// shell/`~` form, for exec). Deploys the bundled binary over SFTP — never compiles anything.
    async fn ensure_agent(
        &self,
        remote: &Remote,
        plan: &TransferPlan,
    ) -> Result<String, TransferError> {
        let hash = agent_hash();
        let agent_dir = format!("{}/.rustle", temp_dir_of(&plan.build_path));
        let bin = format!("{agent_dir}/agent-{hash}");
        let key = deploy_key(remote, hash);

        if self.deployed.lock().unwrap().contains(&key) {
            return Ok(bin);
        }

        let (_keepalive, sftp) = open_sftp(&self.pool, remote).await?;
        let bin_sftp = sftp_base(&bin);

        // Already deployed by a previous run? (survives across process restarts)
        if sftp.metadata(bin_sftp.clone()).await.is_ok() {
            self.deployed.lock().unwrap().insert(key);
            return Ok(bin);
        }

        // Deploy the embedded binary: write to a per-process temp path, then atomically rename it
        // into place (so two clients deploying the same hash never expose a half-written file).
        let mut ensured = HashSet::new();
        ensure_dir(&sftp, &sftp_base(&agent_dir), &mut ensured).await;
        let tmp = format!("{bin_sftp}.tmp.{}", std::process::id());
        write_remote_executable(&sftp, &tmp, AGENT_BINARY).await?;
        sftp.rename(tmp, bin_sftp).await.map_err(TransferError::Sftp)?;

        self.deployed.lock().unwrap().insert(key);
        Ok(bin)
    }

    /// Run the agent and exchange one Plan/Worklist over its stdin/stdout.
    async fn run_plan(
        &self,
        conn: &SharedConnection,
        bin: &str,
        plan: &TransferPlan,
        files: &[LocalFile],
        links: &[LocalLink],
    ) -> Result<Worklist, TransferError> {
        // Unquoted so the remote shell expands `~` in both the binary path and the build root,
        // matching how the build command is run.
        let command = format!("{bin} {root}", root = plan.build_path.trim_end_matches('/'));
        let channel = {
            let handle = conn.lock().await;
            handle
                .channel_open_session()
                .await
                .map_err(|e| TransferError::Ssh(e.into()))?
        };
        channel
            .exec(true, command.as_bytes())
            .await
            .map_err(|e| TransferError::Ssh(e.into()))?;
        let mut stream = channel.into_stream();

        // Handshake — assert the agent speaks our protocol version.
        send(&mut stream, &Request::Hello { proto: PROTOCOL_VERSION }).await?;
        match recv(&mut stream).await? {
            Response::HelloOk { proto, .. } if proto == PROTOCOL_VERSION => {}
            Response::HelloOk { proto, .. } => {
                return Err(AgentError::Version { host: PROTOCOL_VERSION, agent: proto }.into())
            }
            _ => return Err(AgentError::Unexpected.into()),
        }

        // Ship the manifest; the agent reconciles structure and answers with the upload list.
        let request = Request::Plan {
            include_hidden: plan.include_hidden,
            prune: plan.prune,
            excludes: plan.excludes.clone(),
            files: files
                .iter()
                .map(|f| FileEntry { rel: f.rel.clone(), size: f.len, mtime: f.mtime })
                .collect(),
            links: links
                .iter()
                .map(|l| LinkEntry { rel: l.rel.clone(), target: l.target.clone() })
                .collect(),
        };
        send(&mut stream, &request).await?;
        let response = recv(&mut stream).await?;
        let _ = send(&mut stream, &Request::Bye).await; // best-effort graceful close

        match response {
            Response::Worklist { uploads, created_dirs, pruned, symlinks } => {
                Ok(Worklist { uploads, created_dirs, pruned, symlinks })
            }
            Response::Error { message } => Err(AgentError::Reported(message).into()),
            _ => Err(AgentError::Unexpected.into()),
        }
    }

    async fn try_agent(
        &self,
        remote: &Remote,
        plan: &TransferPlan,
        files: &[LocalFile],
        links: &[LocalLink],
    ) -> Result<Worklist, TransferError> {
        let bin = self.ensure_agent(remote, plan).await?;
        let conn = self.pool.connect(remote).await?;
        self.run_plan(&conn, &bin, plan, files, links).await
    }
}

impl RemotePlanner for AgentPlanner {
    fn plan<'a>(
        &'a self,
        remote: &'a Remote,
        plan: &'a TransferPlan,
        files: &'a [LocalFile],
        links: &'a [LocalLink],
    ) -> PortFuture<'a, Result<Worklist, TransferError>> {
        Box::pin(async move {
            match self.try_agent(remote, plan, files, links).await {
                Ok(worklist) => Ok(worklist),
                Err(e) => match &self.fallback {
                    // `auto` mode: degrade loudly, never silently.
                    Some(fallback) => {
                        tracing::warn!(
                            error = %e,
                            "remote agent sync failed; falling back to native SFTP listing"
                        );
                        fallback.plan(remote, plan, files, links).await
                    }
                    None => Err(e),
                },
            }
        })
    }
}

/// Frame and send one request (mirrors the agent's sync `proto::write_frame`, async here).
async fn send<W>(w: &mut W, request: &Request) -> Result<(), AgentError>
where
    W: AsyncWriteExt + Unpin,
{
    let body = request.encode();
    w.write_all(&(body.len() as u32).to_be_bytes()).await.map_err(AgentError::Io)?;
    w.write_all(&body).await.map_err(AgentError::Io)?;
    w.flush().await.map_err(AgentError::Io)?;
    Ok(())
}

/// Read one length-prefixed response frame (mirrors the agent's `proto::read_frame`).
async fn recv<R>(r: &mut R) -> Result<Response, AgentError>
where
    R: AsyncReadExt + Unpin,
{
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.map_err(AgentError::Io)?;
    let n = u32::from_be_bytes(len) as usize;
    // Sanity cap — a corrupt length must not trigger a huge allocation.
    if n > 64 * 1024 * 1024 {
        return Err(AgentError::Malformed);
    }
    let mut body = vec![0u8; n];
    r.read_exact(&mut body).await.map_err(AgentError::Io)?;
    Response::decode(&body).ok_or(AgentError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_is_the_build_dir_parent() {
        assert_eq!(temp_dir_of("~/remote-builds/12345/"), "~/remote-builds");
        assert_eq!(temp_dir_of("/abs/rust/9/"), "/abs/rust");
    }

    #[test]
    fn agent_hash_is_stable_and_hex() {
        let h = agent_hash();
        assert_eq!(h.len(), 16);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(h, agent_hash());
    }
}
