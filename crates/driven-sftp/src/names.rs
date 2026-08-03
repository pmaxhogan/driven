//! The remote filename encoding, and the control names this backend reserves
//! for itself.
//!
//! ## Why encode at all, when the remote is "just Linux"
//!
//! It very often is not. An SFTP destination is whatever box the user typed a
//! hostname for: a Debian VPS, a Synology or QNAP NAS (whose shares are
//! routinely ext4 *or* exFAT), a TrueNAS pool, or Windows OpenSSH serving
//! NTFS. A macOS or Linux SOURCE happily produces `a:b`, `report?.txt`,
//! `why|not`, `trailing.` or `ends with a space `, and every one of those is
//! rejected by NTFS/exFAT and by Windows' path parser.
//!
//! So Driven maps every name through [`encode`] before it reaches the wire,
//! and back through [`decode`] when reporting a name - the same scheme
//! `driven-localfs` applies to a local destination, and deliberately the same
//! BYTES. That is the property worth having: a backup tree written to an SFTP
//! server and one written to a USB stick are identical, so a user can `rsync`
//! one onto the other, or restore from either, with no renaming and no
//! re-upload.
//!
//! This is a verbatim-in-spirit copy of `driven_localfs::names` rather than a
//! shared crate, matching the established call in this plan for
//! `keyring_off_runtime` (a third copy is the pattern; do NOT extract). The one
//! deliberate difference from the local-folder original is:
//!
//! - a metadata sidecar is a SUFFIXED SIBLING (`.<stored>.driven-meta`) rather
//!   than a file inside a `.driven-meta/` directory, because one flat
//!   namespace costs one round trip per lookup instead of two. That makes the
//!   reserved control shape a SUFFIX, which [`encode`] must escape - see
//!   [`META_SUFFIX`].
//!
//! ## The scheme
//!
//! Percent-encoding of the offending UTF-8 bytes, `%` first so the encoding is
//! invertible:
//!
//! - `%` -> `%25` (the escape introducer; encoded first and always).
//! - The Windows/NTFS/exFAT reserved punctuation `/ \ : * ? " < > |` -> `%XX`.
//! - ASCII control bytes `0x00-0x1F` and `0x7F` -> `%XX`.
//! - A TRAILING `.` or space -> `%2E` / `%20`. Windows silently strips both,
//!   turning `report.` into `report` and colliding it with a real `report`.
//! - The whole-name specials `.` and `..` -> `%2E` / `%2E%2E`.
//! - A name that would shadow one of Driven's own control names (see
//!   [`is_reserved_control_name`]) is escaped so it cannot.
//! - A name whose stem is a WINDOWS DEVICE NAME (`CON`, `PRN`, `AUX`, `NUL`,
//!   `COM1`-`COM9`, `LPT1`-`LPT9`) gets its first character escaped. Windows
//!   OpenSSH is a supported server, and `NUL.txt` there is not merely rejected -
//!   it RESOLVES TO A DEVICE, so a write reports success and discards the bytes.
//! - Non-ASCII bytes pass through untouched.
//!
//! Because `%` is always escaped, a `%` in an ENCODED name is only ever the
//! start of a two-hex-digit escape, which is what makes [`decode`] total.

use md5::{Digest, Md5};

/// Suffix of a per-object metadata sidecar: the sidecar for the object stored
/// as `<stored>` is `.<stored>.driven-meta` in the same remote directory.
///
/// Because sidecars and data objects share ONE namespace here (unlike the
/// local-folder backend's `.driven-meta/` directory), this suffix is reserved:
/// [`encode`] guarantees no encoded object name ends with it, so a user file
/// genuinely called `notes.driven-meta` is stored as `notes%2Edriven-meta` and
/// can never be filtered out of a listing as if it were Driven's own metadata.
pub const META_SUFFIX: &str = ".driven-meta";

/// Prefix of an in-progress temp file. These live in the SAME remote directory
/// as their eventual target so the committing `SSH_FXP_RENAME` stays within one
/// filesystem.
pub const TMP_PREFIX: &str = ".driven-tmp-";

