//! Conversion between LSP `file:` URIs and filesystem paths.
//!
//! [`gen_lsp_types::Uri`] is an opaque string here (the crate only parses it under its optional
//! `url`/`fluent-uri` features), so the server does its own percent-coding. Paths, not URIs, are the
//! workspace's key: a client's spelling of a URI need not match the one the server builds while
//! scanning the disk — percent-encoding and Windows drive-letter case both vary — and comparing
//! decoded paths keeps the two spellings from looking like different files.

use std::path::{Path, PathBuf};

use gen_lsp_types::Uri;

/// The local path a `file:` URI refers to, or `None` for any other scheme or a remote authority.
pub fn to_path(uri: &Uri) -> Option<PathBuf> {
    let rest = uri.0.strip_prefix("file://")?;

    // The slot between `file://` and the path is the authority. Empty (`file:///a/b`) means the
    // local machine; anything else names a remote host that has no path on this filesystem.
    if !rest.starts_with('/') {
        return None;
    }

    let decoded = percent_decode(rest);

    // Windows spells the drive as `file:///C:/…`, where the leading slash is URI syntax rather than
    // part of the path.
    let path = match decoded.strip_prefix('/') {
        Some(tail) if is_windows_drive(tail) => tail.to_string(),
        _ => decoded,
    };

    Some(PathBuf::from(path))
}

/// The `file:` URI for a local path.
pub fn from_path(path: &Path) -> Uri {
    let path = path.to_string_lossy().replace('\\', "/");

    let mut uri = String::from("file://");
    if !path.starts_with('/') {
        uri.push('/');
    }

    for byte in path.bytes() {
        if is_unreserved(byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }

    Uri(uri)
}

/// Characters that survive a URI path segment unescaped: RFC 3986's unreserved set, plus the `/`
/// separating segments and the `:` of a Windows drive.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':')
}

fn is_windows_drive(path: &str) -> bool {
    let mut chars = path.chars();

    matches!(
        (chars.next(), chars.next()),
        (Some(letter), Some(':')) if letter.is_ascii_alphabetic()
    )
}

/// Decode `%XX` escapes. A malformed escape is kept verbatim rather than dropped, so a path that was
/// never encoded in the first place still round-trips.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        let decoded = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1).zip(bytes.get(i + 2)))
            .flatten()
            .and_then(|(high, low)| hex(*high).zip(hex(*low)))
            .map(|(high, low)| high << 4 | low);

        if let Some(byte) = decoded {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|digit| digit as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_plain_path() {
        let path = Path::new("/Users/dev/source/Combo.mc");

        assert_eq!(to_path(&from_path(path)).as_deref(), Some(path));
    }

    #[test]
    fn round_trips_spaces_and_multibyte() {
        let path = Path::new("/Users/dev/my source/Café.mc");
        let uri = from_path(path);

        assert!(
            uri.0.contains("my%20source"),
            "space should be escaped: {uri}"
        );
        assert_eq!(to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn accepts_an_unencoded_path() {
        let uri = Uri("file:///Users/dev/my source/Combo.mc".to_string());

        assert_eq!(
            to_path(&uri).as_deref(),
            Some(Path::new("/Users/dev/my source/Combo.mc"))
        );
    }

    #[test]
    fn keeps_a_malformed_escape_verbatim() {
        let uri = Uri("file:///tmp/100%done/a.mc".to_string());

        assert_eq!(
            to_path(&uri).as_deref(),
            Some(Path::new("/tmp/100%done/a.mc"))
        );
    }

    #[test]
    fn rejects_non_file_schemes_and_remote_hosts() {
        assert_eq!(to_path(&Uri("https://example.com/a.mc".to_string())), None);
        assert_eq!(to_path(&Uri("file://server/share/a.mc".to_string())), None);
    }

    #[test]
    fn strips_the_leading_slash_before_a_windows_drive() {
        let uri = Uri("file:///C:/src/Combo.mc".to_string());

        assert_eq!(to_path(&uri).as_deref(), Some(Path::new("C:/src/Combo.mc")));
    }
}
