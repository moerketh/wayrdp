//! Pure clipboard format conversions between cliprdr (MS-RDPECLIP) and the
//! Wayland MIME world. No FreeRDP or Wayland types here, so everything is
//! unit-tested on any platform; `cliprdr.rs` does the wire glue.

use std::path::{Component, Path, PathBuf};

/// Standard clipboard format ids (winuser.h).
pub const CF_TEXT: u32 = 1;
pub const CF_OEMTEXT: u32 = 7;
pub const CF_UNICODETEXT: u32 = 13;

/// Registered (named) formats for file transfer. The ids are ours to pick
/// (>= 0xC000); the peer maps them by NAME, never by number.
pub const FILEGROUPDESCRIPTORW_NAME: &str = "FileGroupDescriptorW";
pub const FILECONTENTS_NAME: &str = "FileContents";
pub const SERVER_FORMAT_FILEGROUPDESCRIPTORW: u32 = 0xC0BC;
pub const SERVER_FORMAT_FILECONTENTS: u32 = 0xC0BD;

/// Text MIME types we offer to Wayland for remote text, best first.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
pub const URI_LIST_MIME: &str = "text/uri-list";
/// Nautilus / GTK file managers paste from this one, not text/uri-list.
pub const GNOME_COPIED_FILES_MIME: &str = "x-special/gnome-copied-files";
/// Marker MIME on our own data source so the data-control echo of our own
/// selection is recognised and not bounced back to the client.
pub const OWNER_MARKER_MIME: &str = "application/x-wayrdp-remote";

/// FILEDESCRIPTORW is 592 bytes on the wire (MS-RDPECLIP 2.2.5.2.3.1).
pub const FILEDESCRIPTORW_LEN: usize = 592;
const FD_ATTRIBUTES: u32 = 0x0000_0004;
const FD_WRITESTIME: u32 = 0x0000_0020;
const FD_FILESIZE: u32 = 0x0000_0040;
const FD_SHOWPROGRESSUI: u32 = 0x0000_4000;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// cFileName is WCHAR[260] including the terminator.
const MAX_NAME_UNITS: usize = 259;
/// Seconds between 1601-01-01 (FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET_SECS: u64 = 11_644_473_600;

/// Pick the best text MIME out of a local offer, if any.
pub fn best_text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES
        .iter()
        .find(|want| mimes.iter().any(|m| m.eq_ignore_ascii_case(want)))
        .copied()
        .or_else(|| {
            mimes
                .iter()
                .find(|m| m.to_ascii_lowercase().starts_with("text/plain"))
                .map(String::as_str)
        })
}

/// Pick the best file-list MIME out of a local offer, if any.
pub fn best_files_mime(mimes: &[String]) -> Option<&'static str> {
    [URI_LIST_MIME, GNOME_COPIED_FILES_MIME]
        .into_iter()
        .find(|want| mimes.iter().any(|m| m == want))
}

/// Whether `mime` is one of the text types we serve for remote text.
pub fn is_text_mime(mime: &str) -> bool {
    TEXT_MIMES.iter().any(|m| m.eq_ignore_ascii_case(mime))
        || mime.to_ascii_lowercase().starts_with("text/plain")
}

/// CF_UNICODETEXT payload (UTF-16LE, NUL-terminated, CRLF) → UTF-8 with LF.
pub fn utf16le_to_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units).replace("\r\n", "\n")
}

/// UTF-8 text → CF_UNICODETEXT payload: CRLF line ends, UTF-16LE, NUL.
pub fn text_to_utf16le(text: &str) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut out = Vec::with_capacity(normalized.len() * 2 + 2);
    for unit in normalized.encode_utf16().chain(std::iter::once(0)) {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// CF_TEXT / CF_OEMTEXT payload → UTF-8. These are code-page text; we only
/// ever request them when the client offers no CF_UNICODETEXT, so a lossy
/// Latin-1-ish read is the honest best effort.
pub fn ansi_to_text(data: &[u8]) -> String {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    match std::str::from_utf8(&data[..end]) {
        Ok(s) => s.replace("\r\n", "\n"),
        Err(_) => data[..end]
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .replace("\r\n", "\n"),
    }
}

/// One entry of a FILEGROUPDESCRIPTORW list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDescriptor {
    /// Relative path with `\` separators, as it travels on the wire.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// FILETIME (100 ns ticks since 1601), when known.
    pub last_write: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FormatError {
    Truncated,
    NameTooLong(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Truncated => write!(f, "truncated FILEGROUPDESCRIPTORW"),
            FormatError::NameTooLong(n) => write!(f, "file name exceeds 259 UTF-16 units: {n}"),
        }
    }
}

