// End-to-end visual proof of the Reed-Solomon FEC layer on synthetic
// video frames. Generates a moving test pattern, runs it through
// ReedSolomonFec, simulates various loss rates, reconstructs, and
// reports PSNR, throughput, and recovery rate. Also writes a few
// PBM (Portable Bitmap) snapshots so you can eyeball the frames.

use qubox_transport::media::rs_fec::{FecController, ReedSolomonFec};
use std::time::Instant;

const W: usize = 128; // frame width
const H: usize = 96; // frame height
const FRAMES: usize = 240;
const N_LOSS_TRIALS: usize = 50;

fn make_frame(frame_idx: usize) -> Vec<u8> {
    // Synthetic RGBA test pattern: vertical bars + moving diagonal line.
    let mut buf = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let off = (y * W + x) * 4;
            let bar = (x * 8 / W) as u8;
            let diag = (((x as i32 - y as i32) + frame_idx as i32).rem_euclid(64)) as u8;
            let r = bar.wrapping_mul(32);
            let g = diag.wrapping_mul(4);
            let b = ((x + y + frame_idx) & 0xFF) as u8;
            buf[off] = r;
            buf[off + 1] = g;
            buf[off + 2] = b;
            buf[off + 3] = 0xFF;
        }
    }
    buf
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(x, y)| {
            let d = (*x as f64) - (*y as f64);
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0 * 255.0 / mse).log10()
}

fn write_pbm(path: &str, frame: &[u8]) {
    // PGM (grayscale) for simplicity. Take the green channel.
    let mut f = std::fs::File::create(path).expect("create pbm");
    use std::io::Write;
    writeln!(f, "P5\n{} {}\n255", W, H).unwrap();
    for pixel in frame.chunks_exact(4) {
        f.write_all(&[pixel[1]]).unwrap();
    }
}

