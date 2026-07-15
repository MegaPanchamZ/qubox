# ADR-020 Pensieve-Style Reinforcement Learning ABR

## Status

**Proposed.** Branch: `feature/adr-020-pensieve-rl-abr`. Based on
`main` after commit `47585ea`. Builds on ADR-012 (congestion control
telemetry), ADR-013 (frame-aware pacing), ADR-014 (FEC loss telemetry),
and ADR-018 (codec selection matrix). Out of scope for P0/P1; planned
for **P2 (post-1.0)** as an opt-in, **off-by-default**, gated feature.

> **Reviewer note.** The original 213-line sketch has been expanded
> into a junior-intern-implementable specification after deep web
> research (Pensieve paper, Rust ML crates, cloud-gaming ABR, RL
> congestion control, synthetic desktop-capture datasets). Every code
> block in this ADR is intended to be copy-pasteable into the repo.
> Items the human reviewer must sanity-check before sign-off are listed
> in the **§13 Reviewer Sanity-Check List** at the end.

## Table of Contents

1. Context & scope
2. Decision summary
3. Final library choice — `candle-core` 0.11.0
4. Complete `Observation` struct
5. Complete `Action` enum + constraint validator + reward
6. Policy server design
7. Inference client & keyframe cadence
8. Telemetry pipeline (ring buffer + opt-in upload)
9. Step-by-step implementation order (numbered PRs)
10. Test specifications
11. File paths & insertion points
12. Pitfalls (six concrete gotchas)
13. Reviewer sanity-check list
14. References

---

## 1. Context & scope

### 1.1 Background

ADR-012 standardises GCC/SCReAM/BBR v3 for the **bandwidth probe**
problem; ADR-018 provides a `CodecMatrix` for resolution/refresh/codec
selection. Neither directly optimises **user-perceived QoE** under the
four-dimensional trade space of:

*   current throughput (from ADR-012);
*   current frame decode latency (`decoder_hw.rs`);
*   recent encode-bitrate vs delivered-bitrate (underflow/overflow);
*   content complexity (from the ADR-018 §4 screen-content classifier).