impl std::error::Error for FormatError {}

/// Encode a FILEGROUPDESCRIPTORW (cItems + N × FILEDESCRIPTORW).
pub fn encode_file_group_descriptor(files: &[FileDescriptor]) -> Result<Vec<u8>, FormatError> {
    let mut out = Vec::with_capacity(4 + files.len() * FILEDESCRIPTORW_LEN);
    out.extend_from_slice(&(files.len() as u32).to_le_bytes());
    for f in files {
        let name: Vec<u16> = f.name.encode_utf16().collect();
        if name.len() > MAX_NAME_UNITS {
            return Err(FormatError::NameTooLong(f.name.clone()));
        }
        let mut flags = FD_ATTRIBUTES | FD_SHOWPROGRESSUI;
        if !f.is_dir {
            flags |= FD_FILESIZE;
        }
        if f.last_write.is_some() {
            flags |= FD_WRITESTIME;
        }
        let start = out.len();
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]); // clsid
        out.extend_from_slice(&[0u8; 8]); // sizel
        out.extend_from_slice(&[0u8; 8]); // pointl
        let attrs = if f.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_ARCHIVE
        };
        out.extend_from_slice(&attrs.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]); // ftCreationTime
        out.extend_from_slice(&[0u8; 8]); // ftLastAccessTime
        out.extend_from_slice(&f.last_write.unwrap_or(0).to_le_bytes());
        let size = if f.is_dir { 0 } else { f.size };
        out.extend_from_slice(&((size >> 32) as u32).to_le_bytes());
        out.extend_from_slice(&(size as u32).to_le_bytes());
        for unit in &name {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out.resize(start + FILEDESCRIPTORW_LEN, 0);
    }
    Ok(out)
}

/// Decode a FILEGROUPDESCRIPTORW.
pub fn decode_file_group_descriptor(data: &[u8]) -> Result<Vec<FileDescriptor>, FormatError> {
    let count = u32::from_le_bytes(
        data.get(0..4)
            .ok_or(FormatError::Truncated)?
            .try_into()
            .expect("4 bytes"),
    ) as usize;
    let mut files = Vec::with_capacity(count.min(4096));
    for i in 0..count {
        let off = 4 + i * FILEDESCRIPTORW_LEN;
        let d = data
            .get(off..off + FILEDESCRIPTORW_LEN)
            .ok_or(FormatError::Truncated)?;
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().expect("4 bytes"));
        let flags = u32_at(0);
        let attrs = u32_at(36);
        let last_write = u64::from_le_bytes(d[56..64].try_into().expect("8 bytes"));
        let size = (u64::from(u32_at(64)) << 32) | u64::from(u32_at(68));
        let units: Vec<u16> = d[72..72 + 520]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&u| u != 0)
            .collect();
        files.push(FileDescriptor {
            name: String::from_utf16_lossy(&units),
            is_dir: flags & FD_ATTRIBUTES != 0 && attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            size,
            last_write: (flags & FD_WRITESTIME != 0 && last_write != 0).then_some(last_write),
        });
    }
    Ok(files)
}

/// Turn a wire name (`dir\sub\file.txt`) into a safe RELATIVE path, or
/// `None` when it tries to escape (`..`, absolute, drive letters, NUL).
/// The host controls these names: a `..\..\.bashrc` must never land
/// outside the staging directory.
pub fn sanitize_relative(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('\0') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split(['\\', '/']) {
        if part.is_empty() || part == "." || part == ".." || part.contains(':') {
            return None;
        }
        out.push(part);
    }
    // Belt and braces: the joined result must be purely Normal components.
    if out.components().all(|c| matches!(c, Component::Normal(_))) {
        Some(out)
    } else {
        None
    }
}