/// The destination-identity marker at the account's `root_path`.
///
/// **Byte-identical to `driven_localfs::names::MARKER_FILE` on purpose**, so a
/// backup tree stays interchangeable between an SFTP server and a local folder.
///
/// The hazard it defends against exists verbatim server-side, and is if
/// anything sharper over SSH than over a USB port: `root_path` is a string the
/// user typed, and a directory that holds the user's OWN data is
/// indistinguishable from an initialized-but-empty destination by inspection
/// alone. Without the marker, `SftpStore`'s adopt-an-unannotated-name path and
/// its remove-then-rename commit would quietly destroy same-named files the
/// user put there. It also catches a server-side mount (a NAS's external
/// volume, an unmounted array) that is not present this cycle - the exact
/// analogue of the unplugged stick.
pub const MARKER_FILE: &str = ".driven-destination.json";

/// Prefix macOS gives an "AppleDouble" shadow file.
///
/// Reserved for the same reason `driven-localfs` reserves it: a NAS share is
/// commonly mounted by a Mac as well as served over SFTP, and macOS writes the
/// xattrs of `X` into a sibling `._X` on any filesystem without native xattr
/// support. Those are not objects - left unfiltered they would double every
/// listing and be carried into the remote-existence audit as objects Driven
/// owns with no `file_state` row.
pub const APPLEDOUBLE_PREFIX: &str = "._";

/// Maximum length in bytes of an ENCODED name component, before the
/// truncate-and-hash fallback.
///
/// The universal single-component limit is 255 bytes and a sidecar adds a
/// leading `.` plus [`META_SUFFIX`] (13 bytes) to whatever this produces, so
/// 200 leaves room for both with margin.
pub const MAX_ENCODED_LEN: usize = 200;

/// Bytes that must never reach a remote filesystem verbatim.
const RESERVED: &[u8] = b"%/\\:*?\"<>|";

/// The MS-DOS device names Windows still reserves in every directory.
const DOS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Is this one of Driven's own control names (which a user file must not be
/// allowed to shadow, and which never appears in a listing as an object)?
///
/// Compared case-INSENSITIVELY because the remote filesystem may be, and a
/// `.X.DRIVEN-META` sidecar would shadow `.x.driven-meta` on NTFS or exFAT.
pub fn is_reserved_control_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == MARKER_FILE
        || lower.ends_with(META_SUFFIX)
        || lower.starts_with(TMP_PREFIX)
        || lower.starts_with(APPLEDOUBLE_PREFIX)
}

/// Does `name` resolve to an MS-DOS device on Windows?
///
/// Windows compares the STEM (everything before the first `.`) case-
/// insensitively and after stripping trailing dots and spaces - so `CON`,
/// `con.txt`, `CON.`, `con ` and `COM1.log` all name the same device. The check
/// is unconditional, not `cfg(windows)`: the destination-independence invariant
/// is the whole point of the encoding, and a tree written against a Linux
/// server containing `CON.txt` must be restorable onto a Windows one.
pub fn is_dos_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches([' ', '.']).to_ascii_lowercase();
    DOS_DEVICE_NAMES.contains(&stem.as_str())
}

/// A fresh, reserved temp filename for an in-flight write.
///
/// Deliberately UUID-shaped: it contains no `:`, which some SFTP servers (and
/// this crate's test fixture, which is stricter than sshd on purpose) refuse in
/// a path segment, and no character [`encode`] would have had to escape.
pub fn temp_name() -> String {
    format!("{TMP_PREFIX}{}", uuid::Uuid::new_v4())
}

