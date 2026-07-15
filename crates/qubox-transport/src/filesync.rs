//! ADR-022 FileSync stream helpers: JSON handshake + bulk file transfer.
//!
//! Security mitigations (production):
//! - Path traversal: reject absolute paths, `..`, null bytes, control chars
//! - Destination confinement: resolved path must stay under `dest_dir`
//! - Size caps: `MAX_FILESYNC_BYTES` (512 MiB default)
//! - Integrity: blake3 hash of full body before atomic rename
//! - Concurrency: at most `MAX_FILESYNC_CONCURRENT` accept handlers
//! - Streaming I/O: no full-file RAM buffers for hash/send/recv

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use quinn::{Connection, RecvStream, SendStream};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::{
    read_json_prefixed, write_json_prefixed, write_stream_envelope, StreamPurpose, MAX_JSON_FRAME,
    STREAM_MAGIC,
};

/// Default max single-file bulk size (512 MiB).
pub const MAX_FILESYNC_BYTES: u64 = 512 * 1024 * 1024;
/// Max concurrent FileSync accept handlers per connection.
pub const MAX_FILESYNC_CONCURRENT: usize = 4;
/// Max relative path UTF-8 bytes.
pub const MAX_REL_PATH_BYTES: usize = 4096;
/// Max file_id UTF-8 bytes.
pub const MAX_FILE_ID_BYTES: usize = 256;

/// Reject unsafe relative paths (traversal, absolute, null, control chars).
pub fn validate_relative_path(relative_path: &str) -> anyhow::Result<()> {
    if relative_path.is_empty() {
        anyhow::bail!("empty relative_path");
    }
    if relative_path.len() > MAX_REL_PATH_BYTES {
        anyhow::bail!("relative_path too long");
    }
    if relative_path.contains('\0') {
        anyhow::bail!("relative_path contains null byte");
    }
    if relative_path.chars().any(|c| c.is_control()) {
        anyhow::bail!("relative_path contains control character");
    }
    // Windows drive / UNC style absolute-looking strings
    if relative_path.starts_with('\\') || relative_path.starts_with('/') {
        anyhow::bail!("relative_path must not start with separator");
    }
    if relative_path.chars().nth(1) == Some(':') {
        anyhow::bail!("relative_path must not include drive prefix");
    }
    let rel = Path::new(relative_path);
    if rel.is_absolute() {
        anyhow::bail!("relative_path must not be absolute");
    }
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                if s.is_empty() {
                    anyhow::bail!("empty path component");
                }
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("refusing unsafe relative_path {relative_path}");
            }
        }
    }
    Ok(())
}

/// Resolve `dest_dir/relative_path` ensuring the result stays under `dest_dir`.
pub fn resolve_safe_target(dest_dir: &Path, relative_path: &str) -> anyhow::Result<PathBuf> {
    validate_relative_path(relative_path)?;
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("create dest_dir {}", dest_dir.display()))?;
    let dest_canon = dest_dir
        .canonicalize()
        .with_context(|| format!("canonicalize dest_dir {}", dest_dir.display()))?;
    let target = dest_canon.join(relative_path);
    // Ensure parent exists, then verify deepest existing ancestor is under dest.
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent {}", parent.display()))?;
    }
    let mut check = target.clone();
    while !check.exists() {
        check = check
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("path escaped dest_dir"))?;
    }
    let check_canon = check
        .canonicalize()
        .with_context(|| format!("canonicalize {}", check.display()))?;
    if !check_canon.starts_with(&dest_canon) {
        anyhow::bail!(
            "resolved path escapes dest_dir ({} not under {})",
            check_canon.display(),
            dest_canon.display()
        );
    }
    Ok(target)
}

/// Stream blake3 + size from disk without loading the whole file into RAM.
pub async fn hash_file_streaming(path: &Path) -> anyhow::Result<([u8; 32], u64)> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
        if size > MAX_FILESYNC_BYTES {
            anyhow::bail!(
                "file {} exceeds MAX_FILESYNC_BYTES ({})",
                path.display(),
                MAX_FILESYNC_BYTES
            );
        }
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(hasher.finalize().as_bytes());
    Ok((arr, size))
}