/// Parse a `text/uri-list` (RFC 2483) or `x-special/gnome-copied-files`
/// payload into local paths. Non-`file:` URIs and remote hosts are skipped.
pub fn parse_uri_list(data: &str) -> Vec<PathBuf> {
    data.lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        // gnome-copied-files prefixes the list with an action line.
        .filter(|l| *l != "copy" && *l != "cut")
        .filter_map(|l| {
            let rest = l.strip_prefix("file://")?;
            let path = if rest.starts_with('/') {
                rest
            } else {
                // file://host/path — only accept localhost.
                let slash = rest.find('/')?;
                if &rest[..slash] != "localhost" {
                    return None;
                }
                &rest[slash..]
            };
            percent_decode(path).map(PathBuf::from)
        })
        .collect()
}

/// Build a `text/uri-list` payload (CRLF line ends per RFC 2483).
pub fn build_uri_list(paths: &[PathBuf]) -> String {
    let mut out = String::new();
    for p in paths {
        out.push_str(&file_uri(p));
        out.push_str("\r\n");
    }
    out
}

/// Build an `x-special/gnome-copied-files` payload.
pub fn build_gnome_copied_files(paths: &[PathBuf]) -> String {
    let mut out = String::from("copy");
    for p in paths {
        out.push('\n');
        out.push_str(&file_uri(p));
    }
    out
}

fn file_uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut s = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Unix seconds + nanos → FILETIME.
pub fn unix_to_filetime(secs: u64, nanos: u32) -> u64 {
    (secs + FILETIME_UNIX_OFFSET_SECS) * 10_000_000 + u64::from(nanos) / 100
}

/// FILETIME → Unix seconds (saturating at the epoch).
pub fn filetime_to_unix(ft: u64) -> u64 {
    (ft / 10_000_000).saturating_sub(FILETIME_UNIX_OFFSET_SECS)
}

/// One local file or directory offered to the client, with its wire name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalEntry {
    pub descriptor: FileDescriptor,
    pub path: PathBuf,
}

/// Walk the copied paths into a flat descriptor list: every directory is an
/// entry followed by its contents (`dir`, `dir\a.txt`, `dir\sub`, ...), the
/// order Windows Explorer expects. Symlinked directories are listed but not
/// descended (loops); unreadable entries are skipped. Stops at `max_entries`.
pub fn collect_local_entries(roots: &[PathBuf], max_entries: usize) -> Vec<LocalEntry> {
    let mut out = Vec::new();
    for root in roots {
        let Some(base) = root.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        walk(root, base.to_string(), &mut out, max_entries);
    }
    out
}

