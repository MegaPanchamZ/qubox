//! Windows clipboard backend via `arboard`.
//!
//! `arboard` on Windows uses the standard Win32 `OpenClipboard` /
//! `SetClipboardData` API. The `!Send + !Sync` constraint is
//! respected by constructing a fresh `Clipboard` per call.

use qubox_proto::ClipboardPayload;

use super::{ClipboardError, ClipboardImage, ClipboardSnapshot};

const MAX_IMAGE_PIXELS: u32 = 4_096 * 4_096;

pub fn read_snapshot_inner(
    text_enabled: bool,
    image_enabled: bool,
) -> Result<ClipboardSnapshot, ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Handle(e.to_string()))?;

    let mut snapshot = ClipboardSnapshot::default();

    if text_enabled {
        match clipboard.get_text() {
            Ok(text) => {
                if !text.is_empty() {
                    snapshot.text = Some(text);
                }
            }
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(error) => return Err(ClipboardError::Read(error.to_string())),
        }
    }

    if image_enabled {
        match clipboard.get_image() {
            Ok(image) => {
                if !image.bytes.is_empty() {
                    let pixels = (image.width as u64) * (image.height as u64);
                    if pixels <= MAX_IMAGE_PIXELS as u64 {
                        match encode_png(image.width as u32, image.height as u32, &image.bytes) {
                            Ok(png) => {
                                snapshot.image = Some(ClipboardImage {
                                    width: image.width as u32,
                                    height: image.height as u32,
                                    png,
                                });
                            }
                            Err(error) => {
                                tracing::warn!(?error, "clipboard image PNG encode failed");
                            }
                        }
                    }
                }
            }
            Err(arboard::Error::ContentNotAvailable) => {}
            Err(error) => return Err(ClipboardError::Read(error.to_string())),
        }
    }

    Ok(snapshot)
}

pub fn apply_payload_inner(payload: &ClipboardPayload) -> Result<(), ClipboardError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| ClipboardError::Handle(e.to_string()))?;

    match payload {
        ClipboardPayload::Text { utf8 } => clipboard
            .set_text(utf8.clone())
            .map_err(|e| ClipboardError::Write(e.to_string())),
        ClipboardPayload::ImagePng { png, .. } => {
            let (width, height, rgba) = decode_png(png)?;
            let img = arboard::ImageData {
                width: width as usize,
                height: height as usize,
                bytes: rgba.into(),
            };
            clipboard
                .set_image(img)
                .map_err(|e| ClipboardError::Write(e.to_string()))
        }
        ClipboardPayload::Clear => clipboard
            .clear()
            .map_err(|e| ClipboardError::Write(e.to_string())),
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| ClipboardError::PngEncode(e.to_string()))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| ClipboardError::PngEncode(e.to_string()))?;
    }
    Ok(out)
}

fn decode_png(png: &[u8]) -> Result<(u32, u32, Vec<u8>), ClipboardError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder
        .read_info()
        .map_err(|e| ClipboardError::PngDecode(e.to_string()))?;
    let mut buf = vec![0_u8; reader.output_buffer_size().unwrap_or(0)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| ClipboardError::PngDecode(e.to_string()))?;
    let bytes = buf[..info.buffer_size()].to_vec();
    Ok((info.width, info.height, bytes))
}