/// Binary bulk header after stream envelope:
/// `[u16 path_len BE][path][u64 size BE][32 blake3][u16 id_len BE][file_id]`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferHeader {
    pub file_id: String,
    pub relative_path: String,
    pub size: u64,
    pub blake3: [u8; 32],
}

impl FileTransferHeader {
    pub fn encode(&self) -> Vec<u8> {
        let path_b = self.relative_path.as_bytes();
        let id_b = self.file_id.as_bytes();
        let mut out = Vec::with_capacity(2 + path_b.len() + 8 + 32 + 2 + id_b.len());
        out.extend_from_slice(&(path_b.len() as u16).to_be_bytes());
        out.extend_from_slice(path_b);
        out.extend_from_slice(&self.size.to_be_bytes());
        out.extend_from_slice(&self.blake3);
        out.extend_from_slice(&(id_b.len() as u16).to_be_bytes());
        out.extend_from_slice(id_b);
        out
    }

    pub fn decode(buf: &[u8]) -> anyhow::Result<(Self, usize)> {
        if buf.len() < 2 {
            anyhow::bail!("FileTransferHeader short");
        }
        let path_len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
        let mut off = 2;
        if buf.len() < off + path_len + 8 + 32 + 2 {
            anyhow::bail!("FileTransferHeader short body");
        }
        let relative_path = String::from_utf8(buf[off..off + path_len].to_vec())?;
        off += path_len;
        let size = u64::from_be_bytes(buf[off..off + 8].try_into()?);
        off += 8;
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(&buf[off..off + 32]);
        off += 32;
        let id_len = u16::from_be_bytes([buf[off], buf[off + 1]]) as usize;
        off += 2;
        if buf.len() < off + id_len {
            anyhow::bail!("FileTransferHeader short id");
        }
        let file_id = String::from_utf8(buf[off..off + id_len].to_vec())?;
        off += id_len;
        Ok((
            Self {
                file_id,
                relative_path,
                size,
                blake3,
            },
            off,
        ))
    }
}

pub async fn write_filesync_msg<T: Serialize>(
    send: &mut SendStream,
    msg: &T,
) -> anyhow::Result<()> {
    write_json_prefixed(send, msg).await
}

pub async fn read_filesync_msg<T: DeserializeOwned>(
    recv: &mut RecvStream,
) -> anyhow::Result<T> {
    read_json_prefixed(recv).await
}

/// Send header + file body from disk (streaming, no full RAM buffer).
pub async fn send_file_bulk(
    send: &mut SendStream,
    header: &FileTransferHeader,
    path: &Path,
) -> anyhow::Result<()> {
    if header.size > MAX_FILESYNC_BYTES {
        anyhow::bail!(
            "file {} exceeds MAX_FILESYNC_BYTES ({})",
            path.display(),
            MAX_FILESYNC_BYTES
        );
    }
    let enc = header.encode();
    send.write_all(&enc).await?;
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let n = tokio::io::copy(&mut file, send).await?;
    if n != header.size {
        anyhow::bail!(
            "sent {n} bytes but header size is {} for {}",
            header.size,
            path.display()
        );
    }
    send.finish().context("finish FileSync bulk")?;
    Ok(())
}

