//! The destination-filename encoding, and the control names this backend
//! reserves for itself.
//!
//! ## Why encode at all
//!
//! The set of legal filenames is NOT the same on the source and the
//! destination. A macOS or Linux source happily produces `a:b`, `report?.txt`,
//! `why|not`, `trailing.` or `ends with a space `, and every one of those is
//! REJECTED by exFAT/FAT32 (and by NTFS, and by Windows' path parser). A backup
//! destination that failed those files one by one would silently skip exactly
//! the files a user is most likely to have named badly.
//!
//! So Driven maps every name through [`encode`] before it reaches the
//! filesystem, and back through [`decode`] when reporting a name.
//!
//! ## Why encode UNCONDITIONALLY
//!
//! The encoding does NOT ask the destination which characters it accepts. It
//! applies the union of every restriction (POSIX, Windows, FAT/exFAT) to every
//! destination. That costs a little cosmetic ugliness on an APFS destination in
//! exchange for a property that matters much more for a backup: **the layout is
//! destination-independent**. A backup folder can be `cp -a`'d from an APFS disk
//! onto a FAT32 stick, or restored from either, with no renaming and no
//! re-upload - because the bytes on disk would have been identical either way.
//! A "only escape what this filesystem rejects" scheme would have produced two
//! incompatible trees for the same source.
//!
//! ## The scheme
//!
//! Percent-encoding of the offending UTF-8 bytes, `%` first so the encoding is
//! invertible:
//!
//! - `%` -> `%25` (the escape introducer; encoded first and always).
//! - The Windows/FAT reserved punctuation `/ \ : * ? " < > |` -> `%XX`.
//! - ASCII control bytes `0x00-0x1F` and `0x7F` -> `%XX`.
//! - A TRAILING `.` or space -> `%2E` / `%20`. Windows silently strips both,
//!   turning `report.` into `report` and colliding it with a real `report`.
//! - The whole-name specials `.` and `..` -> `%2E` / `%2E%2E`.
//! - A name that would shadow one of Driven's own control names (see
//!   [`is_reserved_control_name`]) gets its leading `.` escaped.
//! - Non-ASCII bytes pass through untouched: every filesystem Driven targets
//!   stores UTF-8 (or UTF-16) filenames, and mangling them would make the
//!   destination unbrowsable for the majority of the world's filenames.
//!
//! Because `%` is always escaped, a `%` in an ENCODED name is only ever the
//! start of a two-hex-digit escape. [`decode`] relies on that, and
//! `encode_decode_round_trips` pins it.
//!
//! ## Length
//!
//! Escaping can triple a name's byte length, and every filesystem Driven targets
//! caps a single component at 255. Anything over [`MAX_ENCODED_LEN`] is
//! truncated on a safe boundary and given a deterministic
//! `~<16 hex digits of md5(original)>` tail, so two long names that share a
//! prefix still land on different files and the same long name always lands on
//! the same file.

use md5::{Digest, Md5};

/// Directory holding the per-object metadata sidecars for one destination
/// directory. Reserved: [`encode`] guarantees no user filename can produce it.
pub const META_DIR: &str = ".driven-meta";

/// Filename extension of a metadata sidecar.
pub const META_EXT: &str = ".json";

/// Prefix of an in-progress temp file. These live in the SAME directory as
/// their eventual target so the committing `rename` is atomic (a rename across
/// filesystems is a copy, which is not).
pub const TMP_PREFIX: &str = ".driven-tmp-";

/// The destination-identity marker at the root of a configured destination.
/// See `crate::config::DestinationMarker`.
pub const MARKER_FILE: &str = ".driven-destination.json";

/// Prefix macOS gives an "AppleDouble" shadow file.
///
/// Discovered the hard way while round-tripping against a real exFAT and a real
/// FAT32 volume: on a filesystem with no native extended-attribute support,
/// macOS transparently writes the xattrs and resource fork of `X` into a
/// SIBLING file called `._X`. Driven never asks for this and cannot switch it
/// off, so a destination on a USB stick grows one `._` shadow per object - plus
/// `._.driven-meta` and `._.driven-destination.json` for Driven's own control
/// entries.
///
/// They are not objects. Left unfiltered they appeared in the destination
/// picker, doubled every `list_folder`, and - much worse - would have been
/// carried into the remote-existence audit as objects Driven owns with no
/// `file_state` row, which the audit tries to heal forever.
///
/// So `._` joins the reserved control prefixes: filtered out of every listing,
/// and escaped by [`encode`] so a user file genuinely named `._notes` is stored
/// as `%2E_notes` and can never be mistaken for one.
pub const APPLEDOUBLE_PREFIX: &str = "._";