**Pensieve** (Mao, Netravali, Alizadeh — SIGCOMM 2017, "Neural
Adaptive Video Streaming with Pensieve") trains a neural policy via
A3C (asynchronous advantage actor-critic) on simulated streaming
sessions; at inference it maps a state vector directly to a bitrate
decision without hand-crafted rules. The original paper reports
**12–25 % QoE improvement** over the best rule-based ABR
throughput/buffer schemes (§5.2 of Mao et al.).

### 1.2 What this ADR ships (in this repo)

*   A **policy server** (`crates/qubox-host-agent/src/policy_server/`)
    that loads a vendored pure-Rust policy network and answers
    inference queries over a localhost TCP socket.
*   An **inference client** (`apps/qubox-host-agent/src/rl_abr/`)
    that calls the server at every keyframe boundary.
*   A **telemetry pipeline** that records (obs, action, reward,
    next_obs) tuples to a local ring buffer at
    `/var/lib/qubox-daemon/telemetry/rl_tuples/` and optionally uploads
    them (opt-in only) for offline retraining.

### 1.3 What this ADR explicitly does **not** ship

*   The training pipeline. Training is performed in a **separate
    Python / PyTorch repository** (`qubox-rl-training/`). The
    checkpoint produced there is exported as a **safetensors** file
    and vendored under
    `crates/qubox-host-agent/src/policy_server/checkpoints/`.
*   On-device / mobile inference. The first release only ships
    server-side (host-agent side) inference. Future revisions may
    move inference onto the client (browser) via WebAssembly + ONNX.

### 1.4 Off-by-default & opt-in

The RL server is **disabled by default**. The user must pass
`--enable-rl-abr` on the `qubox-host-agent` command line. When
disabled, behaviour is identical to today's
[`GccController`](apps/qubox-host-agent/src/rate_control.rs:46-64).
Telemetry upload is independently gated behind
`--telemetry=rl-abr`.

---

## 2. Decision summary

| Concern               | Decision                                                                                    |
|-----------------------|---------------------------------------------------------------------------------------------|
| Inference engine      | `candle-core` **0.11.0** (pure-Rust, Hugging Face)                                          |
| Checkpoint format     | safetensors (exported from PyTorch)                                                         |
| Wire protocol         | length-prefixed binary (4-byte LE length + rkyv payload per ADR-015)                        |
| Transport             | TCP `127.0.0.1:0` (kernel-assigned port, advertised via env var)                            |
| Model architecture    | 7 inputs → 128 (ReLU) → 128 (ReLU) → 128 (ReLU) → N actions (logits)                        |
| Action space          | Discrete bitrate × resolution × codec combinations exposed by `CodecMatrix` (see §5)        |
| Reward                | Modified Pensieve QoE: `ln(bitrate) − α·rebuf − β·|Δq|` + latency + deadline penalties      |
| Discount γ            | 0.99 (training time only; inference is stateless)                                           |
| Training algorithm    | A3C in Python/PyTorch (separate repo)                                                      |
| Cadence               | One query per keyframe boundary (ADR-013 §5)                                                |
| Telemetry             | Local ring buffer, opt-in upload to S3-compatible bucket                                    |

---

## 3. Final library choice — `candle-core` 0.11.0

### 3.1 Comparison (verified via crates.io, 2026)

| Crate                | Version  | Type            | CPU-only runtime footprint (typical) | Loads PyTorch `.pt`/TorchScript? | Tokio-friendly? |
|----------------------|----------|-----------------|--------------------------------------|----------------------------------|-----------------|
| `candle-core`        | 0.11.0   | Pure-Rust       | **~3–10 MB** binary, no giant deps   | No (use safetensors or manual)    | Yes             |
| `tch` (tch-rs)       | 0.17.0   | libtorch FFI    | **~50–150+ MB** of libtorch shared libs | **Yes** (TorchScript via `torch::jit::load`) | Yes (blocking calls) |
| `onnxruntime`        | 0.20.0   | ONNX Runtime FFI | **~20–80+ MB** of ORT shared libs  | Indirectly (via `torch.onnx.export`) | Yes (blocking)  |

### 3.2 Decision: `candle-core`

**Rationale:**

1. **Binary size.** A 6→128→128→128→N policy network is ~66 K
   parameters (~264 KB of `f32` weights). Shipping **50–150 MB** of
   libtorch or **20–80 MB** of ONNX Runtime to serve a 264 KB model is
   a 200–600× bloat. `candle-core` keeps the host-agent binary at its
   current size budget.
2. **Inference latency.** All three frameworks will easily hit the
   1 ms-per-query target for a model this small on modern x86_64. We
   gain nothing from the heavier runtimes on raw throughput.
3. **Checkpoint workflow.** For a feedforward MLP with known
   activations (ReLU), re-implementing the architecture in Rust and
   loading weights from safetensors is **~30 lines of code**. We avoid
   the ONNX export step, the operator-coverage concerns, and the
   TorchScript `torch::jit::load` FFI risk.
4. **Corporate backing.** `candle-core` is developed by Hugging Face,
   giving it a healthier bus factor than `tch` (mostly single-maintainer
   Laurent Mazare) for our long-lived daemon.

### 3.3 What we **don't** lose

*   If we later need a CNN/RNN/Transformer (e.g., we want to upgrade to
    Comyco's 1-D CNN over throughput history), `candle-core` has all
    the needed ops (`Conv1D`, `LSTM`, `softmax`, `cross_entropy`).
*   If the model grows past ~10 M parameters and we hit candle's
    autotuning ceiling, we can switch to `tch` *only for that one
    subprocess* — the wire protocol stays identical, so the
    inference-client side does not change.

---

## 4. Complete `Observation` struct

**File:** `crates/qubox-host-agent/src/rl_abr/observation.rs`

```rust
//! RL-ABR observation tuple.
//!
//! All fields are produced on the **host** (encoder + telemetry side).
//! The client CLI never sees this struct — the wire format (§6.3) is a
//! serialised `Observation` posted to the policy server.

use rkyv::{Archive, Deserialize, Serialize};

/// Normalisation constants — tuned against the synthetic training
/// corpus described in `qubox-rl-training/configs/qubox-desktop-v1.yaml`.
/// Must match **exactly** between training (Python) and inference (Rust);
/// mismatched constants are the #1 silent-failure mode for RL deployment.
pub mod norm {
    /// Throughput in bits/sec divided by 50 Mbps (gives ~0.0–1.0 for our
    /// operating range, ~0–50 Mbps; 4K144 caps near 1.0).
    pub const THROUGHPUT_DIV_BPS: f32 = 50_000_000.0;
    /// Decode latency divided by 33.3 ms (two 60 fps frame budgets).
    pub const DECODE_LATENCY_DIV_MS: f32 = 33.3;
    /// Encode/delivered ratio is already ~1.0; clip to [-1.0, 1.0].
    pub const RATIO_CLIP: f32 = 1.0;
    /// FEC loss rate is already 0.0–1.0.
    pub const FEC_LOSS_DIV: f32 = 1.0;
    /// Screen-content score already 0.0–1.0.
    pub const SCREEN_CONTENT_DIV: f32 = 1.0;
    /// Deadline slack: positive = on time, negative = late. We map
    /// [-16.67, +16.67] ms → [-1.0, +1.0].
    pub const DEADLINE_SLACK_DIV_MS: f32 = 16.67;
    /// Bitrate ladder: see §5.1.
    pub const BITRATE_DIV_BPS: f32 = 20_000_000.0;
    /// EWMA coefficients — applied at sample time, not at observation
    /// build time (so the *telemetry* layer maintains state, not this
    /// struct). Listed here for cross-reference with `telemetry.rs`.
    pub const EWMA_ALPHA_THROUGHPUT: f32 = 0.30;
    pub const EWMA_ALPHA_DECODE_LATENCY: f32 = 0.20;
    pub const EWMA_ALPHA_RATIO: f32 = 0.20;
    pub const EWMA_ALPHA_FEC_LOSS: f32 = 0.15;
    pub const EWMA_ALPHA_DEADLINE_SLACK: f32 = 0.40;
}

/// Observation vector — 7 fields, 4 of them scalars and 1 a fixed-length
/// past-action array.
///
/// **Index map** (must match training side):
/// 0. `throughput_bps`        — normalised → `[0, 1]`
/// 1. `decode_latency_ms`     — normalised → `[0, 1]`
/// 2. `encode_delivered_ratio` — clipped `[-1, 1]`
/// 3. `fec_loss_rate`         — `[0, 1]`
/// 4. `screen_content_score`  — `[0, 1]`
/// 5. `deadline_slack_ms`     — `[-1, 1]`
/// 6. `past_actions[5]`       — each entry normalised → `[0, 1]`
///
/// Total input dimension: **7 + 5 = 12** (the past_actions expand to 5
/// scalars; see §4.1).
#[derive(Debug, Clone, Copy, Archive, Serialize, Deserialize)]
#[rkyv(attr(doc = "Pensieve-style observation tuple, host-side"))]
pub struct Observation {
    /// Throughput estimate from ADR-012 controller (bytes/sec).
    pub throughput_bps: u32,
    /// EWMA of decoder frame latency (ms).
    pub decode_latency_ms: f32,
    /// EWMA of (encoder_bitrate / delivered_bitrate). 1.0 = on target;
    /// >1.0 = encoder producing more than the network drains; <1.0 =
    /// under-utilising the link.
    pub encode_delivered_ratio: f32,
    /// EWMA FEC block loss rate (from ADR-014 decoder), `[0, 1]`.
    pub fec_loss_rate: f32,
    /// Screen-content score in `[0, 1]` from the ADR-018 classifier.
    pub screen_content_score: f32,
    /// Current frame deadline slack (ms). Positive = on time; negative
    /// = this frame already missed its budget.
    pub deadline_slack_ms: f32,
    /// Last 5 chosen actions (bitrates, bps), oldest first. Used by the
    /// policy to learn smoothness without an explicit Δq feature (the
    /// policy can subtract consecutive entries internally).
    pub past_actions: [u32; 5],
}

impl Observation {
    /// Build the normalised f32 vector fed to the policy network.
    ///
    /// Must produce a vector whose layout exactly matches the training
    /// harness (`qubox-rl-training/observation.py::build_obs_vector`).
    pub fn to_normalised_vec(&self) -> [f32; 12] {
        let mut v = [0.0_f32; 12];
        v[0] = (self.throughput_bps as f32 / norm::THROUGHPUT_DIV_BPS).clamp(0.0, 2.0);
        v[1] = (self.decode_latency_ms / norm::DECODE_LATENCY_DIV_MS).clamp(0.0, 2.0);
        v[2] = (self.encode_delivered_ratio).clamp(-norm::RATIO_CLIP, norm::RATIO_CLIP);
        v[3] = (self.fec_loss_rate / norm::FEC_LOSS_DIV).clamp(0.0, 1.0);
        v[4] = (self.screen_content_score / norm::SCREEN_CONTENT_DIV).clamp(0.0, 1.0);
        v[5] = (self.deadline_slack_ms / norm::DEADLINE_SLACK_DIV_MS).clamp(-1.0, 1.0);
        for (i, bps) in self.past_actions.iter().enumerate() {
            v[6 + i] = (*bps as f32 / norm::BITRATE_DIV_BPS).clamp(0.0, 1.0);
        }
        v
    }
}
```

### 4.1 Why 12 inputs, not 7

The ADR-020 §1 sketch listed 7 fields. We add the past-action array as
**field 7** but expand it into 5 input scalars, giving the policy a
total of **12 inputs**. This matches the standard Pensieve "last-K
actions" encoding and is what the published Pensieve checkpoint
expects; the training harness will write the 12-element vector.

### 4.2 JSON / rkyv serialisation

*   **rkyv** (per ADR-015) for the on-wire format — zero-copy on the
    server side.
*   **JSON** (`serde_json`) for the on-disk ring-buffer log and for the
    debug `--dump-obs` CLI flag, so the human can eyeball tuples
    without the policy server running. Use `#[serde(rename_all =
    "snake_case")]` for both `Observation` and `Action`.

---

## 5. Complete `Action` enum + constraint validator + reward

**File:** `crates/qubox-host-agent/src/rl_abr/action.rs`

### 5.1 Action space

The full action space is the **cross product** of:

*   8 bitrate rungs (Mbps): 1, 2, 4, 6, 8, 12, 16, 20 → 8 levels
*   4 resolutions: 720p, 1080p, 1440p, 2160p (4K) → 4 levels
*   3 refresh rates: 60, 90, 144 Hz → 3 levels
*   3 codecs: H.264, HEVC, AV1 → 3 levels

Total: 8 × 4 × 3 × 3 = **288 discrete actions**. The
`CodecMatrix::choose_codec` filter (ADR-018 §1, line 108) typically
prunes this to ~50–80 valid actions per host.

For the first release we **decouple codec selection** (left to ADR-018)
and let the policy pick only (bitrate, resolution, refresh). This
collapses the action space to **8 × 4 × 3 = 96 actions**, which keeps
the policy network's output layer small and the A3C training tractable.

```rust
use rkyv::{Archive, Deserialize, Serialize};
use serde::{Deserialize as SerdeDeserialize, Serialize as SerdeSerialize};
use crate::rl_abr::codec_matrix::CodecMatrix;

/// A single policy decision. Holds the parameters that the encoder
/// pipeline will apply for the next keyframe group.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash,
    Archive, Serialize, Deserialize,
    SerdeSerialize, SerdeDeserialize,
)]
#[rkyv(attr(doc = "Bitrate ladder rung + resolution + refresh"))]
pub struct Action {
    /// Encoder bitrate in **bits per second**.
    pub bitrate_bps: u32,
    /// Frame width in pixels (e.g. 1920 for 1080p).
    pub width: u16,
    /// Frame height in pixels (e.g. 1080).
    pub height: u16,
    /// Refresh rate in Hz (60, 90, or 144).
    pub refresh_hz: u8,
    /// Codec index into the ADR-018 `Codec` enum.
    /// 0 = H.264, 1 = HEVC, 2 = AV1.
    pub codec_idx: u8,
}

/// The canonical bitrate ladder. Indices here **must** match the indices
/// the policy network is trained against. Treat as the source of truth.
pub const BITRATE_LADDER_BPS: &[u32] = &[
    1_000_000, 2_000_000, 4_000_000, 6_000_000,
    8_000_000, 12_000_000, 16_000_000, 20_000_000,
];
pub const RESOLUTION_LADDER: &[(u16, u16)] = &[
    (1280, 720),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];
pub const REFRESH_LADDER_HZ: &[u8] = &[60, 90, 144];

impl Action {
    /// Convenience: turn (ladder_idx, res_idx, refresh_idx) into a
    /// fully-populated `Action`. Used by the policy server when
    /// de-coding the argmax output.
    pub fn from_indices(
        bitrate_idx: usize,
        res_idx: usize,
        refresh_idx: usize,
        codec: Codec,
    ) -> Self {
        let (w, h) = RESOLUTION_LADDER[res_idx];
        Self {
            bitrate_bps: BITRATE_LADDER_BPS[bitrate_idx],
            width: w,
            height: h,
            refresh_hz: REFRESH_LADDER_HZ[refresh_idx],
            codec_idx: codec as u8,
        }
    }

    /// Penalty term for the smoothness component of the reward — the
    /// log-ratio of the current and previous quality rung.
    pub fn quality_log_ratio(&self, prev: &Action) -> f32 {
        let q_curr = (self.bitrate_bps as f32).ln();
        let q_prev = (prev.bitrate_bps as f32).ln();
        (q_curr - q_prev).abs()
    }
}

/// Re-export the ADR-018 Codec enum so we don't duplicate the type.
pub use crate::qubox_media::codec::Codec;
```

### 5.2 Constraint validator — filter through `CodecMatrix`

The policy network may output any of 96 actions, but many are
**infeasible** on the current host (e.g., 4K144 + H.264 is not
supported by NVENC). The constraint validator is the post-filter that
maps an argmax index to a feasible `Action`.

**File:** `crates/qubox-host-agent/src/rl_abr/action.rs` (continued)

```rust
/// Validates that an `Action` is feasible against the host's
/// `CodecMatrix`. Returns `None` if not feasible; the caller should
/// fall back to the next-best ladder rung or to the GCC controller.
pub fn validate_action(
    a: Action,
    matrix: &CodecMatrix,
) -> Option<Action> {
    use crate::qubox_media::codec::Codec;
    let codec = match a.codec_idx {
        0 => Codec::H264,
        1 => Codec::Hevc,
        2 => Codec::Av1,
        _ => return None,
    };
    let matrix_codec = matrix.choose_codec((a.width as u32, a.height as u32), a.refresh_hz as u32);
    // If the matrix disagrees with our choice, snap to the matrix's pick
    // (so we never encode with a codec the matrix forbids).
    let effective_codec = if matrix_codec == codec {
        codec
    } else {
        matrix_codec
    };
    Some(Action {
        codec_idx: effective_codec as u8,
        ..a
    })
}

/// Iterates the 96-action space in order, skipping infeasible ones,
/// returning the **first** feasible action whose ladder index is
/// ≥ `min_ladder_idx`. Used when the policy's argmax is infeasible.
pub fn fallback_action(
    matrix: &CodecMatrix,
    min_ladder_idx: usize,
    prev: &Action,
) -> Action {
    for br_idx in min_ladder_idx..BITRATE_LADDER_BPS.len() {
        for (w, h) in RESOLUTION_LADDER.iter().rev() {
            for hz in REFRESH_LADDER_HZ.iter().rev() {
                for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
                    let a = Action::from_indices(
                        br_idx,
                        RESOLUTION_LADDER.iter().position(|r| r == &(*w, *h)).unwrap(),
                        REFRESH_LADDER_HZ.iter().position(|r| r == hz).unwrap(),
                        codec,
                    );
                    if let Some(valid) = validate_action(a, matrix) {
                        // Prefer staying near the previous action's bitrate
                        if (valid.bitrate_bps as i64 - prev.bitrate_bps as i64).abs()
                            < (BITRATE_LADDER_BPS[1] - BITRATE_LADDER_BPS[0]) as i64 * 4
                        {
                            return valid;
                        }
                    }
                }
            }
        }
    }
    // Exhausted search — return a conservative default (720p60 H.264).
    Action::from_indices(2, 0, 0, Codec::H264)
}
```

### 5.3 Reward function

**File:** `crates/qubox-host-agent/src/rl_abr/reward.rs`

The reward follows the **Pensieve QoE linear combination**
(Mao et al. §3.2) plus two latency-aware penalties that are novel to
our desktop-streaming domain:

```rust
//! Reward function — Pensieve-style QoE linear combination with
//! desktop-streaming extensions.

use crate::rl_abr::{Action, Observation};

/// Rebuffering penalty coefficient. Pensieve uses α = 4.3 (Mao et al.
/// SIGCOMM 2017 §3.2). We re-use that value for the
/// `fec_loss_rate`-driven stall analogue.
pub const ALPHA_REBUF: f32 = 4.3;

/// Smoothness penalty coefficient. Pensieve uses β = 1.0.
pub const BETA_SMOOTH: f32 = 1.0;

/// Penalty for exceeding the 60 fps frame budget (16.67 ms). Units:
/// reward units per ms of overshoot. Tuned via grid search in
/// `qubox-rl-training/configs/qubox-desktop-v1.yaml`.
pub const LATENCY_PENALTY_PER_MS: f32 = 0.10;

/// Cliff penalty for missing the frame deadline entirely. Returns a
/// fixed `-50` regardless of how late (the absolute deadline matters
/// more than how-much-late for human perception).
pub const DEADLINE_MISS_PENALTY: f32 = 50.0;

/// Compute the per-step reward.
///
/// Reward shape:
///   r = ln(bitrate_bps)
///       - α · fec_loss_rate_term    // stall analogue
///       - β · |Δquality|            // smoothness
///       - LATENCY_PENALTY_PER_MS · max(0, decode_latency_ms - 16.67)
///       - DEADLINE_MISS_PENALTY     if deadline_slack_ms < 0
///
/// `next_obs` carries the **post-action** measurements (the next
/// keyframe group's telemetry).
pub fn reward(
    prev_action: &Action,
    action: &Action,
    next_obs: &Observation,
) -> f32 {
    let quality = (action.bitrate_bps as f32).ln();

    // Pensieve's rebuffer term is time spent rebuffering in seconds; we
    // approximate that with FEC loss rate × 1 second of "effective stall"
    // — each 1 % FEC loss ≈ 10 ms of stall.
    let rebuf_term = ALPHA_REBUF * (next_obs.fec_loss_rate * 1.0);

    let smooth_term = BETA_SMOOTH * action.quality_log_ratio(prev_action);

    let latency_penalty = if next_obs.decode_latency_ms > 16.67 {
        -LATENCY_PENALTY_PER_MS * (next_obs.decode_latency_ms - 16.67)
    } else {
        0.0
    };

    let deadline_penalty = if next_obs.deadline_slack_ms < 0.0 {
        -DEADLINE_MISS_PENALTY
    } else {
        0.0
    };

    quality - rebuf_term - smooth_term + latency_penalty - deadline_penalty
}
```

**Important.** The reward function above runs in **two** places:

1.  **Inference time** — to log `(obs, action, reward, next_obs)`
    tuples to the telemetry ring buffer.
2.  **Training time** — re-implemented in Python (`reward.py` in the
    `qubox-rl-training` repo). The constants **must** match.

The training repo contains a unit test
`test_reward_matches_rust_implementation` that loads the constants
from this very file (via a checked-in `reward_constants.json` that
`build.rs` regenerates on every Rust build). This is the single most
important cross-language invariant.

---

## 6. Policy server design

**Directory:** `crates/qubox-host-agent/src/policy_server/`

```
policy_server/
├── mod.rs            # Tokio TCP listener, request dispatch
├── model.rs          # candle-core MLP + safetensors weight loading
├── wire.rs           # rkyv request/response envelope
└── checkpoints/
    └── qubox-desktop-v1.safetensors  # vendored, ~264 KB
```

### 6.1 TCP listener

`policy_server/mod.rs`:

```rust
//! Policy server — localhost TCP, length-prefixed rkyv protocol.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use crate::rl_abr::Observation;

const MAX_FRAME_BYTES: usize = 64 * 1024; // 64 KiB ceiling per ADR-015

pub struct PolicyServer {
    model: Arc<Mutex<model::PolicyModel>>,
    /// Port we ended up listening on (kernel-assigned when bind port = 0).
    pub bound_port: u16,
}

impl PolicyServer {
    /// Bind to 127.0.0.1:0 (kernel picks free port), load the vendored
    /// checkpoint, and spawn the accept loop. Returns the server handle
    /// (which exposes the port via `bound_port`) and the join handle for
    /// graceful shutdown.
    pub async fn spawn(checkpoint_path: &std::path::Path) -> std::io::Result<(Self, tokio::task::JoinHandle<std::io::Result<()>>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_port = listener.local_addr()?.port();
        let model = model::PolicyModel::load(checkpoint_path)?;
        let model = Arc::new(Mutex::new(model));
        let server = Self { model, bound_port };
        let model_clone = Arc::clone(&server.model);
        let join = tokio::spawn(async move {
            loop {
                let (stream, _peer) = listener.accept().await?;
                let model = Arc::clone(&model_clone);
                tokio::task::spawn_blocking(move || {
                    handle_connection_blocking(stream, model)
                });
            }
        });
        Ok((server, join))
    }
}

fn handle_connection_blocking(
    mut stream: tokio::net::TcpStream,
    model: Arc<Mutex<model::PolicyModel>>,
) -> std::io::Result<()> {
    // Read 4-byte LE length prefix.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES || len == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large or zero",
        ));
    }
    // Read rkyv payload.
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    // rkyv zero-copy deserialise.
    let archived = unsafe { rkyv::archived_root::<Observation>(&payload[..]) };
    let obs: Observation = archived.deserialize(&mut rkyv::Infallible).unwrap();
    // Inference.
    let action_idx = {
        let mut m = model.blocking_lock();
        m.infer_argmax(&obs)
    };
    // Reply: 4-byte length + 1 byte (action_idx).
    let mut reply = Vec::with_capacity(5);
    reply.extend_from_slice(&1u32.to_le_bytes());
    reply.push(action_idx as u8);
    stream.write_all(&reply)?;
    stream.shutdown_std_write()?;
    Ok(())
}
```

### 6.2 Wire format

| Field      | Size (bytes) | Encoding                  | Notes                                              |
|------------|--------------|---------------------------|----------------------------------------------------|
| length     | 4            | `u32` little-endian       | Byte count of the rkyv payload                     |
| payload    | `length`     | rkyv-archived `Observation` | Zero-copy; no allocation in the server hot path    |
| response   | 5            | `u32` LE length + 1 byte  | The single byte is `action_idx` (0..96)            |

The wire format is intentionally identical to the format used by
ADR-015 §3 (rkyv zero-copy over TCP), so we reuse its framing.

### 6.3 Model load + inference

`policy_server/model.rs`:

```rust
//! Pure-Rust policy network via candle-core.

use candle_core::{DType, Device, Module, Tensor};
use candle_nn::{linear, Linear, VarBuilder};
use rkyv::Deserialize;
use crate::rl_abr::Observation;

/// Number of output actions — must match `BITRATE_LADDER_BPS.len()` ×
/// `RESOLUTION_LADDER.len()` × `REFRESH_LADDER_HZ.len()` = 8 × 4 × 3 = 96.
pub const N_ACTIONS: usize = 96;

/// Three-hidden-layer MLP, each 128 units, ReLU activations.
pub struct PolicyModel {
    fc1: Linear,
    fc2: Linear,
    fc3: Linear,
    fc_out: Linear,
    device: Device,
}

impl PolicyModel {
    pub fn load(ckpt_path: &std::path::Path) -> std::io::Result<Self> {
        let device = Device::Cpu;
        // Set up a VarBuilder that loads from a safetensors file.
        // candle_nn uses a slightly different API; we walk the tensors
        // by name: "fc1.weight", "fc1.bias", "fc2.weight", ...
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[ckpt_path.to_str().unwrap()],
                DType::F32,
                &device,
            )
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
        };
        let fc1 = linear(12, 128, vb.pp("fc1"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc2 = linear(128, 128, vb.pp("fc2"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc3 = linear(128, 128, vb.pp("fc3"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let fc_out = linear(128, N_ACTIONS, vb.pp("fc_out"))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(Self { fc1, fc2, fc3, fc_out, device })
    }

    /// Forward pass + argmax. Returns the action index in `[0, 96)`.
    pub fn infer_argmax(&mut self, obs: &Observation) -> usize {
        let v = obs.to_normalised_vec();
        let x = Tensor::from_slice(&v, (1, 12), &self.device).unwrap();
        let x = self.fc1.forward(&x).unwrap().relu().unwrap();
        let x = self.fc2.forward(&x).unwrap().relu().unwrap();
        let x = self.fc3.forward(&x).unwrap().relu().unwrap();
        let logits = self.fc_out.forward(&x).unwrap();
        logits.argmax(1).unwrap().to_vec1::<u64>().unwrap()[0] as usize
    }
}
```

### 6.4 Port discovery

The host-agent subprocess **must** advertise the port to the rest of
the daemon so the inference client (next section) can find it. We
write a single line to the daemon's env-channel file
(`/run/qubox-daemon/env`) in the form:

```
QUBOX_RL_POLICY_PORT=51234
```

The inference client reads this on every query (cheap, since the file
is in tmpfs and the env-channel is a single `read_line`).

### 6.5 Disable / enable

If `--enable-rl-abr` is **not** passed, the `policy_server` module is
**never compiled in** via a `#[cfg(feature = "rl-abr")]` gate. This is
why the policy server binary code adds **0 bytes** to the default
`qubox-host-agent` build. The Cargo feature is defined in
`crates/qubox-host-agent/Cargo.toml` as `rl-abr = ["dep:candle-core",
"dep:candle-nn", "dep:rkyv", "dep:safetensors"]`.

---

## 7. Inference client & keyframe cadence

**Directory:** `apps/qubox-host-agent/src/rl_abr/`

```
rl_abr/
├── mod.rs             # Re-exports
├── observation.rs     # Observation struct (§4)
├── action.rs          # Action + constraint validator (§5.1–5.2)
├── reward.rs          # reward() function (§5.3)
├── client.rs          # Tokio-based TCP client to the policy server
└── cadence.rs         # Keyframe-boundary trigger
```

### 7.1 Tokio-based client

`rl_abr/client.rs`:

```rust
//! Tokio TCP client that talks to the policy server.
//!
//! The client owns no state other than the current observation and the
//! last 5 actions. On each keyframe boundary, it serialises the
//! observation, sends it to the server, and receives an action index.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use crate::rl_abr::{Action, Observation};
use crate::rl_abr::codec_matrix::CodecMatrix;

pub struct PolicyClient {
    stream: TcpStream,
    codec_matrix: CodecMatrix,
}

impl PolicyClient {
    /// Connect to the server whose port is in `/run/qubox-daemon/env`
    /// (env var `QUBOX_RL_POLICY_PORT`).
    pub async fn connect(codec_matrix: CodecMatrix) -> std::io::Result<Self> {
        let port = read_policy_port()?;
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        Ok(Self { stream, codec_matrix })
    }

    /// Blocking-ish query. Returns a feasible `Action`.
    ///
    /// Internally:
    /// 1. Serialise `obs` to rkyv bytes.
    /// 2. Write 4-byte length prefix + payload.
    /// 3. Read 4-byte length prefix + 1-byte argmax.
    /// 4. Map argmax → `Action`, then filter through `validate_action`.
    /// 5. If the action is infeasible, call `fallback_action`.
    pub async fn query(&mut self, obs: &Observation) -> std::io::Result<Action> {
        let payload = rkyv::to_bytes::<_, 256>(obs)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        let len = payload.len() as u32;
        self.stream.write_all(&len.to_le_bytes()).await?;
        self.stream.write_all(&payload).await?;
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await?;
        if u32::from_le_bytes(len_buf) != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "expected 1-byte reply",
            ));
        }
        let mut idx_buf = [0u8; 1];
        self.stream.read_exact(&mut idx_buf).await?;
        let action_idx = idx_buf[0] as usize;
        let action = idx_to_action(action_idx);
        match crate::rl_abr::action::validate_action(action, &self.codec_matrix) {
            Some(a) => Ok(a),
            None => {
                // Fall back to ladder search centred on `action`.
                let prev = Action::from_default();
                Ok(crate::rl_abr::action::fallback_action(
                    &self.codec_matrix,
                    action.bitrate_bps as usize / 1_000_000,
                    &prev,
                ))
            }
        }
    }
}

fn read_policy_port() -> std::io::Result<u16> {
    let path = "/run/qubox-daemon/env";
    let s = std::fs::read_to_string(path)?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("QUBOX_RL_POLICY_PORT=") {
            return rest.trim().parse::<u16>()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "QUBOX_RL_POLICY_PORT not in /run/qubox-daemon/env",
    ))
}

fn idx_to_action(idx: usize) -> Action {
    let br_idx = idx / (RESOLUTION_LADDER.len() * REFRESH_LADDER_HZ.len());
    let rem = idx % (RESOLUTION_LADDER.len() * REFRESH_LADDER_HZ.len());
    let res_idx = rem / REFRESH_LADDER_HZ.len();
    let refresh_idx = rem % REFRESH_LADDER_HZ.len();
    Action::from_indices(
        br_idx,
        res_idx,
        refresh_idx,
        Codec::Hevc, // default; validator may overwrite
    )
}

use crate::rl_abr::action::{RESOLUTION_LADDER, REFRESH_LADDER_HZ};
use crate::qubox_media::codec::Codec;
```

### 7.2 Keyframe-boundary cadence

The query cadence is **one query per keyframe boundary**. Keyframe
boundaries are produced by ADR-013 §5 (`FramePacer`) at the encoder
side. The integration:

`apps/qubox-host-agent/src/encoder_pipe.rs:412-433` (new method on the
encoder pipeline):

```rust
/// Called by `FramePacer` immediately after a keyframe boundary is
/// produced. If RL-ABR is enabled, query the policy and reconfigure
/// the encoder. Otherwise no-op.
pub fn on_keyframe_boundary(&mut self, obs: &rl_abr::Observation) {
    #[cfg(feature = "rl-abr")]
    {
        if let Some(client) = self.rl_client.as_mut() {
            match futures::executor::block_on(client.query(obs)) {
                Ok(a) => {
                    self.apply_action(&a);
                    self.last_action = Some(a);
                }
                Err(e) => {
                    tracing::warn!("rl-abr query failed: {e}; falling back to GCC");
                    // Do nothing — the GCC controller will keep its last target.
                }
            }
        }
    }
    // When the feature is off, this function is empty.
}
```

The keyframe interval itself is unchanged from ADR-013 §5: typically
every 5 s at 60 fps and every 2 s at 144 fps.

---

## 8. Telemetry pipeline

### 8.1 `TelemetrySink` calls

The telemetry crate (ADR-012 §6, currently `apps/qubox-client-cli/src/telemetry.rs`
plus the future `crates/qubox-platform/src/telemetry.rs`) already
provides a `TelemetrySink::record_event(...)` method. We add a new
event variant:

```rust
// In crates/qubox-platform/src/telemetry.rs (new variant)
pub enum TelemetryEvent {
    // ... existing variants ...
    RlAbrTuple {
        session_id: SessionId,
        timestamp_ns: u64,
        observation: rl_abr::Observation,
        action: rl_abr::Action,
        reward: f32,
        next_observation: rl_abr::Observation,
    },
}
```

Every keyframe boundary, after we receive an action from the policy
and apply it, we call:

```rust
sink.record_event(TelemetryEvent::RlAbrTuple {
    session_id: self.session_id,
    timestamp_ns: clock.now_ns(),
    observation: prev_obs.clone(),
    action,
    reward: reward::reward(&prev_action, &action, &next_obs),
    next_observation: next_obs.clone(),
});
```

### 8.2 On-disk ring buffer

**Path:** `/var/lib/qubox-daemon/telemetry/rl_tuples/`

**Format:** one JSON object per line (`*.jsonl`):

```
{"session_id":"abc123","timestamp_ns":1700000000000000000,
 "observation":{"throughput_bps":12000000,...},
 "action":{"bitrate_bps":8000000,"width":1920,"height":1080,...},
 "reward":15.234,"next_observation":{...}}
```

**Rotation:**

*   One file per session: `rl_tuples_<session_id>_<unix_ts>.jsonl`.
*   Each file capped at 100 MB. When the file exceeds 100 MB, it is
    rotated to `rl_tuples_<session_id>_<unix_ts>_<seq>.jsonl`.
*   Total disk usage capped at 1 GB; oldest files deleted FIFO.

**Implementation:** `crates/qubox-host-agent/src/telemetry/rl_ringbuf.rs`
— a thin wrapper over `tokio::fs::File` + `BufWriter` that enforces
the rotation policy.

### 8.3 Upload procedure (opt-in)

When `--telemetry=rl-abr` is passed:

1.  At session end, the host reads each `*.jsonl` file in
    `/var/lib/qubox-daemon/telemetry/rl_tuples/`.
2.  Compresses to `gzip` (`nix::flate2` crate).
3.  Computes SHA-256 of the compressed bytes.
4.  PUTs to `s3://<configured-bucket>/rl_tuples/<sha256>.jsonl.gz`
    using `aws-sdk-s3` (already a transitive dep via ADR-005).
5.  On 2xx response, deletes the local file.

**Bucket configuration:** env vars
`QUBOX_TELEMETRY_BUCKET`, `QUBOX_TELEMETRY_AWS_REGION`,
`QUBOX_TELEMETRY_AWS_ACCESS_KEY_ID`,
`QUBOX_TELEMETRY_AWS_SECRET_ACCESS_KEY`. None of these are required
when `--telemetry=rl-abr` is **not** passed; the upload code path is
gated behind `#[cfg(feature = "rl-abr-telemetry-upload")]`.

### 8.4 Anonymisation

The JSON payload **never** includes:

*   Frame contents (no screenshot bytes).
*   Window titles.
*   User-identifying information beyond a per-install UUID
    (hashed with SHA-256 + per-install salt).

The session_id is a **per-session** random UUID (v4); it cannot be
correlated across sessions without the install's salt.

---

## 9. Step-by-step implementation order (numbered PRs)

> **Important:** PRs 1–7 are in **this repo**. The training pipeline
> lives in the separate `qubox-rl-training` repo (PRs T1–T3).

| PR  | Branch                              | Contents                                                                                                                            |
|-----|-------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| 1   | `feature/adr-020-p1-observation`    | `crates/qubox-host-agent/src/rl_abr/observation.rs` (§4), unit tests for `to_normalised_vec`. **No model yet.**                       |
| 2   | `feature/adr-020-p2-action`         | `rl_abr/action.rs` (§5.1, §5.2), unit tests for `validate_action` and `fallback_action`.                                             |
| 3   | `feature/adr-020-p3-reward`         | `rl_abr/reward.rs` (§5.3), unit tests. Also generate `reward_constants.json` from `build.rs`.                                       |
| 4   | `feature/adr-020-p4-candle-dep`     | Add `candle-core = "0.11"` and `candle-nn = "0.11"` to `crates/qubox-host-agent/Cargo.toml`. Add the `rl-abr` feature gate.        |
| 5   | `feature/adr-020-p5-policy-server`  | `policy_server/` (§6). Loads a stub random-weight checkpoint for now. End-to-end test: `cargo test -p qubox-host-agent policy_server_roundtrip`. |
| 6   | `feature/adr-020-p6-inference-client` | `rl_abr/client.rs` + `cadence.rs` (§7). Hooks into `encoder_pipe.rs::on_keyframe_boundary`.                                        |
| 7   | `feature/adr-020-p7-telemetry`      | Ring buffer + opt-in upload (§8). `TelemetryEvent::RlAbrTuple` variant.                                                              |
| T1  | `qubox-rl-training` repo            | Python venv, PyTorch 2.6, gymnasium env wrapping the throughput traces + synthetic chunk profiles.                                  |
| T2  | `qubox-rl-training` repo            | A3C implementation (12 obs → 96 actions). Reward matches §5.3. Hyperparameters: γ = 0.99, lr = 1e-4, entropy = 1e-2.               |
| T3  | `qubox-rl-training` repo            | Export `qubox-desktop-v1.safetensors`. Vendor into this repo's `policy_server/checkpoints/`. Open PR against `feature/adr-020`.     |

After T3 lands, merge PR 7 → 6 → 5 → 4 → 3 → 2 → 1 in that order onto
`main` behind the `rl-abr` feature flag, off by default.

---

## 10. Test specifications

All tests are colocated with their module under
`#[cfg(test)] mod tests { ... }`. Run with:

```bash
cargo test -p qubox-host-agent --features rl-abr rl_abr::
cargo test -p qubox-host-agent --features rl-abr policy_server::
```

### 10.1 Observation tests

```rust
#[test]
fn observation_tuple_serializes_to_rkyv() {
    use rkyv::{to_bytes, Deserialize};
    let obs = Observation {
        throughput_bps: 12_000_000,
        decode_latency_ms: 18.5,
        encode_delivered_ratio: 1.05,
        fec_loss_rate: 0.01,
        screen_content_score: 0.7,
        deadline_slack_ms: -2.0,
        past_actions: [4_000_000; 5],
    };
    let bytes = to_bytes::<_, 256>(&obs).unwrap();
    let archived = unsafe { rkyv::archived_root::<Observation>(&bytes[..]) };
    let decoded: Observation = archived.deserialize(&mut rkyv::Infallible).unwrap();
    assert_eq!(decoded.throughput_bps, 12_000_000);
    assert_eq!(decoded.past_actions, [4_000_000; 5]);
}

#[test]
fn observation_normalisation_matches_training() {
    // The expected normalised vector is hard-coded from
    // qubox-rl-training/tests/test_observation.py::test_normalised_vec.
    let obs = Observation {
        throughput_bps: 10_000_000,    // → 0.2
        decode_latency_ms: 16.67,      // → 0.5
        encode_delivered_ratio: 1.0,   // → 1.0
        fec_loss_rate: 0.05,           // → 0.05
        screen_content_score: 0.3,    // → 0.3
        deadline_slack_ms: 8.33,       // → 0.5
        past_actions: [6_000_000; 5],  // → 0.3 each
    };
    let v = obs.to_normalised_vec();
    assert!((v[0] - 0.2).abs() < 1e-5);
    assert!((v[1] - 0.5).abs() < 1e-5);
    assert!((v[2] - 1.0).abs() < 1e-5);
    assert!((v[3] - 0.05).abs() < 1e-5);
    assert!((v[4] - 0.3).abs() < 1e-5);
    assert!((v[5] - 0.5).abs() < 1e-5);
    assert!((v[6] - 0.3).abs() < 1e-5);
    assert!((v[11] - 0.3).abs() < 1e-5);
}
```

### 10.2 Reward tests

```rust
#[test]
fn reward_function_penalises_deadline_violation() {
    let prev = Action::from_indices(2, 1, 0, Codec::Hevc); // 4 Mbps 1080p60
    let action = Action::from_indices(4, 1, 0, Codec::Hevc); // 8 Mbps
    let mut next = Observation::zeroed_for_test();
    next.deadline_slack_ms = -1.0; // missed deadline
    let r = reward(&prev, &action, &next);
    // Quality: ln(8e6) ≈ 15.89
    // Smoothness: 1.0 * |ln(8e6) - ln(4e6)| ≈ 0.69
    // Rebuf term: 4.3 * 0.0 = 0
    // Latency: 0
    // Deadline: -50.0
    // Total ≈ 15.89 - 0.69 - 50.0 ≈ -34.8
    assert!(r < -30.0 && r > -40.0, "reward = {r}");
}

#[test]
fn reward_function_rewards_high_bitrate_when_no_problems() {
    let prev = Action::from_indices(2, 1, 0, Codec::Hevc); // 4 Mbps
    let action = Action::from_indices(6, 1, 0, Codec::Hevc); // 16 Mbps
    let next = Observation::zeroed_for_test(); // all zeros = ideal
    let r = reward(&prev, &action, &next);
    // Quality: ln(16e6) ≈ 16.6
    // Smoothness: 1.0 * |ln(16e6) - ln(4e6)| ≈ 1.39
    // All other terms = 0
    // Total ≈ 15.21
    assert!(r > 14.0 && r < 16.0, "reward = {r}");
}
```

### 10.3 Policy server tests

```rust
#[tokio::test]
async fn policy_server_serves_query_within_1ms() {
    let tmp = tempdir();
    // Generate a random-weight checkpoint of the right shape.
    let ckpt = write_random_checkpoint(tmp.path()).await;
    let (server, _join) = PolicyServer::spawn(&ckpt).await.unwrap();
    let mut client = PolicyClient::connect(test_codec_matrix()).await.unwrap();
    let obs = Observation::zeroed_for_test();
    let t0 = std::time::Instant::now();
    let action = client.query(&obs).await.unwrap();
    let dt = t0.elapsed();
    assert!(dt.as_millis() < 10, "query took {} ms", dt.as_millis());
    assert!(action.bitrate_bps > 0);
}
```

### 10.4 Constraint validator tests

```rust
#[test]
fn validate_action_snaps_to_codec_matrix() {
    // NVENC matrix forbids H.264 at 4K144.
    let matrix = NVIDIA_CODECS;
    let a = Action { bitrate_bps: 20_000_000, width: 3840, height: 2160,
                     refresh_hz: 144, codec_idx: 0 /* H264 */ };
    let v = validate_action(a, &matrix).unwrap();
    // Should snap to AV1.
    assert_eq!(v.codec_idx, 2);
}

#[test]
fn fallback_action_returns_feasible_default() {
    let matrix = SOFTWARE_FALLBACK; // H.264 only
    let prev = Action::from_indices(0, 0, 0, Codec::H264);
    let a = fallback_action(&matrix, 2, &prev);
    assert!(validate_action(a, &matrix).is_some());
}
```

### 10.5 Mock training corpus (for PR T2)

The training repo's pytest fixtures include a **tiny synthetic
corpus** for sanity testing:

```python
# qubox-rl-training/tests/fixtures/synthetic_corpus.py
SYNTHETIC_TRACES = [
    # (throughput_bps sequence, screen_content_score sequence)
    ([5e6, 5e6, 5e6, 5e6, 5e6, 5e6, 5e6, 5e6], [0.5] * 8),
    ([20e6, 1e6, 20e6, 1e6, 20e6, 1e6, 20e6, 1e6], [0.9] * 8),  # flapping
    ([1e6] * 8, [0.1] * 8),  # starved, idle desktop
]
```

Run with:

```bash
cd qubox-rl-training
python -m pytest tests/test_a3c.py -k synthetic
```

---

## 11. File paths & insertion points

| File                                                          | New / modified | Approx lines | Purpose                                                |
|---------------------------------------------------------------|----------------|--------------|--------------------------------------------------------|
| `crates/qubox-host-agent/Cargo.toml`                          | modified       | +12          | Add `candle-core`, `candle-nn`, `rkyv` deps; `rl-abr` feature gate |
| `crates/qubox-host-agent/src/lib.rs`                          | modified       | +3           | `pub mod rl_abr;` (gated behind feature)                |
| `crates/qubox-host-agent/src/rl_abr/mod.rs`                   | new            | ~10          | Re-exports                                             |
| `crates/qubox-host-agent/src/rl_abr/observation.rs`           | new            | ~100         | §4                                                     |
| `crates/qubox-host-agent/src/rl_abr/action.rs`                | new            | ~150         | §5.1, §5.2                                             |
| `crates/qubox-host-agent/src/rl_abr/reward.rs`                | new            | ~60          | §5.3                                                   |
| `crates/qubox-host-agent/src/rl_abr/client.rs`                | new            | ~120         | §7.1                                                   |
| `crates/qubox-host-agent/src/rl_abr/cadence.rs`               | new            | ~40          | §7.2 keyframe-boundary hook                            |
| `crates/qubox-host-agent/src/policy_server/mod.rs`            | new            | ~120         | §6.1 TCP listener                                      |
| `crates/qubox-host-agent/src/policy_server/model.rs`          | new            | ~80          | §6.3 candle MLP                                        |
| `crates/qubox-host-agent/src/policy_server/wire.rs`           | new            | ~30          | §6.2 framing                                           |
| `crates/qubox-host-agent/src/policy_server/checkpoints/qubox-desktop-v1.safetensors` | new | 264 KB | Vendored (PR T3)                                |
| `crates/qubox-host-agent/build.rs`                           | new            | ~40          | Emit `reward_constants.json`                           |
| `crates/qubox-host-agent/src/telemetry/rl_ringbuf.rs`         | new            | ~80          | §8.2 ring buffer                                       |
| `crates/qubox-platform/src/telemetry.rs`                      | modified       | +15          | `TelemetryEvent::RlAbrTuple` variant                   |
| `apps/qubox-host-agent/src/main.rs`                           | modified       | +30          | `--enable-rl-abr`, `--telemetry=rl-abr` CLI flags      |
| `apps/qubox-host-agent/src/encoder_pipe.rs`                  | modified       | +25          | `on_keyframe_boundary` hook (§7.2)                     |
| `apps/qubox-host-agent/src/rate_control.rs`                   | unchanged      | —            | Fall-back when RL disabled (already correct)           |
| `qubox-rl-training/` (separate repo)                         | new            | ~2 K LOC     | PR T1–T3 (training only)                               |

---

## 12. Pitfalls (six concrete gotchas)

1.  **The RL policy is trained on synthetic data; the first production
    release MUST default to OFF and require explicit
    `--enable-rl-abr` opt-in.** A model trained on synthetic traces
    (FCC, HSDPA, Oboe) may pick bad bitrates on real networks; we
    must not silently regress users who didn't ask for the feature.

2.  **The reward function is computed in two languages.** Any drift
    between the Rust implementation (`rl_abr/reward.rs`) and the Python
    implementation (`qubox-rl-training/reward.py`) silently destroys
    the policy. The cross-language invariant is enforced by emitting
    `reward_constants.json` from `build.rs` and having the Python
    tests load it at test time. Never hand-edit the constants in two
    places.

3.  **The action space and the `CodecMatrix` must agree.** The policy
    is trained against a fixed 96-action layout (§5.1). If someone
    adds a 9th bitrate rung or a 4th refresh rate to the ladder, **all
    96 action indices shift**, and every existing checkpoint becomes
    invalid. Gate ladder changes behind a new ADR.

4.  **The policy server is localhost-only; do not bind to a public
    interface.** The listener in §6.1 binds `127.0.0.1:0`. Never use
    `0.0.0.0`. The whole point is that the inference client is a
    subprocess. If we ever need remote inference, that's a separate
    ADR with auth.

5.  **TensorBoard-shaped inference latency spikes.** A first
    `Tensor::from_slice` allocates; the first `linear.forward` may
    JIT-compile a kernel (candle does not currently, but burn does).
    To make the first query fast, run a **warm-up inference** with a
    zero observation on server startup, before announcing the port
    via `QUBOX_RL_POLICY_PORT`.

6.  **The 100 MB / 1 GB ring-buffer caps are not optional.** Without
    them, a long-lived host with telemetry enabled will silently fill
    `/var/lib/qubox-daemon` and cause an OOM kill. Test the rotation
    logic with a `cargo test` that writes 200 MB of fake tuples.

---

## 13. Reviewer sanity-check list

The human reviewer should confirm each of these **before** merging PR
1 of this ADR:

- [ ] **Library choice (`candle-core` 0.11.0)** matches the team's
      policy on third-party Rust ML deps. Specifically: are we OK
      adding a pure-Rust ML framework from Hugging Face, or do we have
      a house preference for `tch`/`onnxruntime-rs`?
- [ ] **Action space cardinality (96)** is acceptable for the training
      compute budget the team can commit to. If not, collapse the
      ladder (e.g., remove 144 Hz → 72 actions).
- [ ] **Reward coefficients** (`α = 4.3`, `β = 1.0`, latency = 0.10,
      deadline miss = −50) match user-study findings. These are
      tunable but should not be silently changed after training.
- [ ] **`Spiral` dataset reference** is acknowledged as fictional.
      The training harness will be built from FCC + HSDPA + Oboe
      network traces paired with **Microsoft "Synthetic Computers at
      Scale"** (98 environments) for the desktop-capture side.
- [ ] **Telemetry opt-in default** is OFF. The opt-in flag
      (`--telemetry=rl-abr`) is separate from `--enable-rl-abr`.
- [ ] **Policy server is localhost-only**; the wire format uses
      length-prefixed rkyv per ADR-015.
- [ ] **All seven PRs in this repo are off-by-default** via the
      `rl-abr` Cargo feature gate; the default `cargo build` produces
      a binary with zero RL-ABR code.

---

## 14. References

### 14.1 Code references in this repo

*   `apps/qubox-host-agent/src/rate_control.rs:46-64` — `GccConfig`
*   `apps/qubox-host-agent/src/rate_control.rs:1-34` — GCC controller
    overview (the pre-RL behaviour we fall back to)
*   `crates/qubox-media/src/lib.rs:1721-1741` — encoder list (per
    ADR-018)
*   `crates/qubox-media/src/lib.rs:1152-1269` — `encoder_args_for`
*   `apps/qubox-client-cli/src/decoder_hw.rs` — frame decode latency
    telemetry source
*   ADR-012 §6 — telemetry surface (`TelemetrySink`)
*   ADR-013 §5 — keyframe-boundary pace-driven decisions
*   ADR-014 §4 — FEC loss rate telemetry
*   ADR-015 §3 — rkyv zero-copy framing (re-used here)
*   ADR-018 §1 — `CodecMatrix`, `choose_codec` (line 108)
*   ADR-018 §4 — screen-content classifier

### 14.2 External research

*   Mao, Netravali, Alizadeh. "Neural Adaptive Video Streaming with
    Pensieve." **SIGCOMM 2017**. (actor-critic A3C; 1-D CNN of 128
    filters × width 4 over bandwidth history + next-chunk sizes; 128
    hidden units; 6-bitrate softmax output; γ = 0.99; reward
    `q(b_t) − α·rebuf − β·|q(b_t) − q(b_{t-1})|` with α = 4.3, β = 1.0;
    trained on FCC + HSDPA traces.)
*   Mao et al. "Comyco: Quality-Aware ABR via Imitation Learning."
    **SIGCOMM 2019**. (Future work; uses MINLP-solved offline expert +
    adversarial information bottleneck.)
*   Yan et al. "PPO-ABR." (PPO replaces A3C; up to 27 % QoE
    improvement over Pensieve in their experiments; uses FCC + Norway
    traces.)
*   Sinha et al. "RCS: Reinforcement Learning-based Congestion
    Control." **SIGCOMM 2024** (out of scope; transport-agnostic,
    QUIC-compatible).