/// Receive bulk into `target.qubox-partial`, verify blake3, atomic rename.
pub async fn recv_file_bulk(
    recv: &mut RecvStream,
    dest_dir: &Path,
    max_bytes: u64,
) -> anyhow::Result<(FileTransferHeader, std::path::PathBuf)> {
    // Header is variable; read path_len first then rest.
    let mut path_len_buf = [0u8; 2];
    recv.read_exact(&mut path_len_buf).await?;
    let path_len = u16::from_be_bytes(path_len_buf) as usize;
    if path_len > MAX_REL_PATH_BYTES {
        anyhow::bail!("relative_path too long");
    }
    let mut rest_prefix = vec![0u8; path_len + 8 + 32 + 2];
    recv.read_exact(&mut rest_prefix).await?;
    let mut hdr_bytes = Vec::with_capacity(2 + rest_prefix.len() + MAX_FILE_ID_BYTES);
    hdr_bytes.extend_from_slice(&path_len_buf);
    hdr_bytes.extend_from_slice(&rest_prefix);
    // Need file_id length from end of rest_prefix
    let id_len_off = path_len + 8 + 32;
    let id_len =
        u16::from_be_bytes([rest_prefix[id_len_off], rest_prefix[id_len_off + 1]]) as usize;
    if id_len > MAX_FILE_ID_BYTES {
        anyhow::bail!("file_id too long");
    }
    let mut id_buf = vec![0u8; id_len];
    recv.read_exact(&mut id_buf).await?;
    hdr_bytes.extend_from_slice(&id_buf);
    let (header, _) = FileTransferHeader::decode(&hdr_bytes)?;
    if header.file_id.is_empty() || header.file_id.contains('\0') {
        anyhow::bail!("invalid file_id");
    }
    if header.size > max_bytes.min(MAX_FILESYNC_BYTES) {
        anyhow::bail!("remote file size {} exceeds cap", header.size);
    }
    let target = resolve_safe_target(dest_dir, &header.relative_path)?;
    let partial = {
        let mut p = target.as_os_str().to_owned();
        p.push(".qubox-partial");
        PathBuf::from(p)
    };
    {
        let mut out = tokio::fs::File::create(&partial).await?;
        let mut remaining = header.size;
        let mut buf = vec![0u8; 64 * 1024];
        let mut hasher = blake3::Hasher::new();
        while remaining > 0 {
            let chunk = remaining.min(buf.len() as u64) as usize;
            recv.read_exact(&mut buf[..chunk]).await?;
            hasher.update(&buf[..chunk]);
            out.write_all(&buf[..chunk]).await?;
            remaining -= chunk as u64;
        }
        out.sync_all().await?;
        if hasher.finalize().as_bytes() != &header.blake3 {
            let _ = tokio::fs::remove_file(&partial).await;
            anyhow::bail!("blake3 mismatch for {}", header.file_id);
        }
    }
    tokio::fs::rename(&partial, &target).await?;
    Ok((header, target))
}

/// Cap for handshake JSON (reuse MAX_JSON_FRAME).
pub fn max_handshake_frame() -> u32 {
    MAX_JSON_FRAME
}

/// Open FileSync uni + stream a local file (host or client initiator).
pub async fn push_file_over_connection(
    conn: &Connection,
    path: &Path,
    file_id: &str,
    relative_path: &str,
) -> anyhow::Result<()> {
    validate_relative_path(relative_path)?;
    if file_id.is_empty() || file_id.len() > MAX_FILE_ID_BYTES || file_id.contains('\0') {
        anyhow::bail!("invalid file_id");
    }
    let (arr, size) = hash_file_streaming(path).await?;
    let mut send = conn
        .open_uni()
        .await
        .context("open FileSync uni")?;
    write_stream_envelope(&mut send, StreamPurpose::FileSync)
        .await
        .context("FileSync envelope")?;
    let header = FileTransferHeader {
        file_id: file_id.to_string(),
        relative_path: relative_path.to_string(),
        size,
        blake3: arr,
    };
    send_file_bulk(&mut send, &header, path).await?;
    debug!(file_id, %relative_path, size, "FileSync push complete");
    Ok(())
}

/// Accept next uni stream if it is FileSync; return recv after envelope.
pub async fn accept_filesync_uni(conn: &Connection) -> anyhow::Result<Option<RecvStream>> {
    let mut recv = match conn.accept_uni().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "accept_uni ended");
            return Ok(None);
        }
    };
    let mut magic = [0u8; 2];
    use tokio::io::AsyncReadExt as _;
    recv.read_exact(&mut magic).await?;
    if magic[0] != STREAM_MAGIC {
        anyhow::bail!("non-muxed stream on FileSync accept");
    }
    if magic[1] != StreamPurpose::FileSync as u8 {
        debug!(purpose = magic[1], "skipping non-FileSync uni");
        return Ok(None);
    }
    Ok(Some(recv))
}

