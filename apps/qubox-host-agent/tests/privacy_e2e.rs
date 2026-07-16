//! E2E test for privacy mode blank overlay.
//!
//! Starts a host-agent with `--privacy-mode blank-overlay --enable-privacy-on-session-start`,
//! connects a client, and verifies that `ControlMsg::BlankOverlay` messages are printed
//! (indicating the host-side BlankOverlayManager is working).
//!
//! ## Setup
//!
//! ```bash
//! nohup Xephyr :99 -ac -screen 1024x768x24 -resizeable > /tmp/xephyr.log 2>&1 &
//! DISPLAY=:99 cargo test -p host-agent --test privacy_e2e -- --nocapture
//! ```

use std::{
    io::BufRead,
    process::{Command, Stdio},
    sync::atomic::AtomicBool,
    time::Duration,
};

/// Check if Xephyr :99 is available.
fn xephyr_99_available() -> bool {
    matches!(std::env::var("DISPLAY"), Ok(d) if d == ":99" || d == ":99.0")
}

fn require_e2e() -> bool {
    matches!(
        std::env::var("QUBOX_REQUIRE_E2E").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

#[test]
fn privacy_blank_overlay_e2e() {
    if !require_e2e() {
        eprintln!("SKIPPED: privacy_blank_overlay_e2e (set QUBOX_REQUIRE_E2E=1 to run)");
        return;
    }
    if !xephyr_99_available() {
        panic!("QUBOX_REQUIRE_E2E=1 but DISPLAY is not :99 (start Xephyr/Xvfb)");
    }
    eprintln!("DISPLAY :99 + QUBOX_REQUIRE_E2E; starting privacy e2e test");

    // ── Start signaling server ──
    // Use the workspace signaling-server binary (build it first).
    // Bind to port 0 to let the OS assign a random port, but we need to know
    // the port to pass it to host-agent and qubox-client-cli. Instead, try to bind
    // to a specific port with retry on collision.
    let signal_bin = which_or_build("qubox-signaling-server");
    eprintln!("qubox-signaling-server binary: {signal_bin}");

    // Use port 7000 by default; pkill any orphaned signaling servers first
    let _ = std::process::Command::new("pkill")
        .args(["-f", "qubox-signaling-server"])
        .status();
    std::thread::sleep(Duration::from_millis(300));

    let sig_port: u16 = 7000;
    let mut signaling = Command::new(&signal_bin)
        .args(["--bind", &format!("127.0.0.1:{sig_port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start signaling server");
    // Give it a moment to bind
    std::thread::sleep(Duration::from_millis(500));
    // Quick check: is signaling server still alive?
    if let Ok(Some(status)) = signaling.try_wait() {
        panic!(
            "signaling-server exited immediately with status {status}. \
             Try: pkill -f qubox-signaling-server"
        );
    }
    // Verify port is listening
    let sig_ok = std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{sig_port}").parse().unwrap(),
        Duration::from_secs(2),
    )
    .is_ok();
    assert!(
        sig_ok,
        "signaling server is not listening on port {sig_port}"
    );
    eprintln!("signaling server started on 127.0.0.1:{sig_port}");

    // ── Generate identities ──
    // Always clean up stale identity files from previous runs
    let tmp_dir = std::env::temp_dir().join("bp_privacy_e2e");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");

    let host_identity_path = tmp_dir.join("host_identity.json");
    let client_identity_path = tmp_dir.join("client_identity.json");

    let host_bin = which_or_build("host-agent");
    let client_bin = which_or_build("qubox-client-cli");
    eprintln!("host-agent binary: {host_bin}");
    eprintln!("qubox-client-cli binary: {client_bin}");

    // Create client identity (host-agent generates identity on startup automatically)
    let client_output = Command::new(&client_bin)
        .args([
            "--identity-path",
            client_identity_path.to_str().unwrap(),
            "--name",
            "test-client",
            "identity",
        ])
        .output()
        .expect("client identity");
    eprintln!(
        "client identity created: {}",
        String::from_utf8_lossy(&client_output.stdout)
    );
    let mut host = Command::new(&host_bin)
        .args([
            "--identity-path",
            host_identity_path.to_str().unwrap(),
            "--name",
            "test-host",
            "--server",
            &format!("ws://127.0.0.1:{sig_port}/ws"),
            "--x11-display",
            ":99.0",
            "--display",
            "0",
            "--stream-mode",
            "multi-display",
            "--privacy-mode",
            "blank-overlay",
            "--enable-privacy-on-session-start",
            "--max-media-frames",
            "30",
            "--auto-approve-pairing",
            "--disable-audio",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start host-agent");
    eprintln!("host-agent started (binary: {host_bin})");
    std::thread::sleep(Duration::from_millis(200));
    if let Ok(Some(status)) = host.try_wait() {
        panic!("host-agent exited immediately with status {status}");
    }

    // Start draining host stdout/stderr from the get-go to prevent pipe deadlock.
    // The blank-overlay message reader and stderr drain are set up here but not awaited yet.
    let host_stdout = host.stdout.take().expect("host stdout");
    let host_stderr = host.stderr.take().expect("host stderr");

    // Drain stderr into a shared log
    let stderr_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stderr_logger = stderr_log.clone();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(host_stderr);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(mut log) = stderr_logger.lock() {
                log.push(line);
            }
        }
    });

    // Drain stdout into a shared log (also checked for blank overlay message)
    let stdout_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let stdout_logger = stdout_log.clone();
    let found_overlay = std::sync::Arc::new(AtomicBool::new(false));
    let found_logger = found_overlay.clone();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(host_stdout);
        for line in reader.lines().map_while(Result::ok) {
            if line
                .contains("received ControlMsg::BlankOverlay { show: true, display_id: Some(0) }")
            {
                found_logger.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Ok(mut log) = stdout_logger.lock() {
                log.push(line);
            }
        }
    });

    // Give host-agent time to register on signaling server
    std::thread::sleep(Duration::from_secs(2));

    // ── Pair client with host ──
    let pair_output = Command::new(&client_bin)
        .args([
            "--identity-path",
            client_identity_path.to_str().unwrap(),
            "--server",
            &format!("ws://127.0.0.1:{sig_port}/ws"),
            "pair",
            "--host",
            "test-host",
        ])
        .output()
        .expect("failed to start qubox-client-cli pair");
    if !pair_output.status.success() {
        let stderr = String::from_utf8_lossy(&pair_output.stderr);
        eprintln!("pair stderr: {stderr}");
        let stdout = String::from_utf8_lossy(&pair_output.stdout);
        eprintln!("pair stdout: {stdout}");
    }
    assert!(
        pair_output.status.success(),
        "pairing failed: {}",
        pair_output.status
    );
    eprintln!("client paired with host");

    // ── Start qubox-client-cli in skip-window mode ──
    let mut client = Command::new(&client_bin)
        .args([
            "--identity-path",
            client_identity_path.to_str().unwrap(),
            "--server",
            &format!("ws://127.0.0.1:{sig_port}/ws"),
            "start-session",
            "--host",
            "test-host",
            "--skip-window",
            "--max-stream-frames",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start qubox-client-cli");
    eprintln!("qubox-client-cli start-session started");

    // ── Check host stdout for blank overlay show message ──
    // Already draining in background thread; check the atomic flag
    let found_show = {
        let deadline = std::time::Instant::now() + Duration::from_secs(40);
        while std::time::Instant::now() < deadline {
            if found_overlay.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        found_overlay.load(std::sync::atomic::Ordering::SeqCst)
    };

    if found_show {
        eprintln!("PASS: host printed BlankOverlay show message");
    } else {
        // Dump captured stderr on failure
        if let Ok(log) = stderr_log.lock() {
            for line in log.iter() {
                eprintln!("host stderr: {line}");
            }
        }
    }

    // Kill remaining processes (client, host, signaling) to avoid hanging
    let _ = client.kill();
    let _ = client.wait();
    let _ = host.kill();
    let _ = host.wait();
    let _ = signaling.kill();
    let _ = signaling.wait();

    // Cleanup
    let _ = std::fs::remove_dir_all(&tmp_dir);

    assert!(
        found_show,
        "Expected host to print BlankOverlay show message"
    );

    eprintln!("PASS: privacy blank overlay e2e test completed");
}

/// Find a binary in PATH or in the cargo target directory.
/// When run via `cargo test`, the test binary is in `target/debug/deps/`.
/// Built binaries are in `target/debug/`.
fn which_or_build(name: &str) -> String {
    // First check PATH
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = format!("{dir}/{name}");
            if std::path::Path::new(&candidate).exists() {
                return candidate;
            }
        }
    }
    // Walk up from the test binary's directory looking for the named binary
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().unwrap().to_path_buf();
        for _ in 0..3 {
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
            if let Some(parent) = dir.parent() {
                dir = parent.to_path_buf();
            } else {
                break;
            }
        }
    }
    name.to_string()
}

/// Check if the signaling server binary is available (e2e job only).
#[test]
fn signaling_server_available() {
    if !require_e2e() {
        return;
    }
    let path = which_or_build("qubox-signaling-server");
    assert!(
        std::path::Path::new(&path).exists(),
        "signaling-server binary not found at {path}. Build with: cargo build -p qubox-signaling-server"
    );
}
