//! ROI (Region of Interest) classifier for unequal error protection.
//!
//! Decides the parity-shard count per frame based on whether the
//! capture region covers the central 1080p desktop area. Keyframes
//! always get 2× parity (m=2). Central captures get 2×, peripheral
//! get 1.5× (m=1).

use qubox_media::EncodedVideoAccessUnit;
use qubox_proto::CaptureRegion;

/// Decide the parity-shard count for an encoded access unit.
///
/// Returns `m` in 1..=4. The caller passes `m` to
/// `MediaDatagramSender::with_reed_solomon().send_frame()` or the
/// adaptive `FecController::adjust_for_loss(...)` output.
pub fn classify_roi(
    encoded: &EncodedVideoAccessUnit,
    capture_region: Option<CaptureRegion>,
    receiver_max_parity: usize,
) -> usize {
    // 1. Keyframes always get 2x parity regardless of ROI.
    if encoded.keyframe {
        return receiver_max_parity.min(2);
    }

    // 2. ROI classification: does the capture cover the central 1080p
    //    desktop region?
    let covers_central = match capture_region {
        None => true,
        Some(r) => {
            // Centre of the capture framebuffer.
            let cx = (r.x + r.width / 2) as i32;
            let cy = (r.y + r.height / 2) as i32;
            // 1920×1080 centred on (cx, cy).
            let (cl, ct, cw, ch) = (cx - 960, cy - 540, 1920, 1080);

            let (rx0, ry0) = (r.x as i32, r.y as i32);
            let (rx1, ry1) = (rx0 + r.width as i32, ry0 + r.height as i32);

            let overlap_w = (rx1.min(cl + cw)).saturating_sub(rx0.max(cl));
            let overlap_h = (ry1.min(ct + ch)).saturating_sub(ry0.max(ct));
            if overlap_w <= 0 || overlap_h <= 0 {
                false
            } else {
                let overlap = (overlap_w as u64) * (overlap_h as u64);
                let central_area = (cw as u64) * (ch as u64);
                // >50 % of the central 1920×1080 region is visible.
                overlap * 2 > central_area
            }
        }
    };

    if covers_central {
        // ROI capture → 2× parity (full protection).
        receiver_max_parity.min(2)
    } else {
        // Peripheral capture → 1.5× parity (1 shard per block).
        receiver_max_parity.min(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qubox_media::{EncodedVideoAccessUnit, H264NalUnitInfo};

    fn make_au(keyframe: bool) -> EncodedVideoAccessUnit {
        EncodedVideoAccessUnit {
            codec: qubox_proto::VideoCodec::H264,
            frame_id: 0,
            timestamp_micros: 0,
            keyframe,
            nal_units: Vec::<H264NalUnitInfo>::new(),
            bytes: Vec::new(),
            display_id: 0,
            stream_id: 0,
            width: 1920,
            height: 1080,
            color_space: None,
            bit_depth: 8,
        }
    }

    #[test]
    fn roi_classifier_central_1080p_gets_2x() {
        let au = make_au(false);
        let region = CaptureRegion {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert_eq!(classify_roi(&au, Some(region), 4), 2);
    }

    #[test]
    fn roi_classifier_periphery_gets_1_5x() {
        let au = make_au(false);
        // Picture-in-picture on the right side — does not cover central 1080p.
        let region = CaptureRegion {
            x: 1920,
            y: 0,
            width: 800,
            height: 600,
        };
        assert_eq!(classify_roi(&au, Some(region), 4), 1);
    }

    #[test]
    fn roi_classifier_keyframe_always_gets_2x() {
        let au = make_au(true);
        // Even a tiny peripheral capture gets 2x on keyframes.
        let region = CaptureRegion {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        };
        assert_eq!(classify_roi(&au, Some(region), 4), 2);
    }

    #[test]
    fn roi_classifier_no_region_defaults_to_2x() {
        let au = make_au(false);
        assert_eq!(classify_roi(&au, None, 4), 2);
    }

    #[test]
    fn roi_classifier_respects_receiver_max_parity() {
        let au = make_au(false);
        let region = CaptureRegion {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        // Even central ROI can't exceed receiver_max_parity.
        assert_eq!(classify_roi(&au, Some(region), 1), 1);
    }
}
