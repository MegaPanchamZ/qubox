use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    let constants = serde_json::json!({
        "alpha_rebuf": 4.3_f32,
        "beta_smooth": 1.0_f32,
        "latency_penalty_per_ms": 0.10_f32,
        "deadline_miss_penalty": 50.0_f32,
        "throughput_div_bps": 50_000_000_u32,
        "decode_latency_div_ms": 33.3_f32,
        "ratio_clip": 1.0_f32,
        "fec_loss_div": 1.0_f32,
        "screen_content_div": 1.0_f32,
        "deadline_slack_div_ms": 16.67_f32,
        "bitrate_div_bps": 20_000_000_u32,
        "ewma_alpha_throughput": 0.30_f32,
        "ewma_alpha_decode_latency": 0.20_f32,
        "ewma_alpha_ratio": 0.20_f32,
        "ewma_alpha_fec_loss": 0.15_f32,
        "ewma_alpha_deadline_slack": 0.40_f32,
        "bitrate_ladder_bps": [
            1_000_000_u32, 2_000_000, 4_000_000, 6_000_000,
            8_000_000, 12_000_000, 16_000_000, 20_000_000,
        ],
        "resolution_ladder": [
            [1280_u16, 720_u16],
            [1920_u16, 1080_u16],
            [2560_u16, 1440_u16],
            [3840_u16, 2160_u16],
        ],
        "refresh_ladder_hz": [60_u8, 90_u8, 144_u8],
    });
    let path = Path::new(&out).join("reward_constants.json");
    fs::write(&path, serde_json::to_string_pretty(&constants).unwrap()).unwrap();
    println!("cargo::rerun-if-changed=build.rs");
}