fn main() {
    println!("=== Qubix RS-FEC video-frame demo ===");
    println!(
        "  frame: {}x{} RGBA  ({} bytes), {} frames simulated\n",
        W,
        H,
        W * H * 4,
        FRAMES
    );

    let block_size = 4;
    let parity_shards = 2;
    let rs = ReedSolomonFec::new(block_size, parity_shards).unwrap();

    // Build a couple of reference frames for PSNR comparison.
    let sample_indices = [0usize, FRAMES / 4, FRAMES / 2, FRAMES - 1];
    let originals: Vec<Vec<u8>> = sample_indices.iter().map(|&i| make_frame(i)).collect();
    let enc: Vec<_> = originals.iter().map(|f| rs.encode(f).unwrap()).collect();
    let shard_len = enc[0].shard_len;

    // Save one PBM snapshot for visual inspection.
    write_pbm("/tmp/rs_demo_original.pgm", &originals[0]);
    println!("  saved /tmp/rs_demo_original.pgm (frame 0 reference, P5 format)\n");

    // === FEC throughput on a moving test pattern ===
    let t = Instant::now();
    let mut recovered_total = 0usize;
    let mut total_bytes = 0usize;
    for i in 0..FRAMES {
        let f = make_frame(i);
        total_bytes += f.len();
        let e = rs.encode(&f).unwrap();
        // Simulate one lost data shard (within recovery range for block=4/parity=2).
        let mut data: Vec<Option<Vec<u8>>> = e.data.into_iter().map(Some).collect();
        let mut par: Vec<Option<Vec<u8>>> = e.parity.into_iter().map(Some).collect();
        let idx = i % data.len();
        data[idx] = None;
        let n = rs.reconstruct(&mut data, &mut par).unwrap();
        recovered_total += n;
    }
    let enc_elapsed = t.elapsed();
    let fps = FRAMES as f64 / enc_elapsed.as_secs_f64();
    let mb = total_bytes as f64 / enc_elapsed.as_secs_f64() / 1_048_576.0;
    println!("  encode+simulate-loss+recover @ 1 lost shard/frame:");
    println!(
        "    {FRAMES} frames in {elapsed:>7.2?} -> {fps:>7.0} fps, {mb:>7.1} MB/s",
        elapsed = enc_elapsed
    );
    println!("    recovered shards total: {recovered_total}\n");

    // === PSNR vs. loss rate ===
    println!("  PSNR after RS(4+2) recovery at various loss rates:");
    println!("    loss %   |   mean PSNR (dB)   |   recovered shards");
    println!("    ---------+--------------------+--------------------");
    let loss_rates = [0.0_f64, 0.1, 0.5, 1.0, 2.0, 5.0];
    // RS(4+2) can recover at most 2 shards per block (== parity).
    // Anything above ~33% (2/6) per-block loss is unrecoverable; this
    // demo only shows the working region.
    for &loss_pct in &loss_rates {
        let mut psnr_sum = 0.0;
        let mut recovered = 0usize;
        for trial in 0..N_LOSS_TRIALS {
            for (idx, original) in originals.iter().enumerate() {
                let e = enc[idx].clone();
                let mut data: Vec<Option<Vec<u8>>> = e.data.into_iter().map(Some).collect();
                let mut par: Vec<Option<Vec<u8>>> = e.parity.into_iter().map(Some).collect();
                let total_shards = data.len() + par.len();
                let n_drop = ((loss_pct / 100.0) * total_shards as f64).ceil() as usize;
                // Deterministic drop pattern (vary by trial).
                let mut drop_set: Vec<usize> = (0..n_drop)
                    .map(|k| (trial * 7 + k * 13 + idx * 5) % total_shards)
                    .collect();
                drop_set.sort_unstable();
                drop_set.dedup();
                for &d in &drop_set {
                    if d < data.len() {
                        data[d] = None;
                    } else {
                        par[d - data.len()] = None;
                    }
                }
                let _prev_data = data.clone();
                let _prev_par = par.clone();
                let n = rs.reconstruct(&mut data, &mut par).unwrap_or(0);
                recovered += n;
                // Assemble original-size frame from recovered data shards.
                let mut out = Vec::with_capacity(data.len() * shard_len);
                for s in &data {
                    out.extend_from_slice(s.as_ref().expect("drop beyond parity"));
                }
                out.truncate(original.len());
                psnr_sum += psnr(&out, original);
            }
        }
        let n_samples = N_LOSS_TRIALS * originals.len();
        let mean_psnr = psnr_sum / n_samples as f64;
        let marker = if mean_psnr >= 40.0 {
            "(visually lossless)"
        } else if mean_psnr >= 30.0 {
            "(imperceptible)"
        } else if mean_psnr >= 20.0 {
            "(visible artifacts)"
        } else {
            "(unusable)"
        };
        println!(
            "    {lp:>5.1} %  |   {p:>10.2} dB       |   {r:>5} / {n:>5}  {m}",
            lp = loss_pct,
            p = mean_psnr,
            r = recovered,
            n = n_samples,
            m = marker
        );
    }

    // === FecController reaction time ===
    println!("\n  FecController: step up/down response");
    let mut ctrl = FecController::new(block_size);
    let mut prev = ctrl.last_parity();
    println!("    start                                       -> {prev} parity");
    for (label, loss_x1000) in [
        ("step up to 0.4% loss  (300 ppm)", 400u32),
        ("step up to 1.5% loss  (1500 ppm)", 1500),
        ("step down to 0.0% loss (0 ppm)", 0),
        ("step up to 3.0% loss  (3000 ppm)", 3000),
        ("step up to 7.0% loss  (7000 ppm)", 7000),
    ] {
        ctrl.adjust_for_loss(loss_x1000);
        let cur = ctrl.last_parity();
        let arrow = if cur > prev {
            "up"
        } else if cur < prev {
            "dn"
        } else {
            "=="
        };
        println!("    {label:<46} -> {cur} parity  ({arrow})");
        prev = cur;
    }

    println!("\n  Done.  Open /tmp/rs_demo_original.pgm in any image viewer to inspect the reference frame.");
}