/// Maximum length in bytes of an ENCODED name component, before the
/// truncate-and-hash fallback. Leaves room for the sidecar's `.json` suffix
/// inside the universal 255-byte component limit.
pub const MAX_ENCODED_LEN: usize = 200;

/// Bytes that must never reach the filesystem verbatim.
///
/// `%` is deliberately first in the doc order but is handled by the same table:
/// [`encode`] walks the input once and escapes any byte in this set, and `%`
/// being in the set is what makes the transform invertible.
const RESERVED: &[u8] = b"%/\\:*?\"<>|";

/// Is this one of Driven's own control names (which a user file must not be
/// allowed to shadow)?
///
/// Compared case-INSENSITIVELY because the destination may be, and a
/// `.DRIVEN-META` directory would shadow `.driven-meta` on exFAT.
pub fn is_reserved_control_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == META_DIR
        || lower == MARKER_FILE
        || lower.starts_with(TMP_PREFIX)
        || lower.starts_with(APPLEDOUBLE_PREFIX)
}

/// Encode one path COMPONENT for the destination filesystem.
///
/// Returns an error only for an empty name, which is a caller bug rather than a
/// filesystem-hostile filename (no filesystem can represent it, and no scanner
/// produces it).
pub fn encode(name: &str) -> anyhow::Result<String> {
    if name.is_empty() {
        anyhow::bail!("localfs.name_invalid: an object name must not be empty");
    }

    // Built as BYTES, not chars: a non-ASCII name is a multi-byte UTF-8
    // sequence and `byte as char` would map each byte through Latin-1 and
    // corrupt it. Every byte written here is either a preserved input byte or
    // ASCII, so the result is valid UTF-8 by construction.
    let mut buf: Vec<u8> = Vec::with_capacity(name.len());
    for &b in name.as_bytes() {
        if b < 0x20 || b == 0x7F || RESERVED.contains(&b) {
            buf.extend_from_slice(format!("%{b:02X}").as_bytes());
        } else {
            buf.push(b);
        }
    }
    let mut out = String::from_utf8(buf).expect("escaping only ever emits ASCII or input bytes");

    // `.` and `..` name the directory itself and its parent; neither can be a
    // file. Escape the dots rather than rejecting the file.
    if out == "." {
        out = "%2E".to_string();
    } else if out == ".." {
        out = "%2E%2E".to_string();
    }

    // Windows (and the FAT drivers that follow it) silently strip a trailing dot
    // or space, which would collide `report.` with `report`. Escaping the last
    // one is enough: the result then ends in a hex digit.
    if out.ends_with('.') {
        out.truncate(out.len() - 1);
        out.push_str("%2E");
    } else if out.ends_with(' ') {
        out.truncate(out.len() - 1);
        out.push_str("%20");
    }

    // Never let a user file shadow one of Driven's control names.
    if is_reserved_control_name(&out) {
        out = format!("%2E{}", &out[1..]);
    }

    if out.len() > MAX_ENCODED_LEN {
        out = truncate_with_digest(&out, name);
    }

    Ok(out)
}