fn walk(path: &Path, name: String, out: &mut Vec<LocalEntry>, max: usize) {
    if out.len() >= max {
        return;
    }
    let Ok(lmeta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let meta = if lmeta.file_type().is_symlink() {
        match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return,
        }
    } else {
        lmeta.clone()
    };
    let last_write = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| unix_to_filetime(d.as_secs(), d.subsec_nanos()));
    out.push(LocalEntry {
        descriptor: FileDescriptor {
            name: name.clone(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            last_write,
        },
        path: path.to_path_buf(),
    });
    if meta.is_dir() && !lmeta.file_type().is_symlink() {
        let Ok(rd) = std::fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            if let Some(cn) = child.file_name().to_str() {
                walk(&child.path(), format!("{name}\\{cn}"), out, max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_text_roundtrip_normalises_line_ends() {
        let wire = text_to_utf16le("héllo\nwörld ✓\r\nend");
        // CRLF on the wire, NUL-terminated.
        let units: Vec<u16> = wire
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(*units.last().unwrap(), 0);
        assert_eq!(
            String::from_utf16(&units[..units.len() - 1]).unwrap(),
            "héllo\r\nwörld ✓\r\nend"
        );
        assert_eq!(utf16le_to_text(&wire), "héllo\nwörld ✓\nend");
    }

    #[test]
    fn unicode_text_stops_at_first_nul_and_keeps_astral_chars() {
        let mut wire = text_to_utf16le("a😀b");
        wire.extend_from_slice(&[0x41, 0x00]); // garbage after the NUL
        assert_eq!(utf16le_to_text(&wire), "a😀b");
    }

    #[test]
    fn ansi_text_falls_back_to_latin1() {
        assert_eq!(ansi_to_text(b"caf\xe9\r\nx\0junk"), "café\nx");
    }

    #[test]
    fn best_mime_choice() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            best_text_mime(&m(&["TEXT", "text/plain;charset=utf-8"])),
            Some("text/plain;charset=utf-8")
        );
        assert_eq!(best_text_mime(&m(&["image/png"])), None);
        assert_eq!(
            best_files_mime(&m(&["x-special/gnome-copied-files", "text/uri-list"])),
            Some(URI_LIST_MIME)
        );
    }

    #[test]
    fn file_group_descriptor_roundtrip() {
        let files = vec![
            FileDescriptor {
                name: "dir".into(),
                is_dir: true,
                size: 0,
                last_write: Some(unix_to_filetime(1_700_000_000, 0)),
            },
            FileDescriptor {
                name: "dir\\ünï.bin".into(),
                is_dir: false,
                size: 5 * 1024 * 1024 * 1024 + 7, // > 4 GiB: both size halves
                last_write: None,
            },
        ];
        let wire = encode_file_group_descriptor(&files).unwrap();
        assert_eq!(wire.len(), 4 + 2 * FILEDESCRIPTORW_LEN);
        assert_eq!(decode_file_group_descriptor(&wire).unwrap(), files);
    }

    #[test]
    fn file_group_descriptor_rejects_truncation_and_long_names() {
        let wire = encode_file_group_descriptor(&[FileDescriptor {
            name: "a".into(),
            is_dir: false,
            size: 1,
            last_write: None,
        }])
        .unwrap();
        assert_eq!(
            decode_file_group_descriptor(&wire[..wire.len() - 1]),
            Err(FormatError::Truncated)
        );
        let long = "x".repeat(260);
        assert!(matches!(
            encode_file_group_descriptor(&[FileDescriptor {
                name: long,
                is_dir: false,
                size: 0,
                last_write: None,
            }]),
            Err(FormatError::NameTooLong(_))
        ));
    }

    #[test]
    fn sanitize_relative_blocks_escapes() {
        assert_eq!(
            sanitize_relative("dir\\sub\\f.txt"),
            Some(PathBuf::from("dir/sub/f.txt"))
        );
        for bad in [
            "..\\..\\.bashrc",
            "dir\\..\\x",
            "\\abs",
            "/abs",
            "C:\\x",
            "a\\\\b",
            "",
            "a\0b",
            ".",
        ] {
            assert_eq!(sanitize_relative(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn uri_list_roundtrip_with_escapes() {
        let paths = vec![
            PathBuf::from("/home/u/my file.txt"),
            PathBuf::from("/home/u/ünï/€.bin"),
        ];
        let list = build_uri_list(&paths);
        assert!(list.starts_with("file:///home/u/my%20file.txt\r\n"));
        assert_eq!(parse_uri_list(&list), paths);
        assert_eq!(parse_uri_list(&build_gnome_copied_files(&paths)), paths);
    }

    #[test]
    fn uri_list_skips_comments_remote_hosts_and_other_schemes() {
        let list = "# comment\r\nfile://localhost/a\r\nfile://evil/b\r\nhttps://x/y\r\nfile:///c\n";
        assert_eq!(
            parse_uri_list(list),
            vec![PathBuf::from("/a"), PathBuf::from("/c")]
        );
    }

    #[test]
    fn filetime_conversion() {
        let ft = unix_to_filetime(0, 0);
        assert_eq!(ft, 116_444_736_000_000_000);
        assert_eq!(filetime_to_unix(ft), 0);
        assert_eq!(filetime_to_unix(unix_to_filetime(1_700_000_000, 999)), 1_700_000_000);
    }

    #[test]
    fn collect_local_entries_walks_dirs_in_explorer_order() {
        let root = std::env::temp_dir().join(format!("wayrdp-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("top/sub")).unwrap();
        std::fs::write(root.join("top/a.txt"), b"abc").unwrap();
        std::fs::write(root.join("top/sub/z.bin"), b"").unwrap();
        std::fs::write(root.join("single.txt"), b"12345").unwrap();

        let entries = collect_local_entries(&[root.join("top"), root.join("single.txt")], 100);
        let names: Vec<_> = entries
            .iter()
            .map(|e| (e.descriptor.name.as_str(), e.descriptor.is_dir, e.descriptor.size))
            .collect();
        assert_eq!(
            names,
            vec![
                ("top", true, 0),
                ("top\\a.txt", false, 3),
                ("top\\sub", true, 0),
                ("top\\sub\\z.bin", false, 0),
                ("single.txt", false, 5),
            ]
        );
        assert_eq!(collect_local_entries(&[root.join("top")], 2).len(), 2);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
