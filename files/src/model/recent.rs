//! Recently used files, from the desktop-wide recently-used.xbel.

use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::format::{days_from_civil, from_unix};

pub struct Recent {
    pub path: PathBuf,
    pub when: SystemTime,
}

fn xbel_path() -> Option<PathBuf> {
    let base = match env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(env::var_os("HOME")?).join(".local/share"),
    };
    Some(base.join("recently-used.xbel"))
}

/// Newest-first recent files that still exist, at most `limit`.
pub fn recent(limit: usize) -> Vec<Recent> {
    let Some(text) = xbel_path().and_then(|p| fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    parse(&text, limit, Path::exists)
}

fn parse(xbel: &str, limit: usize, exists: impl Fn(&Path) -> bool) -> Vec<Recent> {
    let mut found = Vec::new();
    for tag in xbel.split("<bookmark ").skip(1) {
        let tag = tag.split('>').next().unwrap_or("");
        let Some(href) = attr(tag, "href").and_then(|h| h.strip_prefix("file://")) else {
            continue;
        };
        let when = ["modified", "visited", "added"]
            .iter()
            .find_map(|a| attr(tag, a).and_then(parse_rfc3339))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        found.push((PathBuf::from(percent_decode(&unescape(href))), when));
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.1));
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter(|(p, _)| seen.insert(p.clone()) && exists(p))
        .take(limit)
        .map(|(path, when)| Recent { path, when })
        .collect()
}

/// The five XML entities.
fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `xbel` without the bookmarks of `path`, and how many that was. Every
/// other byte is kept as it was.
fn without_bookmarks(xbel: &str, path: &Path) -> (String, usize) {
    let mut out = String::with_capacity(xbel.len());
    let (mut rest, mut removed) = (xbel, 0);
    while let Some(start) = rest.find("<bookmark ") {
        let after = &rest[start..];
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let end = if after[..tag_end].ends_with('/') {
            tag_end + 1
        } else {
            match after.find("</bookmark>") {
                Some(i) => i + "</bookmark>".len(),
                None => break,
            }
        };
        let mine = attr(&after[..tag_end], "href")
            .map(unescape)
            .and_then(|h| h.strip_prefix("file://").map(percent_decode))
            .is_some_and(|p| Path::new(&p) == path);
        if mine {
            // Take the line's indentation and its newline along.
            let head = &rest[..start];
            let keep = head.trim_end_matches([' ', '\t']);
            out.push_str(keep);
            let mut tail = &after[end..];
            if keep.ends_with('\n') || keep.is_empty() {
                tail = tail.strip_prefix('\n').unwrap_or(tail);
            }
            rest = tail;
            removed += 1;
        } else {
            out.push_str(&rest[..start + end]);
            rest = &after[end..];
        }
    }
    out.push_str(rest);
    (out, removed)
}

/// Take `path` out of the recently-used file `file` (written to a temp
/// file and renamed). True if there was anything to take out.
fn remove_from(file: &Path, path: &Path) -> std::io::Result<bool> {
    let file = fs::canonicalize(file)?;
    let (text, removed) = without_bookmarks(&fs::read_to_string(&file)?, path);
    if removed == 0 {
        return Ok(false);
    }
    let tmp = file.with_extension(format!("xbel.tmp{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, &file).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(true)
}

/// Forget `path` in the desktop's recent files.
pub fn forget(path: &Path) -> std::io::Result<bool> {
    match xbel_path() {
        Some(f) if f.exists() => remove_from(&f, path),
        _ => Ok(false),
    }
}

/// The value of ` name="..."` within a tag's text.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&key) {
        let at = from + i;
        // Make sure it's a whole attribute name, not the tail of another.
        if at == 0 || tag.as_bytes()[at - 1].is_ascii_whitespace() {
            let rest = &tag[at + key.len()..];
            return rest.split('"').next();
        }
        from = at + key.len();
    }
    None
}

