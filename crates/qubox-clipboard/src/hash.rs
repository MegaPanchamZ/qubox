//! `blake3` content hashing for clipboard payloads.
//!
//! The hash is local-state only: it lets the watcher detect when the
//! clipboard's text or image has actually changed (a no-op when the
//! same content is set twice) without re-sending identical bytes
//! over the wire. The hash is never sent on the wire — both sides
//! compute it locally.

use qubox_proto::ClipboardPayload;

/// Compute the content hash of a `ClipboardPayload`.
///
/// `Clear` produces the `blake3` hash of an empty buffer, which is
/// also the hash of an empty text. Callers must therefore use the
/// `ClipboardPayload` variant itself (not just the hash) to
/// distinguish a `Clear` from an empty text.
pub fn hash_payload(payload: &ClipboardPayload) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    match payload {
        ClipboardPayload::Text { utf8 } => {
            hasher.update(utf8.as_bytes());
        }
        ClipboardPayload::ImagePng { width, height, png } => {
            hasher.update(&width.to_le_bytes());
            hasher.update(&height.to_le_bytes());
            hasher.update(png);
        }
        ClipboardPayload::Clear => {
            hasher.update(&[] as &[u8]);
        }
    }
    hasher.finalize()
}

/// True when the incoming `seq` should be accepted given the
/// `last_seen_seq` the receiver has already applied. Strict
/// greater-than gives last-write-wins semantics and rejects
/// duplicates from a rebroadcast.
pub fn seq_advances(last_seen_seq: u64, incoming_seq: u64) -> bool {
    incoming_seq > last_seen_seq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_text_hash_is_stable() {
        let p1 = ClipboardPayload::Text {
            utf8: "hello".to_string(),
        };
        let p2 = ClipboardPayload::Text {
            utf8: "hello".to_string(),
        };
        let p3 = ClipboardPayload::Text {
            utf8: "hellp".to_string(),
        };
        assert_eq!(hash_payload(&p1), hash_payload(&p2));
        assert_ne!(hash_payload(&p1), hash_payload(&p3));
    }

    #[test]
    fn blake3_png_hash_is_stable() {
        let p1 = ClipboardPayload::ImagePng {
            width: 4,
            height: 4,
            png: vec![0xFF; 16],
        };
        let p2 = ClipboardPayload::ImagePng {
            width: 4,
            height: 4,
            png: vec![0xFF; 16],
        };
        assert_eq!(hash_payload(&p1), hash_payload(&p2));

        let p3 = ClipboardPayload::ImagePng {
            width: 4,
            height: 4,
            png: vec![0x00; 16],
        };
        assert_ne!(hash_payload(&p1), hash_payload(&p3));
    }

    #[test]
    fn clear_and_empty_text_have_the_same_hash_but_different_variants() {
        let clear = ClipboardPayload::Clear;
        let empty_text = ClipboardPayload::Text {
            utf8: String::new(),
        };
        assert_eq!(hash_payload(&clear), hash_payload(&empty_text));
        assert_ne!(clear, empty_text);
    }

    #[test]
    fn seq_comparator_advances_only_on_strictly_greater() {
        assert!(!seq_advances(5, 5));
        assert!(!seq_advances(5, 4));
        assert!(seq_advances(5, 6));
    }
}