/// Background acceptor: write incoming bulk files under `dest_dir`.
/// Concurrent receives are capped by [`MAX_FILESYNC_CONCURRENT`].
pub async fn run_filesync_accept_loop(conn: Connection, dest_dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dest_dir);
    let slots = Arc::new(Semaphore::new(MAX_FILESYNC_CONCURRENT));
    loop {
        match accept_filesync_uni(&conn).await {
            Ok(Some(mut recv)) => {
                let dest = dest_dir.clone();
                let permit = match slots.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    match recv_file_bulk(&mut recv, &dest, MAX_FILESYNC_BYTES).await {
                        Ok((hdr, path)) => {
                            debug!(
                                file_id = %hdr.file_id,
                                path = %path.display(),
                                "FileSync received file"
                            );
                        }
                        Err(e) => warn!(error = %e, "FileSync recv failed"),
                    }
                });
            }
            Ok(None) => {
                // wrong purpose or connection closed
                if conn.close_reason().is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(e) => {
                warn!(error = %e, "FileSync accept error");
                if conn.close_reason().is_some() {
                    break;
                }
            }
        }
    }
}

/// Congestion gate for FileSync bulk (Phase C): pause when media path is saturated.
#[derive(Debug, Clone, Copy)]
pub struct FileSyncCongestionGate {
    /// Pause FileSync when estimated media bitrate exceeds this fraction of target.
    pub high_water_ratio: f64,
    /// Resume when below this fraction.
    pub low_water_ratio: f64,
    paused: bool,
}

impl Default for FileSyncCongestionGate {
    fn default() -> Self {
        Self {
            high_water_ratio: 0.85,
            low_water_ratio: 0.55,
            paused: false,
        }
    }
}

