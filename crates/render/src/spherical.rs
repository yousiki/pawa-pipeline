//! Inject Spherical Video V1 (Google spatial-media) metadata into a rendered MP4.
//!
//! ReplayMod's `RenderSettings.injectSphericalMetadata` only fires on its built-in MP4 muxing path;
//! the `MP4_CUSTOM` ffmpeg export this pipeline uses bypasses it. ffmpeg can READ spherical metadata
//! but cannot write it, so it is injected here after the encode.
//!
//! The V1 format is a single `uuid` box (UUID `ffcc8263-f855-4a93-8814-587a02521fdd`) carrying an
//! RDF/XML blob, inserted as a child of the video `trak`. Because these renders write `moov` at the
//! END of the file (no `+faststart`), inserting inside `moov` does not shift the `mdat` payload, so
//! the sample-chunk offsets in `stco`/`co64` — which point into `mdat`, ahead of `moov` — stay
//! valid and no offset rewrite is needed. A file whose `moov` precedes its `mdat` is refused rather
//! than corrupted.

use std::path::Path;

use anyhow::{Result, bail};

/// Google "Spherical Video V1" uuid box identifier.
const SPHERICAL_UUID: [u8; 16] = [
    0xff, 0xcc, 0x82, 0x63, 0xf8, 0x55, 0x4a, 0x93, 0x88, 0x14, 0x58, 0x7a, 0x02, 0x52, 0x1f, 0xdd,
];

/// Value of `GSpherical:StitchingSoftware`. Part of the injected bytes, so changing it changes every
/// future file's metadata — and the fixture test below.
const SOFTWARE: &str = "minecraft-renderer";

/// One ISO-BMFF box located inside a buffer.
#[derive(Clone, Copy, Debug)]
struct BoxRef {
    kind: [u8; 4],
    /// Offset of the box header (its size field).
    start: usize,
    /// Offset of the first payload byte, past the header and any 64-bit largesize.
    content: usize,
    /// One past the last byte of the box.
    end: usize,
}

/// The boxes of `buf[start..end]`, stopping cleanly at a truncated or zero-size trailer rather than
/// reporting a box that runs past the region it lives in.
fn boxes(buf: &[u8], start: usize, end: usize) -> Vec<BoxRef> {
    let mut found = Vec::new();
    let mut off = start;
    while off + 8 <= end {
        let mut size = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = buf[off + 4..off + 8].try_into().unwrap();
        let content = if size == 1 {
            // 64-bit largesize, stored in the 8 bytes after the header.
            if off + 16 > end {
                return found;
            }
            size = u64::from_be_bytes(buf[off + 8..off + 16].try_into().unwrap());
            off + 16
        } else {
            if size == 0 {
                // A zero size means "extends to the end of the enclosing region".
                size = (end - off) as u64;
            }
            off + 8
        };
        let Some(box_end) = usize::try_from(size).ok().and_then(|s| off.checked_add(s)) else {
            return found;
        };
        if size < 8 || box_end > end {
            return found;
        }
        found.push(BoxRef {
            kind,
            start: off,
            content,
            end: box_end,
        });
        off = box_end;
    }
    found
}

fn find(buf: &[u8], start: usize, end: usize, want: &[u8; 4]) -> Option<BoxRef> {
    boxes(buf, start, end).into_iter().find(|b| b.kind == *want)
}

/// A `trak` is the video track iff its `mdia/hdlr` handler_type is `vide`.
fn is_video_trak(buf: &[u8], content: usize, end: usize) -> bool {
    let Some(mdia) = find(buf, content, end, b"mdia") else {
        return false;
    };
    let Some(hdlr) = find(buf, mdia.content, mdia.end, b"hdlr") else {
        return false;
    };
    // hdlr content: version/flags(4) + pre_defined(4) + handler_type(4) ...
    let handler = hdlr.content + 8;
    buf.get(handler..handler + 4) == Some(&b"vide"[..])
}

fn spherical_xml() -> Vec<u8> {
    format!(
        concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#""#,
            r#" xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/">"#,
            "<GSpherical:Spherical>true</GSpherical:Spherical>",
            "<GSpherical:Stitched>true</GSpherical:Stitched>",
            "<GSpherical:StitchingSoftware>{software}</GSpherical:StitchingSoftware>",
            "<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>",
            "</rdf:SphericalVideo>",
        ),
        software = SOFTWARE
    )
    .into_bytes()
}