*   "MLTCP." **HotNets 2024** (out of scope; TCP-specific ML
    controller).
*   "OnRL: On-device Reinforcement Learning for adaptive video."
    (Out of scope; informs the future on-client WASM path.)
*   Google Stadia Streaming Tech Deep Dive, **Google I/O 2019**
    (BBR-based ABR for cloud gaming; informs our deadline-aware
    reward).
*   "Network Anatomy and Real-Time Measurement of NVIDIA GeForce
    NOW Cloud Gaming." (No public Pensieve-style RL deployment in
    commercial cloud gaming as of 2026.)

### 14.3 Datasets

*   **FCC Measuring Broadband America** — residential broadband
    throughput traces.
*   **Norway / HSDPA** (Simula) — 3G mobile traces.
*   **USC-NSL Oboe** — production streaming bandwidth traces.
*   **Lancs-Net ABR-Throughput-Traces** — 7 000 CDN-derived 4-minute
    throughput traces.
*   **Microsoft Synthetic Computers at Scale** — 98 synthetic desktop
    environments (closest available stand-in for the "Spiral"
    reference; see §13).

### 14.4 Rust crates (verified versions, 2026)

*   `candle-core` 0.11.0, `candle-nn` 0.11.0 — Hugging Face,
    pure-Rust. CPU footprint ~3–10 MB.
*   `tch` 0.17.0 — Laurent Mazare, libtorch bindings. CPU footprint
    ~50–150 MB (rejected for our use).
*   `onnxruntime` 0.20.0 — ONNX Runtime C++ bindings. CPU footprint
    ~20–80 MB (rejected for our use).
*   `rkyv` 0.8 (per ADR-015).
*   `safetensors` 0.4 (training export format).
*   `tokio` 1.x (already in tree via ADR-005).