/// Decode %XX escapes (bytes that aren't valid UTF-8 are kept lossily).
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "2026-10-19T02:12:00.123456Z" (UTC, with or without fraction).
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i32, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let (h, mi, se): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let days = days_from_civil(y, m, day);
    Some(from_unix(days * 86400 + h * 3600 + mi * 60 + se))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn decodes_percent() {
        assert_eq!(percent_decode("/a%20b/%C3%A9%25"), "/a b/é%");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn parses_times() {
        let t = parse_rfc3339("1970-01-02T00:00:01.5Z").unwrap();
        assert_eq!(t, SystemTime::UNIX_EPOCH + Duration::from_secs(86401));
        let t = parse_rfc3339("2026-10-19T02:12:00.123456Z").unwrap();
        assert_eq!(
            t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(),
            1_792_375_920
        );
        assert!(parse_rfc3339("garbage").is_none());
    }

    #[test]
    fn parses_xbel() {
        let xbel = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0">
  <bookmark href="file:///home/u/old%20file.txt" added="2026-01-01T00:00:00Z" modified="2026-01-01T00:00:00Z" visited="2026-01-01T00:00:00Z">
    <info><metadata owner="x"><bookmark:applications><bookmark:application name="a" modified="2030-01-01T00:00:00Z"/></bookmark:applications></metadata></info>
  </bookmark>
  <bookmark href="file:///home/u/new.txt" added="2026-02-01T00:00:00Z" modified="2026-10-19T02:12:00.5Z" visited="2026-10-19T02:12:00Z"/>
  <bookmark href="https://example.com/" modified="2027-01-01T00:00:00Z"/>
  <bookmark href="file:///gone" modified="2026-12-01T00:00:00Z"/>
  <bookmark href="file:///home/u/new.txt" modified="2026-03-01T00:00:00Z"/>
</xbel>"#;
        let got = parse(xbel, 10, |p| p != Path::new("/gone"));
        let paths: Vec<_> = got.iter().map(|r| r.path.to_str().unwrap()).collect();
        assert_eq!(paths, ["/home/u/new.txt", "/home/u/old file.txt"]);
        assert_eq!(parse(xbel, 1, |_| true).len(), 1);
    }

    const XBEL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xbel version="1.0">
  <bookmark href="file:///home/u/a%20b.txt" modified="2026-01-01T00:00:00Z">
    <info><metadata><bookmark:applications><bookmark:application name="a"/></bookmark:applications></metadata></info>
  </bookmark>
  <bookmark href="file:///home/u/c&amp;d.txt" modified="2026-01-02T00:00:00Z"/>
  <bookmark href="file:///home/u/keep.txt" modified="2026-01-03T00:00:00Z">
    <info/>
  </bookmark>
  <bookmark href="file:///home/u/a%20b.txt" modified="2026-01-04T00:00:00Z"/>
</xbel>
"#;

    #[test]
    fn removes_only_that_path() {
        let (out, n) = without_bookmarks(XBEL, Path::new("/home/u/a b.txt"));
        assert_eq!(n, 2);
        assert!(!out.contains("a%20b"));
        assert!(out.contains("c&amp;d.txt") && out.contains("keep.txt"));
        // The rest is byte for byte what it was.
        let lines: Vec<_> = XBEL.lines().collect();
        let want = [&lines[..2], &lines[5..9], &lines[10..]]
            .concat()
            .join("\n")
            + "\n";
        assert_eq!(out, want);
        let (out, n) = without_bookmarks(XBEL, Path::new("/home/u/c&d.txt"));
        assert_eq!(n, 1);
        assert!(!out.contains("c&amp;d") && out.contains("keep.txt"));
        assert_eq!(
            without_bookmarks(XBEL, Path::new("/nope")),
            (XBEL.to_string(), 0)
        );
    }

    #[test]
    fn forgetting_rewrites_a_temp_file() {
        let dir = std::env::temp_dir().join(format!("cnde-xbel-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("recently-used.xbel");
        fs::write(&f, XBEL).unwrap();
        assert!(remove_from(&f, Path::new("/home/u/keep.txt")).unwrap());
        assert!(!remove_from(&f, Path::new("/home/u/keep.txt")).unwrap());
        let got = parse(&fs::read_to_string(&f).unwrap(), 10, |_| true);
        assert_eq!(got.len(), 2);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
