//! Chunk codec / `LiveSync` plain-text interoperability tests.
//!
//! These load *pinned official-format fixtures* (real Obsidian `LiveSync` leaf
//! documents) rather than only self-generated round trips, so the codec policy
//! is exercised against the actual on-wire shape `LiveSync` produces.

use couchdb_file_sync::models::{ChunkDoc, LegacyChunkData};
use std::path::PathBuf;

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/livesync")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

#[test]
fn plain_text_livesync_chunk_round_trips_without_base64_reinterpretation() {
    // A real LiveSync text chunk: `data` is a raw string with no codec. In the
    // supported plain-text import mode it must come back byte-identical, never
    // base64-reinterpreted.
    let chunk: ChunkDoc = serde_json::from_str(&fixture("note-plain-leaf.json")).unwrap();
    assert!(
        chunk.codec.is_none(),
        "official LiveSync chunks carry no codec"
    );
    let bytes = chunk
        .content_bytes(LegacyChunkData::PlainText)
        .expect("plain-text import must decode");
    let text = String::from_utf8(bytes).expect("plain chunk content is UTF-8");
    assert!(text.starts_with("# Hello, LiveSync"), "{text}");
    assert!(
        text.contains("raw string `data`"),
        "content must round-trip unchanged: {text}"
    );
}

#[test]
fn base64_looking_raw_text_is_not_reinterpreted() {
    // "YWJj is raw text..." is not base64 for anything; it is plain text. In
    // plain-text import mode the bytes must be the literal text, not a base64
    // decode.
    let chunk: ChunkDoc =
        serde_json::from_str(&fixture("note-plain-leaf-base64-looking.json")).unwrap();
    let bytes = chunk
        .content_bytes(LegacyChunkData::PlainText)
        .expect("plain-text import must decode");
    assert_eq!(
        bytes, b"YWJj is raw text, not base64 for 'abc'.\n",
        "base64-looking raw text must NOT be reinterpreted"
    );
}

#[test]
fn v05_untagged_base64_record_reads_in_migration_mode() {
    // A v0.5 untagged base64 chunk (written by an earlier version of this
    // tool). Migration mode must decode it as base64.
    let chunk: ChunkDoc = serde_json::from_str(&fixture("v05-base64-leaf.json")).unwrap();
    assert!(chunk.codec.is_none());
    assert_eq!(
        chunk.content_bytes(LegacyChunkData::Base64).unwrap(),
        b"hello v0.5 chunk",
        "v0.5 migration mode decodes untagged base64"
    );
}

#[test]
fn unsupported_compressed_livesync_chunk_fails() {
    // Official LiveSync LZ-compressed chunks are unsupported. They must fail
    // per-chunk in either legacy mode rather than corrupt content.
    let chunk: ChunkDoc = serde_json::from_str(&fixture("compressed-leaf.json")).unwrap();
    for legacy in [LegacyChunkData::Base64, LegacyChunkData::PlainText] {
        let err = chunk
            .content_bytes(legacy)
            .expect_err("compressed chunk must be rejected");
        assert!(
            err.to_string().contains("unsupported LiveSync compressed"),
            "{err}"
        );
    }
}

#[test]
fn native_encoded_chunk_is_unambiguous_in_any_mode() {
    // A native chunk written by this tool carries an explicit base64 codec, so
    // it decodes identically under either legacy policy.
    let chunk = ChunkDoc::new_encoded("h:native".to_string(), b"\x00\xFF\xFE binary".to_vec());
    for legacy in [LegacyChunkData::Base64, LegacyChunkData::PlainText] {
        assert_eq!(
            chunk.content_bytes(legacy).unwrap(),
            b"\x00\xFF\xFE binary",
            "explicit codec is authoritative regardless of legacy policy"
        );
    }
}