/// Decode a name produced by [`encode`] back to the original.
///
/// A malformed escape (a `%` not followed by two hex digits) cannot come out of
/// [`encode`], so it means the name was written by something else; it is passed
/// through verbatim rather than failing, since the only consumer is a display
/// string.
pub fn decode(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &encoded[i + 1..i + 3];
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The deterministic disambiguated form of `encoded`, used when a DIFFERENT
/// original name already owns `encoded` on the destination (the case-insensitive
/// / Unicode-folding collision case).
///
/// `~<16 hex digits of md5(original)>` is inserted before the extension so the
/// file stays recognisable and keeps its type. Deterministic in `original`, so
/// re-running a backup after a crash picks the SAME file rather than accreting
/// one copy per attempt.
pub fn disambiguate(encoded: &str, original: &str) -> String {
    let tag = digest_tag(original);
    let (stem, ext) = split_extension(encoded);
    let mut out = format!("{stem}~{tag}{ext}");
    if out.len() > MAX_ENCODED_LEN {
        out = truncate_with_digest(&out, original);
    }
    out
}

/// First 16 hex digits of md5(`s`). Not a security boundary - this only has to
/// make two different filenames land on two different files.
fn digest_tag(s: &str) -> String {
    let mut h = Md5::new();
    h.update(s.as_bytes());
    let digest: [u8; 16] = h.finalize().into();
    hex::encode(&digest[..8])
}

/// Split an encoded name into `(stem, extension)`, where the extension includes
/// its leading dot. A leading dot (a dotfile) is never treated as an extension
/// separator.
fn split_extension(encoded: &str) -> (&str, &str) {
    match encoded.rfind('.') {
        Some(i) if i > 0 => encoded.split_at(i),
        _ => (encoded, ""),
    }
}

/// Truncate an over-long encoded name to [`MAX_ENCODED_LEN`] and append a
/// digest of the ORIGINAL so the result stays unique and deterministic.
///
/// Truncation never splits a `%XX` escape or a multi-byte UTF-8 sequence: it
/// backs off to the last byte boundary that is neither inside an escape nor
/// inside a code point.
fn truncate_with_digest(encoded: &str, original: &str) -> String {
    let tag = digest_tag(original);
    // 1 for `~` plus the tag.
    let budget = MAX_ENCODED_LEN.saturating_sub(tag.len() + 1);
    let mut cut = budget.min(encoded.len());
    while cut > 0 && !is_safe_cut(encoded, cut) {
        cut -= 1;
    }
    format!("{}~{}", &encoded[..cut], tag)
}

/// Is byte offset `cut` a safe place to truncate `encoded`: on a char boundary
/// and not in the middle of a `%XX` escape?
fn is_safe_cut(encoded: &str, cut: usize) -> bool {
    if !encoded.is_char_boundary(cut) {
        return false;
    }
    let b = encoded.as_bytes();
    // Cutting at `cut` keeps bytes [0, cut). An escape occupies three bytes
    // starting at a `%`; the cut must not land one or two bytes into one.
    if cut >= 1 && b[cut - 1] == b'%' {
        return false;
    }
    if cut >= 2 && b[cut - 2] == b'%' {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_pass_through_unchanged() {
        for name in [
            "report.txt",
            "a b c.pdf",
            "Ünïcödé.jpg",
            "日本語.txt",
            "_-+=~",
        ] {
            assert_eq!(encode(name).unwrap(), name, "{name} must not be mangled");
            assert_eq!(decode(&encode(name).unwrap()), name);
        }
    }

    #[test]
    fn filesystem_hostile_characters_are_escaped_and_round_trip() {
        let cases = [
            ("a:b", "a%3Ab"),
            ("what?.txt", "what%3F.txt"),
            ("a|b", "a%7Cb"),
            ("a*b", "a%2Ab"),
            ("a\"b", "a%22b"),
            ("a<b>c", "a%3Cb%3Ec"),
            ("a\\b", "a%5Cb"),
            ("a/b", "a%2Fb"),
            ("100%", "100%25"),
            ("bell\u{7}", "bell%07"),
            ("del\u{7f}", "del%7F"),
        ];
        for (raw, want) in cases {
            assert_eq!(encode(raw).unwrap(), want, "encoding {raw:?}");
            assert_eq!(decode(want), raw, "decoding {want:?}");
        }
    }

    #[test]
    fn trailing_dots_and_spaces_cannot_collide_after_windows_strips_them() {
        // `report.` and `report` are DIFFERENT source files but the same file on
        // Windows/FAT. Escaping the trailing character keeps them apart.
        assert_eq!(encode("report.").unwrap(), "report%2E");
        assert_eq!(encode("report ").unwrap(), "report%20");
        assert_ne!(
            encode("report.").unwrap(),
            encode("report").unwrap(),
            "a trailing dot must survive as a distinct destination name"
        );
        assert_eq!(decode("report%2E"), "report.");
        assert_eq!(decode("report%20"), "report ");
    }

    #[test]
    fn dot_and_dotdot_are_escaped() {
        assert_eq!(encode(".").unwrap(), "%2E");
        assert_eq!(encode("..").unwrap(), "%2E%2E");
        assert_eq!(decode("%2E"), ".");
        assert_eq!(decode("%2E%2E"), "..");
        // A dotfile is not special.
        assert_eq!(encode(".bashrc").unwrap(), ".bashrc");
    }

    #[test]
    fn no_user_name_can_shadow_a_driven_control_name() {
        for raw in [
            META_DIR,
            MARKER_FILE,
            ".driven-tmp-abc",
            ".DRIVEN-META",
            ".Driven-Tmp-9",
            // A user file whose name looks like a macOS AppleDouble shadow. If
            // it were stored verbatim, every listing and the audit would filter
            // the user's own file out as filesystem noise.
            "._notes.txt",
        ] {
            let enc = encode(raw).unwrap();
            assert!(
                !is_reserved_control_name(&enc),
                "{raw:?} encoded to {enc:?}, which still shadows a control name"
            );
            assert_eq!(decode(&enc), raw, "the escape must still round-trip");
        }
        // And the control names themselves are recognised.
        assert!(is_reserved_control_name(META_DIR));
        assert!(is_reserved_control_name(MARKER_FILE));
        assert!(is_reserved_control_name(".driven-tmp-1234"));
        assert!(
            is_reserved_control_name("._photo.jpg"),
            "macOS writes an AppleDouble shadow beside every object on exFAT/FAT32"
        );
        assert!(!is_reserved_control_name("driven-meta"));
        assert!(!is_reserved_control_name(".hidden"));
    }

    #[test]
    fn an_empty_name_is_a_caller_bug_not_an_encoding_problem() {
        assert!(encode("").is_err());
    }

    #[test]
    fn over_long_names_are_truncated_deterministically_and_stay_distinct() {
        let a = format!("{}.txt", "x".repeat(400));
        let b = format!("{}.txt", "y".repeat(400));
        let ea = encode(&a).unwrap();
        let eb = encode(&b).unwrap();
        assert!(ea.len() <= MAX_ENCODED_LEN, "len {}", ea.len());
        assert!(eb.len() <= MAX_ENCODED_LEN);
        assert_ne!(ea, eb);
        assert_eq!(ea, encode(&a).unwrap(), "truncation must be deterministic");

        // Truncation must never split a percent escape - a half escape would
        // decode to garbage and, worse, could re-introduce a reserved byte.
        let hostile = format!("{}.txt", "?".repeat(300));
        let enc = encode(&hostile).unwrap();
        assert!(enc.len() <= MAX_ENCODED_LEN);
        assert!(
            !enc.trim_end_matches(|c: char| c.is_ascii_hexdigit())
                .ends_with('%'),
            "truncated at a partial escape: {enc}"
        );
    }

    #[test]
    fn disambiguation_is_deterministic_and_keeps_the_extension() {
        let d1 = disambiguate("foo.txt", "Foo.txt");
        let d2 = disambiguate("foo.txt", "Foo.txt");
        assert_eq!(d1, d2);
        assert!(d1.ends_with(".txt"), "{d1}");
        assert!(d1.starts_with("foo~"), "{d1}");
        assert_ne!(d1, disambiguate("foo.txt", "FOO.txt"));
        // An extensionless name still gets a tail.
        let d3 = disambiguate("README", "readme");
        assert!(d3.starts_with("README~"), "{d3}");
        // A dotfile's leading dot is not an extension separator.
        let d4 = disambiguate(".env", ".ENV");
        assert!(d4.starts_with(".env~"), "{d4}");
    }

    #[test]
    fn encode_decode_round_trips_over_a_hostile_corpus() {
        let corpus = [
            "plain",
            "with space",
            "a:b*c?d\"e<f>g|h\\i/j",
            "%already%encoded%",
            "%2E",
            "trailing.",
            "trailing ",
            "..",
            "\u{1}\u{1f}\u{7f}",
            "emoji \u{1f600}.bin",
            META_DIR,
        ];
        for raw in corpus {
            let enc = encode(raw).unwrap();
            assert_eq!(decode(&enc), raw, "round trip failed for {raw:?}");
            // The encoded form must be free of every byte the destination could
            // reject.
            for &b in enc.as_bytes() {
                assert!(
                    b >= 0x20 && b != 0x7F && (b == b'%' || !RESERVED.contains(&b)),
                    "encoded {enc:?} still contains a reserved byte {b:#04x}"
                );
            }
        }
    }
}