/// A `uuid` box: size(4) + `uuid`(4) + 16-byte UUID + payload.
fn uuid_box(payload: &[u8]) -> Vec<u8> {
    let size = (8 + SPHERICAL_UUID.len() + payload.len()) as u32;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(b"uuid");
    out.extend_from_slice(&SPHERICAL_UUID);
    out.extend_from_slice(payload);
    out
}

/// Add `delta` to a box's size field — the 32-bit one, or the 64-bit largesize when size == 1.
fn grow_size(buf: &mut [u8], start: usize, delta: u64) {
    let size = u32::from_be_bytes(buf[start..start + 4].try_into().unwrap());
    if size == 1 {
        let large = u64::from_be_bytes(buf[start + 8..start + 16].try_into().unwrap()) + delta;
        buf[start + 8..start + 16].copy_from_slice(&large.to_be_bytes());
    } else {
        buf[start..start + 4].copy_from_slice(&(size + delta as u32).to_be_bytes());
    }
}

/// Inject Spherical Video V1 metadata into `path`, in place.
///
/// Returns `true` when the box was written and `false` when the file already carried one, so
/// re-running a finalize step over an already-injected render is a no-op rather than a second box.
/// Errors on a file this cannot safely edit — no `moov`, no video `trak`, or a `moov` ahead of the
/// `mdat` it would shift.
///
/// The projection is always `equirectangular` and the stereo mode always mono: the only method that
/// would need otherwise, `ODS`, is not a [`RenderMethod`].
///
/// [`RenderMethod`]: pawa_schema::driver::RenderMethod
pub fn inject_spherical(path: &Path) -> Result<bool> {
    let mut buf = std::fs::read(path)?;
    let end = buf.len();

    let top = boxes(&buf, 0, end);
    let Some(moov) = top.iter().find(|b| b.kind == *b"moov").copied() else {
        bail!("no moov box found in {}", path.display());
    };
    // Safe-insert precondition: moov must be the LAST top-level box, i.e. after mdat, so growing it
    // never shifts mdat and the stco/co64 sample offsets stay valid.
    if moov.end != end || top.iter().any(|b| b.start > moov.start) {
        bail!(
            "moov is not the last top-level box of {} (faststart?); refusing to inject",
            path.display()
        );
    }

    let Some(trak) = boxes(&buf, moov.content, moov.end)
        .into_iter()
        .find(|b| b.kind == *b"trak" && is_video_trak(&buf, b.content, b.end))
    else {
        bail!("no video trak found in the moov of {}", path.display());
    };

    let already = boxes(&buf, trak.content, trak.end).into_iter().any(|b| {
        b.kind == *b"uuid" && buf.get(b.content..b.content + 16) == Some(&SPHERICAL_UUID[..])
    });
    if already {
        return Ok(false);
    }

    let injected = uuid_box(&spherical_xml());
    // Grow the ancestors first: their size fields sit BEFORE the insertion point, so their offsets
    // are still valid. Then splice the box in as the trak's last child.
    grow_size(&mut buf, trak.start, injected.len() as u64);
    grow_size(&mut buf, moov.start, injected.len() as u64);
    buf.splice(trak.end..trak.end, injected);
    std::fs::write(path, &buf)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// size(4) + type(4) + payload, the shape every box in the fixture takes.
    fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    /// The smallest file this injector accepts: `ftyp`, then `mdat`, then a `moov` holding one audio
    /// trak and one video trak. Two traks on purpose — picking the first one would pass a one-trak
    /// fixture and put the metadata on the wrong track in a real render.
    fn fixture() -> Vec<u8> {
        let mut hdlr_video = vec![0u8; 4]; // version/flags
        hdlr_video.extend_from_slice(&[0; 4]); // pre_defined
        hdlr_video.extend_from_slice(b"vide");
        let mut hdlr_audio = vec![0u8; 4];
        hdlr_audio.extend_from_slice(&[0; 4]);
        hdlr_audio.extend_from_slice(b"soun");

        let video_trak = boxed(b"trak", &boxed(b"mdia", &boxed(b"hdlr", &hdlr_video)));
        let audio_trak = boxed(b"trak", &boxed(b"mdia", &boxed(b"hdlr", &hdlr_audio)));

        let mut moov_payload = audio_trak;
        moov_payload.extend_from_slice(&video_trak);

        let mut file = boxed(b"ftyp", b"isom");
        file.extend_from_slice(&boxed(b"mdat", &[0xab; 32]));
        file.extend_from_slice(&boxed(b"moov", &moov_payload));
        file
    }

    #[test]
    fn injects_the_expected_bytes_into_the_video_trak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("equirect.mp4");
        let original = fixture();
        std::fs::write(&path, &original).unwrap();

        assert!(inject_spherical(&path).unwrap());
        let written = std::fs::read(&path).unwrap();

        // The exact box, byte for byte: 8-byte header, the V1 UUID, then the RDF payload. This is
        // the contract with every downstream player, so it is pinned rather than described.
        let payload = concat!(
            r#"<?xml version="1.0"?>"#,
            r#"<rdf:SphericalVideo xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#""#,
            r#" xmlns:GSpherical="http://ns.google.com/videos/1.0/spherical/">"#,
            "<GSpherical:Spherical>true</GSpherical:Spherical>",
            "<GSpherical:Stitched>true</GSpherical:Stitched>",
            "<GSpherical:StitchingSoftware>minecraft-renderer</GSpherical:StitchingSoftware>",
            "<GSpherical:ProjectionType>equirectangular</GSpherical:ProjectionType>",
            "</rdf:SphericalVideo>",
        )
        .as_bytes();
        let mut expected = (24u32 + payload.len() as u32).to_be_bytes().to_vec();
        expected.extend_from_slice(b"uuid");
        expected.extend_from_slice(&SPHERICAL_UUID);
        expected.extend_from_slice(payload);

        assert_eq!(written.len(), original.len() + expected.len());
        // It lands at the very end, which is the end of the video trak, which is the end of moov,
        // which is the end of the file — the whole reason this insertion is offset-safe.
        assert_eq!(&written[written.len() - expected.len()..], &expected[..]);

        // mdat is untouched and still where it was, so no stco/co64 offset needs rewriting.
        let mdat_at = original.windows(4).position(|w| w == b"mdat").unwrap();
        assert_eq!(&written[..mdat_at + 36], &original[..mdat_at + 36]);

        // Both ancestors grew by exactly the box, and nothing else did.
        let moov = boxes(&written, 0, written.len())
            .into_iter()
            .find(|b| b.kind == *b"moov")
            .unwrap();
        assert_eq!(moov.end, written.len());
        let traks: Vec<_> = boxes(&written, moov.content, moov.end)
            .into_iter()
            .filter(|b| b.kind == *b"trak")
            .collect();
        assert_eq!(traks.len(), 2);
        // trak(8) + mdia(8) + hdlr(8 + 12) = 36 bytes per trak in the fixture.
        assert_eq!(traks[0].end - traks[0].start, 36); // audio trak, unchanged
        assert_eq!(traks[1].end - traks[1].start, 36 + expected.len());
        assert!(is_video_trak(&written, traks[1].content, traks[1].end));
    }

    #[test]
    fn second_injection_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("equirect.mp4");
        std::fs::write(&path, fixture()).unwrap();

        assert!(inject_spherical(&path).unwrap());
        let once = std::fs::read(&path).unwrap();
        assert!(!inject_spherical(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), once);
    }

    #[test]
    fn refuses_a_faststart_layout() {
        // moov ahead of mdat: growing it would shift every sample without fixing the offsets that
        // point at them, so the file must come back untouched and the caller must hear about it.
        let mut file = boxed(b"ftyp", b"isom");
        let video_trak = boxed(
            b"trak",
            &boxed(b"mdia", &boxed(b"hdlr", b"\0\0\0\0\0\0\0\0vide")),
        );
        file.extend_from_slice(&boxed(b"moov", &video_trak));
        file.extend_from_slice(&boxed(b"mdat", &[0xab; 32]));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("faststart.mp4");
        std::fs::write(&path, &file).unwrap();
        assert!(inject_spherical(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), file);
    }
}
