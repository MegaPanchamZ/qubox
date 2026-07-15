pub const SOBEL_THRESHOLD: u8 = 32;
pub const HIGH_FREQ_RATIO_THRESHOLD: f32 = 0.18;
pub const VERTICAL_EDGE_RATIO_THRESHOLD: f32 = 0.06;
pub const COLOR_CARDINALITY_MAX: u8 = 24;
pub const DOWNSAMPLE_FACTOR: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentClass {
    Natural,
    ScreenContent,
    Mixed,
}

pub struct ContentClassifier {
    width: u32,
    height: u32,
    aggressive: bool,
}

impl ContentClassifier {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            aggressive: false,
        }
    }

    pub fn with_aggressive(mut self, aggressive: bool) -> Self {
        self.aggressive = aggressive;
        self
    }

    fn effective_threshold(&self, base: f32) -> f32 {
        if self.aggressive {
            base * 0.75
        } else {
            base
        }
    }

    fn downsample_dimensions(&self) -> (u32, u32) {
        let dw = (self.width / DOWNSAMPLE_FACTOR).max(1);
        let dh = (self.height / DOWNSAMPLE_FACTOR).max(1);
        (dw, dh)
    }

    pub fn classify_from_grayscale(&self, gray_pixels: &[u8]) -> ContentClass {
        let (dw, dh) = self.downsample_dimensions();
        let expected = (dw * dh) as usize;
        if gray_pixels.len() < expected {
            return ContentClass::Natural;
        }

        // Compute Sobel magnitudes on downsample grid
        let mut edge_histogram = [0u32; 256];
        let mut vertical_edges: u64 = 0;
        let mut total_pixels: u64 = 0;

        // Stride-aware 3x3 Sobel
        let stride = dw as usize;
        for y in 1..(dh - 1) {
            for x in 1..(dw - 1) {
                let idx = y as usize * stride + x as usize;
                // 3x3 neighborhood
                let tl = gray_pixels[idx - stride - 1] as i16;
                let tc = gray_pixels[idx - stride] as i16;
                let tr = gray_pixels[idx - stride + 1] as i16;
                let ml = gray_pixels[idx - 1] as i16;
                let mr = gray_pixels[idx + 1] as i16;
                let bl = gray_pixels[idx + stride - 1] as i16;
                let bc = gray_pixels[idx + stride] as i16;
                let br = gray_pixels[idx + stride + 1] as i16;

                // Gx = (tr + 2*mr + br) - (tl + 2*ml + bl)
                let gx = (tr + 2 * mr + br) - (tl + 2 * ml + bl);
                // Gy = (bl + 2*bc + br) - (tl + 2*tc + tr)
                let gy = (bl + 2 * bc + br) - (tl + 2 * tc + tr);

                let mag = ((gx.abs() + gy.abs()) / 2).min(255) as u8;
                let bin = (mag as usize).min(255);
                edge_histogram[bin] += 1;

                // Vertical edge: |Gx| >> |Gy| (strong horizontal gradient → vertical edge)
                if gx.abs() > gy.abs() * 3 && mag > SOBEL_THRESHOLD {
                    vertical_edges += 1;
                }
                total_pixels += 1;
            }
        }

        if total_pixels == 0 {
            return ContentClass::Natural;
        }

        let high_freq: u64 = edge_histogram[200..256].iter().map(|&c| c as u64).sum();
        let high_freq_ratio = high_freq as f32 / total_pixels as f32;
        let vertical_edge_ratio = vertical_edges as f32 / total_pixels as f32;

        let hf_thresh = self.effective_threshold(HIGH_FREQ_RATIO_THRESHOLD);
        let ve_thresh = self.effective_threshold(VERTICAL_EDGE_RATIO_THRESHOLD);

        // Color cardinality estimation from grayscale variance
        let mut quantized_colors = [false; 256];
        for &p in gray_pixels
            .iter()
            .take(expected)
            .step_by(DOWNSAMPLE_FACTOR as usize)
        {
            let q = (p >> 4) as usize; // 16-bin quantization
            quantized_colors[q] = true;
        }
        let color_cardinality = quantized_colors.iter().filter(|&&c| c).count() as u8;

        if high_freq_ratio > hf_thresh
            && vertical_edge_ratio > ve_thresh
            && color_cardinality < COLOR_CARDINALITY_MAX
        {
            ContentClass::ScreenContent
        } else if high_freq_ratio > hf_thresh * 0.5 && color_cardinality < 12 {
            ContentClass::Mixed
        } else {
            ContentClass::Natural
        }
    }

    pub fn classify(&self, rgba_pixels: &[u8]) -> ContentClass {
        let (dw, dh) = self.downsample_dimensions();
        let expected_rgba = (dw * dh * 4) as usize;
        if rgba_pixels.len() < expected_rgba {
            return ContentClass::Natural;
        }
        let stride = dw as usize;
        let gray: Vec<u8> = (0..dh)
            .flat_map(|y| {
                (0..dw).map(move |x| {
                    let idx = (y as usize * stride + x as usize) * 4;
                    let r = rgba_pixels[idx] as u32;
                    let g = rgba_pixels[idx + 1] as u32;
                    let b = rgba_pixels[idx + 2] as u32;
                    ((r * 77 + g * 150 + b * 29) >> 8) as u8
                })
            })
            .collect();
        self.classify_from_grayscale(&gray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_solid(width: u32, height: u32, gray_val: u8) -> Vec<u8> {
        let (dw, dh) = (width / DOWNSAMPLE_FACTOR, height / DOWNSAMPLE_FACTOR);
        let dw = dw.max(1);
        let dh = dh.max(1);
        vec![gray_val; (dw * dh) as usize]
    }

    fn make_checkerboard(width: u32, height: u32, tile: u32) -> Vec<u8> {
        let (dw, dh) = (width / DOWNSAMPLE_FACTOR, height / DOWNSAMPLE_FACTOR);
        let dw = dw.max(1);
        let dh = dh.max(1);
        let tile_w = (tile / DOWNSAMPLE_FACTOR).max(1);
        (0..dh)
            .flat_map(|y| {
                (0..dw).map(move |x| {
                    if ((x / tile_w) + (y / tile_w)) % 2 == 0 {
                        200u8
                    } else {
                        40u8
                    }
                })
            })
            .collect()
    }

    fn make_text_pattern(width: u32, height: u32) -> Vec<u8> {
        let (dw, dh) = (width / DOWNSAMPLE_FACTOR, height / DOWNSAMPLE_FACTOR);
        let dw = dw.max(1);
        let dh = dh.max(1);
        let mut pixels = vec![240u8; (dw * dh) as usize];
        // Draw horizontal and vertical lines (text-like edges)
        for y in (0..dh).step_by(3) {
            for x in 0..dw {
                pixels[(y * dw + x) as usize] = 10;
            }
        }
        for x in (0..dw).step_by(8) {
            for y in 0..dh {
                pixels[(y * dw + x) as usize] = 10;
            }
        }
        pixels
    }

    fn make_gradient(width: u32, height: u32) -> Vec<u8> {
        let (dw, dh) = (width / DOWNSAMPLE_FACTOR, height / DOWNSAMPLE_FACTOR);
        let dw = dw.max(1);
        let dh = dh.max(1);
        (0..dh)
            .flat_map(|y| {
                (0..dw).map(move |x| {
                    ((x as f32 / dw as f32 * 200.0) + (y as f32 / dh as f32 * 55.0)) as u8
                })
            })
            .collect()
    }

    #[test]
    fn classifier_detects_text_heavy_frames() {
        let classifier = ContentClassifier::new(1920, 1080);
        let gray = make_text_pattern(1920, 1080);
        let result = classifier.classify_from_grayscale(&gray);
        assert_eq!(result, ContentClass::ScreenContent);
    }

    #[test]
    fn classifier_detects_natural_video() {
        let classifier = ContentClassifier::new(1920, 1080);
        let gray = make_gradient(1920, 1080);
        let result = classifier.classify_from_grayscale(&gray);
        assert_eq!(result, ContentClass::Natural);
    }

    #[test]
    fn classifier_is_conservative_on_mixed() {
        let classifier = ContentClassifier::new(1920, 1080);
        let gray = make_gradient(1920, 1080);
        let mut mixed = gray.clone();
        // Add some text-like artifacts
        for i in (0..mixed.len()).step_by(mixed.len() / 20) {
            mixed[i] = 0;
        }
        let result = classifier.classify_from_grayscale(&mixed);
        assert_ne!(result, ContentClass::ScreenContent);
    }

    #[test]
    fn classifier_returns_natural_for_solid() {
        let classifier = ContentClassifier::new(640, 480);
        let gray = make_solid(640, 480, 128);
        let result = classifier.classify_from_grayscale(&gray);
        assert_eq!(result, ContentClass::Natural);
    }

    #[test]
    fn classifier_detects_checkerboard_as_screen() {
        let classifier = ContentClassifier::new(1920, 1080);
        let gray = make_checkerboard(1920, 1080, 32);
        let result = classifier.classify_from_grayscale(&gray);
        assert_eq!(result, ContentClass::ScreenContent);
    }

    #[test]
    fn aggressive_mode_lowers_thresholds() {
        let classifier = ContentClassifier::new(1920, 1080).with_aggressive(true);
        let gray = make_gradient(1920, 1080);
        let result = classifier.classify_from_grayscale(&gray);
        assert_eq!(result, ContentClass::Natural);
    }

    #[test]
    fn rgba_classify_works() {
        let (w, h) = (1920u32, 1080u32);
        let (dw, dh) = (w / DOWNSAMPLE_FACTOR, h / DOWNSAMPLE_FACTOR);
        let mut rgba = vec![255u8; (dw * dh * 4) as usize];
        for i in (0..rgba.len()).step_by(4 * 8) {
            rgba[i] = 0;
            rgba[i + 1] = 0;
            rgba[i + 2] = 0;
        }
        let classifier = ContentClassifier::new(w, h);
        let result = classifier.classify(&rgba);
        assert_eq!(result, ContentClass::ScreenContent);
    }
}
