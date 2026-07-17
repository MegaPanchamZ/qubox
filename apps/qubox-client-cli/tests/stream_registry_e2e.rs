//! E2E test for StreamRegistry with `--tile --show-privacy-indicator`.
//!
//! Starts a signaling-server + host-agent with `--stream-mode all-displays`,
//! then starts qubox-client-cli with `--tile --show-privacy-indicator --list-streams`.
//! Verifies the registry output contains at least one stream.
//!
//! ## Setup
//!
//! ```bash
//! nohup Xephyr :99 -ac -screen 1024x768x24 -resizeable > /tmp/xephyr.log 2>&1 &
//! DISPLAY=:99 cargo test -p qubox-client-cli --test stream_registry_e2e -- --nocapture
//! ```

use std::{
    io::BufRead,
    process::{Command, Stdio},
    time::Duration,
};

fn xephyr_99_available() -> bool {
    matches!(std::env::var("DISPLAY"), Ok(d) if d == ":99" || d == ":99.0")
}

fn require_e2e() -> bool {
    matches!(
        std::env::var("QUBOX_REQUIRE_E2E").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}

fn which_or_build(name: &str) -> String {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = format!("{dir}/{name}");
            if std::path::Path::new(&candidate).is_file() {
                return candidate;
            }
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().unwrap().to_path_buf();
        for _ in 0..5 {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
            for profile in ["debug", "release"] {
                let alt = dir.join(profile).join(name);
                if alt.is_file() {
                    return alt.to_string_lossy().to_string();
                }
            }
            if let Some(parent) = dir.parent() {
                dir = parent.to_path_buf();
            } else {
                break;
            }
        }
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let root = std::path::Path::new(&manifest)
            .ancestors()
            .nth(2)
            .unwrap_or(std::path::Path::new(&manifest));
        for profile in ["debug", "release"] {
            let candidate = root.join("target").join(profile).join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }
    panic!(
        "binary `{name}` not found in PATH or target/{{debug,release}}. \
         Build with: cargo build -p {name}"
    );
}

#[test]
fn stream_registry_tile_list_e2e() {
    // Only run in the dedicated e2e job (needs built bins + real display).
    if !require_e2e() {
        eprintln!("SKIPPED: stream_registry_tile_list_e2e (set QUBOX_REQUIRE_E2E=1 to run)");
        return;
    }
    if !xephyr_99_available() {
        panic!("QUBOX_REQUIRE_E2E=1 but DISPLAY is not :99 (start Xephyr/Xvfb)");
    }
    eprintln!("DISPLAY :99 + QUBOX_REQUIRE_E2E; starting stream registry e2e test");

    // ── Start signaling server ──
    let _ = std::process::Command::new("pkill")
        .args(["-f", "qubox-signaling-server"])
        .status();
    std::thread::sleep(Duration::from_millis(300));

    let sig_bin = which_or_build("qubox-signaling-server");
    let sig_port: u16 = 7001; // different port to avoid conflicts

    let mut signaling = Command::new(&sig_bin)
        .args(["--bind", &format!("127.0.0.1:{sig_port}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start signaling server");
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(Some(status)) = signaling.try_wait() {
        panic!("qubox-signaling-server exited immediately with status {status}");
    }

    eprintln!("signaling server started on 127.0.0.1:{sig_port}");

    // ── Generate identities ──
    let tmp_dir = std::env::temp_dir().join("bp_stream_registry_e2e");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).expect("create temp dir");

    let host_identity_path = tmp_dir.join("host_identity.json");
    let client_identity_path = tmp_dir.join("client_identity.json");

    let host_bin = which_or_build("qubox-host-agent");
    let client_bin = which_or_build("qubox-client-cli");

    // Create client identity
    let _ = Command::new(&client_bin)
        .args([
            "--identity-path",
            client_identity_path.to_str().unwrap(),
            "--name",
            "test-client",
            "identity",
        ])
        .output()
        .expect("client identity");

    // ── Start host-agent ──
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
            "--max-media-frames",
            "30",
            "--auto-approve-pairing",
            "--disable-audio",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start host-agent");
    std::thread::sleep(Duration::from_millis(500));
    if let Ok(Some(status)) = host.try_wait() {
        panic!("host-agent exited immediately with status {status}");
    }
    eprintln!("host-agent started");

    // Give host-agent time to register
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
        .expect("pair");
    assert!(
        pair_output.status.success(),
        "pairing failed: {}",
        pair_output.status
    );
    eprintln!("client paired with host");

    // ── Start qubox-client-cli with --tile --show-privacy-indicator --list-streams ──
    let mut client = Command::new(&client_bin)
        .args([
            "--identity-path",
            client_identity_path.to_str().unwrap(),
            "--server",
            &format!("ws://127.0.0.1:{sig_port}/ws"),
            "start-session",
            "--host",
            "test-host",
            "--tile",
            "--show-privacy-indicator",
            "--list-streams",
            "--max-stream-frames",
            "30",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("qubox-client-cli");
    eprintln!("qubox-client-cli started with --tile --show-privacy-indicator --list-streams");

    // ── Read client stdout for the stream registry table ──
    let stdout = client.stdout.take().expect("client stdout");
    let reader = std::io::BufReader::new(stdout);
    let mut found_table = false;
    let mut display_count = 0;

    for line in reader.lines().map_while(Result::ok) {
        if line.contains("display_id") && line.contains("size") {
            found_table = true;
        }
        if found_table && line.contains('|') && !line.contains('-') && !line.contains("display_id")
        {
            // Count lines with display_id data
            display_count += 1;
        }
    }

    let _ = client.wait();
    let _ = host.kill();
    let _ = host.wait();
    let _ = signaling.kill();
    let _ = signaling.wait();
    let _ = std::fs::remove_dir_all(&tmp_dir);

    assert!(
        found_table,
        "Expected stream registry table in client output"
    );
    assert!(
        display_count >= 1,
        "Expected at least 1 stream in registry, got {display_count}"
    );
    eprintln!("PASS: stream registry e2e test — found {display_count} stream(s)");
}
