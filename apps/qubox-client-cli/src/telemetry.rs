//! Structured NDJSON telemetry for the Qubox GUI launcher.
//!
//! When the GUI spawns `qubox-client-cli` as a subprocess, it parses
//! one JSON object per line on **stdout** (NDJSON). When the flag
//! `--json-telemetry` is **off**, the binary prints human-readable
//! text as it always has. When the flag is **on**:
//!
//! * All non-essential `println!()` output is redirected to **stderr**
//!   so the stdout stream is pure JSON.
//! * This module writes one JSON object per line via a `BufWriter` for
//!   performance.
//!
//! Every event is shaped as `{ "op": "<name>", ... }` and serialised
//! via `serde_json`. The full schema is documented in the GUI frontend
//! (see `apps/client-gui/src-tauri/src/lib.rs`).
//!
//! ## Why a global once-lock?
//!
//! Telemetry emission is hot and called from many tasks (network,
//! control stream, render loop, decoder). A `OnceLock<TelemetryEmitter>`
//! gives lock-free reads after the first call, and a single
//! `Mutex<BufWriter<Stdout>>` is held for only the duration of a
//! serialise-and-flush cycle.

use std::io::{BufWriter, Write};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

/// One telemetry event written to stdout as a single JSON line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TelemetryEvent {
    /// The client discovered a host from the signaling server.
    HostDiscovered {
        peer_id: String,
        device_name: String,
        transports: Vec<String>,
    },
    /// The client requested a pairing; carries the pairing request id.
    PairingRequested { host_id: String, request_id: String },
    /// The signaling server established a pairing.
    PairingEstablished { host_id: String, client_id: String },
    /// A session was planned by the signaling server.
    SessionPlanned {
        session_id: String,
        transport: String,
        codec: String,
        rtt_ms: u32,
    },
    /// A media frame was decoded by the local ffmpeg decoder.
    FrameDecoded {
        frame_id: u32,
        bytes: u32,
        keyframe: bool,
    },
    /// A frame was rendered to the local video window.
    FrameRendered { rendered: u64, skipped: u64 },
    /// A `ControlMsg` was received from the host.
    Control { msg: serde_json::Value },
    /// A session has ended.
    SessionEnded { reason: String },
    /// A warning or non-fatal error worth surfacing in the GUI log.
    Warn { message: String },
    /// Process is shutting down cleanly.
    Bye,
}

/// Buffers writes to stdout and exposes a thread-safe `emit` API.
struct TelemetryWriter {
    inner: Mutex<BufWriter<std::io::Stdout>>,
}

impl TelemetryWriter {
    fn new() -> Self {
        Self {
            inner: Mutex::new(BufWriter::new(std::io::stdout())),
        }
    }

    fn write(&self, event: &TelemetryEvent) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if serde_json::to_writer(&mut *guard, event).is_err() {
            return;
        };
        let _ = guard.write_all(b"\n");
        let _ = guard.flush();
    }
}

/// Process-wide telemetry state. The first call to `set_enabled(true)`
/// installs the writer. Subsequent calls are no-ops (the binary's
/// telemetry mode is decided once at startup).
static TELEMETRY: OnceLock<TelemetryWriter> = OnceLock::new();

/// Enable telemetry mode. Idempotent: subsequent calls have no effect
/// if the writer is already installed.
pub fn enable() {
    let _ = TELEMETRY.set(TelemetryWriter::new());
}

/// Returns `true` if telemetry mode is active and `emit` is a no-op
/// otherwise.
pub fn is_enabled() -> bool {
    TELEMETRY.get().is_some()
}

/// Emit a telemetry event. Silently does nothing if telemetry mode is
/// disabled, so call sites can be wired up unconditionally.
pub fn emit(event: &TelemetryEvent) {
    if let Some(writer) = TELEMETRY.get() {
        writer.write(event);
    }
}

/// Convenience for the common case of a one-shot event.
pub fn emit_value(value: serde_json::Value) {
    if let Some(writer) = TELEMETRY.get() {
        let Ok(mut guard) = writer.inner.lock() else {
            return;
        };
        if serde_json::to_writer(&mut *guard, &value).is_err() {
            return;
        };
        let _ = guard.write_all(b"\n");
        let _ = guard.flush();
    }
}

/// Helper: redirect a human-readable status line to **stderr** when
/// telemetry mode is active, otherwise print it to **stdout** as before.
/// Use this to wrap existing `println!` calls so the binary still
/// produces a useful log line in both modes.
pub fn eprintln_status(message: impl AsRef<str>) {
    let line = message.as_ref();
    if is_enabled() {
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_is_noop_when_disabled() {
        assert!(!is_enabled());
        emit(&TelemetryEvent::Bye);
    }

    #[test]
    fn serde_round_trip_host_discovered() {
        let event = TelemetryEvent::HostDiscovered {
            peer_id: "p".to_string(),
            device_name: "d".to_string(),
            transports: vec!["native_quic".to_string()],
        };
        let s = serde_json::to_string(&event).unwrap();
        assert!(s.contains("\"op\":\"host_discovered\""));
        assert!(s.contains("\"peer_id\":\"p\""));
    }
}