/// Encode one path COMPONENT for the remote filesystem.
///
/// Returns an error only for an empty name, which is a caller bug rather than a
/// hostile filename (no filesystem can represent it, and no scanner produces
/// it).
pub fn encode(name: &str) -> anyhow::Result<String> {
    if name.is_empty() {
        anyhow::bail!("sftp.name_invalid: an object name must not be empty");
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

    // Windows (and NTFS/exFAT drivers generally) silently strip a trailing dot
    // or space, which would collide `report.` with `report`. Escaping the last
    // one is enough: the result then ends in a hex digit.
    if out.ends_with('.') {
        out.truncate(out.len() - 1);
        out.push_str("%2E");
    } else if out.ends_with(' ') {
        out.truncate(out.len() - 1);
        out.push_str("%20");
    }

    // A name colliding with the sidecar SUFFIX is fixed at the suffix, not at
    // the front: escaping the leading character of `notes.driven-meta` would
    // leave it still ending in `.driven-meta` and still invisible to every
    // listing. Escape the `.` that begins the suffix instead.
    if out.to_ascii_lowercase().ends_with(META_SUFFIX) {
        let cut = out.len() - META_SUFFIX.len();
        out = format!("{}%2E{}", &out[..cut], &out[cut + 1..]);
    }

    // The remaining control shapes are PREFIXES, and a device name is decided
    // by the stem; escaping the first character fixes both. Read the byte from
    // the string rather than assuming `.` - a device name can start with any
    // letter.
    if is_reserved_control_name(&out) || is_dos_device_name(&out) {
        let mut chars = out.chars();
        let first = chars
            .next()
            .expect("a non-empty name has a first character");
        let rest: String = chars.collect();
        let escaped: String = first
            .to_string()
            .bytes()
            .map(|b| format!("%{b:02X}"))
            .collect();
        out = format!("{escaped}{rest}");
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
/// original name already owns `encoded` on the remote (the case-insensitive /
/// Unicode-folding collision case a Synology or Windows server produces).
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
    fn an_encoded_name_never_contains_a_colon() {
        // Not cosmetic: a `:` in a path segment is refused outright by this
        // crate's test fixture (deliberately stricter than sshd) and is illegal
        // on any Windows-served share. The encoding is what makes that a
        // non-issue rather than a class of files that silently fails to back up.
        for raw in ["a:b", "12:34:56.log", "C:file"] {
            let enc = encode(raw).unwrap();
            assert!(!enc.contains(':'), "{raw:?} encoded to {enc:?}");
            assert_eq!(decode(&enc), raw);
        }
    }

    #[test]
    fn trailing_dots_and_spaces_cannot_collide_after_windows_strips_them() {
        assert_eq!(encode("report.").unwrap(), "report%2E");
        assert_eq!(encode("report ").unwrap(), "report%20");
        assert_ne!(
            encode("report.").unwrap(),
            encode("report").unwrap(),
            "a trailing dot must survive as a distinct remote name"
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
    fn a_user_file_named_like_a_sidecar_stays_visible() {
        // THE flat-namespace hazard. Sidecars are siblings of their objects
        // here, so `list_folder` filters on the `.driven-meta` SUFFIX. Without
        // an escape, a user's own `notes.driven-meta` would be filtered out of
        // every listing and every audit - backed up once and then invisible
        // forever.
        for raw in [
            "notes.driven-meta",
            ".driven-meta",
            "REPORT.DRIVEN-META",
            ".a.driven-meta",
        ] {
            let enc = encode(raw).unwrap();
            assert!(
                !is_reserved_control_name(&enc),
                "{raw:?} encoded to {enc:?}, which a listing would still hide"
            );
            assert_eq!(decode(&enc), raw, "the escape must still round-trip");
        }
    }

    #[test]
    fn the_marker_filename_matches_the_local_folder_backend_byte_for_byte() {
        // A backup tree has to stay interchangeable between an SFTP server and
        // a USB stick, which it cannot be if the two backends disagree about
        // what the destination marker is called.
        assert_eq!(MARKER_FILE, driven_localfs::names::MARKER_FILE);
        assert!(is_reserved_control_name(MARKER_FILE));
        assert!(is_reserved_control_name(".DRIVEN-DESTINATION.JSON"));
    }

    #[test]
    fn no_user_name_can_shadow_a_driven_control_name() {
        for raw in [
            ".driven-tmp-abc",
            ".Driven-Tmp-9",
            // A user file whose name looks like a macOS AppleDouble shadow.
            "._notes.txt",
            // Both shapes at once: a tmp-prefixed name that also ends in the
            // sidecar suffix.
            ".driven-tmp-x.driven-meta",
            // The destination marker. If a user file could be stored under this
            // name it would overwrite the very thing that proves the directory
            // is a Driven destination.
            MARKER_FILE,
            ".DRIVEN-DESTINATION.JSON",
        ] {
            let enc = encode(raw).unwrap();
            assert!(
                !is_reserved_control_name(&enc),
                "{raw:?} encoded to {enc:?}, which still shadows a control name"
            );
            assert_eq!(decode(&enc), raw, "the escape must still round-trip");
        }
        // And the control names themselves are recognised.
        assert!(is_reserved_control_name(".report.txt.driven-meta"));
        assert!(is_reserved_control_name(".driven-tmp-1234"));
        assert!(is_reserved_control_name(&temp_name()));
        assert!(
            is_reserved_control_name("._photo.jpg"),
            "a Mac mounting the same share writes an AppleDouble beside every object"
        );
        assert!(!is_reserved_control_name("driven-meta"));
        assert!(!is_reserved_control_name(".hidden"));
    }

    #[test]
    fn windows_device_names_are_escaped_so_a_write_can_never_hit_a_device() {
        // Windows OpenSSH is a supported server. `NUL.txt` there OPENS THE NULL
        // DEVICE: the write succeeds and the bytes are discarded.
        for raw in [
            "CON", "con", "nul.txt", "NUL", "COM1", "com9.log", "LPT9.log", "aux", "PRN.dat",
            "CON ", "con.",
        ] {
            let enc = encode(raw).unwrap();
            assert!(
                !is_dos_device_name(&enc),
                "{raw:?} encoded to {enc:?}, which still names an MS-DOS device"
            );
            assert_eq!(decode(&enc), raw, "the escape must still round-trip");
        }

        for ok in [
            "console.log",
            "conf",
            "nullable.rs",
            "com10",
            "lpt0",
            "my-con",
        ] {
            assert!(!is_dos_device_name(ok), "{ok:?} is not a device");
            assert_eq!(encode(ok).unwrap(), ok);
        }
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
        // The sidecar affix has to fit inside the universal 255-byte component
        // limit on top of the encoded name.
        assert!(ea.len() + 1 + META_SUFFIX.len() <= 255);

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
        let d3 = disambiguate("README", "readme");
        assert!(d3.starts_with("README~"), "{d3}");
        let d4 = disambiguate(".env", ".ENV");
        assert!(d4.starts_with(".env~"), "{d4}");
    }

    #[test]
    fn temp_names_are_reserved_and_unique() {
        let a = temp_name();
        let b = temp_name();
        assert_ne!(a, b);
        assert!(is_reserved_control_name(&a));
        assert!(!a.contains(':'), "{a}");
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
            ".driven-meta",
            "notes.driven-meta",
            ".driven-tmp-1",
            "._shadow",
            ".driven-destination.json",
            "CON",
            "nul.txt",
            "COM1",
            "LPT9.log",
            "aux",
            "CON ",
        ];
        for raw in corpus {
            let enc = encode(raw).unwrap();
            assert_eq!(decode(&enc), raw, "round trip failed for {raw:?}");
            assert!(
                !is_dos_device_name(&enc),
                "encoded {enc:?} still names an MS-DOS device"
            );
            assert!(
                !is_reserved_control_name(&enc),
                "encoded {enc:?} still shadows a Driven control name"
            );
            for &b in enc.as_bytes() {
                assert!(
                    b >= 0x20 && b != 0x7F && (b == b'%' || !RESERVED.contains(&b)),
                    "encoded {enc:?} still contains a reserved byte {b:#04x}"
                );
            }
        }
    }
}