impl FileSyncCongestionGate {
    pub fn should_pause(&mut self, current_bps: u32, target_bps: u32) -> bool {
        if target_bps == 0 {
            return false;
        }
        let ratio = current_bps as f64 / target_bps as f64;
        if self.paused {
            if ratio < self.low_water_ratio {
                self.paused = false;
            }
        } else if ratio > self.high_water_ratio {
            self.paused = true;
        }
        self.paused
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

/// Wait until the congestion gate allows bulk transfer (poll interval 50ms).
pub async fn wait_for_filesync_budget(
    gate: &std::sync::Arc<tokio::sync::Mutex<FileSyncCongestionGate>>,
    sample: impl Fn() -> (u32, u32),
) {
    loop {
        let (cur, target) = sample();
        let paused = {
            let mut g = gate.lock().await;
            g.should_pause(cur, target)
        };
        if !paused {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn header_roundtrip() {
        let h = FileTransferHeader {
            file_id: "abc".into(),
            relative_path: "saves/x.sav".into(),
            size: 42,
            blake3: [9u8; 32],
        };
        let e = h.encode();
        let (d, n) = FileTransferHeader::decode(&e).unwrap();
        assert_eq!(n, e.len());
        assert_eq!(d, h);
    }

    #[test]
    fn congestion_gate_hysteresis() {
        let mut g = FileSyncCongestionGate::default();
        assert!(!g.should_pause(50, 100));
        assert!(g.should_pause(90, 100));
        assert!(g.should_pause(70, 100)); // still paused until low water
        assert!(!g.should_pause(50, 100));
    }

    #[test]
    fn validate_relative_path_rejects_traversal() {
        assert!(validate_relative_path("saves/x.sav").is_ok());
        assert!(validate_relative_path("../etc/passwd").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
        assert!(validate_relative_path("").is_err());
        assert!(validate_relative_path("a\0b").is_err());
        assert!(validate_relative_path("C:\\Windows\\x").is_err());
        assert!(validate_relative_path("foo/../../bar").is_err());
    }

    #[test]
    fn resolve_safe_target_stays_under_dest() {
        let dir = tempfile::tempdir().unwrap();
        let ok = resolve_safe_target(dir.path(), "nested/game.sav").unwrap();
        assert!(ok.starts_with(dir.path().canonicalize().unwrap()));
        assert!(resolve_safe_target(dir.path(), "../escape.sav").is_err());
    }

    #[tokio::test]
    async fn hash_file_streaming_matches_blake3() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob.bin");
        let data = vec![0x5Au8; 90_000];
        std::fs::write(&p, &data).unwrap();
        let (arr, size) = hash_file_streaming(&p).await.unwrap();
        assert_eq!(size, data.len() as u64);
        assert_eq!(&arr, blake3::hash(&data).as_bytes());
    }

    #[tokio::test]
    async fn bulk_roundtrip_via_temp_files() {
        // Unit-level path of send/recv without full QUIC: write header+body to a pipe buffer.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("game.sav");
        let data = vec![0xABu8; 128 * 1024];
        std::fs::write(&src, &data).unwrap();
        let hash = blake3::hash(&data);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(hash.as_bytes());
        let header = FileTransferHeader {
            file_id: "f-test".into(),
            relative_path: "game.sav".into(),
            size: data.len() as u64,
            blake3: arr,
        };
        // Encode header + body as a single buffer and parse with decode + body slice.
        let mut blob = header.encode();
        blob.extend_from_slice(&data);
        let (hdr, off) = FileTransferHeader::decode(&blob).unwrap();
        assert_eq!(hdr.file_id, "f-test");
        assert_eq!(&blob[off..], &data[..]);
        // atomic apply path
        let dest = dir.path().join("out");
        std::fs::create_dir_all(&dest).unwrap();
        let target = dest.join("game.sav");
        // Use qubox-sync style finalize via write+rename
        let partial = {
            let mut p = target.as_os_str().to_owned();
            p.push(".qubox-partial");
            std::path::PathBuf::from(p)
        };
        std::fs::write(&partial, &data).unwrap();
        let actual = blake3::hash(&std::fs::read(&partial).unwrap());
        assert_eq!(actual.as_bytes(), &arr);
        std::fs::rename(&partial, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), data);

        let gate = Arc::new(Mutex::new(FileSyncCongestionGate::default()));
        wait_for_filesync_budget(&gate, || (10, 100)).await;
        assert!(!gate.lock().await.is_paused());
    }

    /// Loopback e2e: real QUIC uni stream FileSync push + accept + path confine.
    #[tokio::test]
    async fn loopback_quic_filesync_push_accept() {
        use rcgen::generate_simple_self_signed;
        use rustls::pki_types::CertificateDer;
        use rustls::RootCertStore;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::Arc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("payload.bin");
        let data = vec![0xCDu8; 48_000];
        std::fs::write(&src, &data).unwrap();
        let dest = dir.path().join("incoming");
        std::fs::create_dir_all(&dest).unwrap();

        let certified = generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = CertificateDer::from(certified.cert.der().to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        let mut server_config =
            quinn::ServerConfig::with_single_cert(vec![cert_der.clone()], key.into()).unwrap();
        server_config.transport_config(Arc::new(quinn::TransportConfig::default()));
        let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut client_config = quinn::ClientConfig::with_root_certificates(Arc::new(roots)).unwrap();
        client_config.transport_config(Arc::new(quinn::TransportConfig::default()));
        let mut client_ep =
            quinn::Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).unwrap();
        client_ep.set_default_client_config(client_config);

        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.expect("accept");
            let conn = incoming.await.expect("handshake");
            let mut recv = accept_filesync_uni(&conn)
                .await
                .expect("accept filesync")
                .expect("stream");
            let (hdr, path) = recv_file_bulk(&mut recv, &dest, MAX_FILESYNC_BYTES)
                .await
                .expect("recv bulk");
            (hdr, path, dest)
        });

        let conn = client_ep
            .connect(addr, "localhost")
            .unwrap()
            .await
            .expect("client connect");
        push_file_over_connection(&conn, &src, "file-1", "nested/payload.bin")
            .await
            .expect("push");

        let (hdr, path, dest_dir) =
            tokio::time::timeout(Duration::from_secs(5), server_task)
                .await
                .expect("server timed out")
                .expect("server join");
        assert_eq!(hdr.file_id, "file-1");
        assert_eq!(hdr.relative_path, "nested/payload.bin");
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(path.starts_with(dest_dir.canonicalize().unwrap()));
    }
}
