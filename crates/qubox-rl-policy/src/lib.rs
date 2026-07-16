pub mod action;
pub mod model;
pub mod observation;
pub mod reward;

use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::action::idx_to_action;
use crate::model::PolicyModel;
use crate::observation::Observation;

pub const TELEMETRY_DIR: &str = "/var/lib/qubox-daemon/telemetry/rl_tuples";
pub const FILE_CAP_BYTES: u64 = 100 * 1024 * 1024;
pub const TOTAL_CAP_BYTES: u64 = 1024 * 1024 * 1024;

pub fn telemetry_path() -> PathBuf {
    PathBuf::from(TELEMETRY_DIR)
}

pub struct PolicyServer {
    model: Arc<Mutex<PolicyModel>>,
    pub bound_port: u16,
}

impl PolicyServer {
    pub async fn spawn(
        checkpoint_path: &std::path::Path,
    ) -> std::io::Result<(Self, tokio::task::JoinHandle<std::io::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_port = listener.local_addr()?.port();
        let model = PolicyModel::load(checkpoint_path)?;
        let model = Arc::new(Mutex::new(model));
        let server = Self { model, bound_port };
        let model_clone = Arc::clone(&server.model);
        let join = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await?;
                let model = Arc::clone(&model_clone);
                tokio::task::spawn_blocking(move || {
                    let mut stream = stream.into_std()?;
                    handle_connection_blocking(&mut stream, model)
                });
            }
        });
        Ok((server, join))
    }
}

fn handle_connection_blocking(
    stream: &mut std::net::TcpStream,
    model: Arc<Mutex<PolicyModel>>,
) -> std::io::Result<()> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 64 * 1024 || len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large or zero",
        ));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    let obs: Observation = bincode::deserialize(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let action_idx = {
        let m = model.blocking_lock();
        m.infer_argmax(&obs)
    };
    let action = idx_to_action(action_idx);
    let reply = bincode::serialize(&action)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let len = reply.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(&reply)?;
    stream.shutdown(Shutdown::Write)?;
    Ok(())
}

#[cfg(test)]
mod tests {}
