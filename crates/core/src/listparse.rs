//! Parses `LIST` replies from Xbox FTP servers.
//!
//! Xbox dashboards answer `LIST` in one of two shapes. UnleashX, EvolutionX, Avalaunch and XBMC
//! print Unix `ls -l` lines, some without a group column; a few print DOS `dir` lines. Neither
//! has a fixed column count once names contain spaces, so both parsers anchor on the date and
//! take everything after it as the name.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn is_month(s: &str) -> bool {
    MONTHS.contains(&s.to_ascii_lowercase().as_str())
}

/// Splits `s` into whitespace-separated tokens, returning each token with the byte offset just
/// past it, so the remainder of the line (a name with spaces) can be sliced off intact.
fn tokens(s: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        if c.is_whitespace() {
            if let Some(st) = start.take() {
                out.push((&s[st..i], i));
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(st) = start {
        out.push((&s[st..], s.len()));
    }
    out
}

fn rest_after(s: &str, end: usize) -> &str {
    s[end..].trim_start()
}

fn parse_unix(line: &str) -> Option<ListEntry> {
    let t = tokens(line);
    let first = t.first()?.0;
    let kind = first.chars().next()?;
    if !matches!(kind, 'd' | '-' | 'l') || first.len() < 10 {
        return None;
    }
    // Find the month; size is the token before it, the name starts three tokens after it.
    let m = t.iter().position(|(tok, _)| is_month(tok))?;
    if m < 2 || m + 2 >= t.len() {
        return None;
    }
    let size: u64 = t[m - 1].0.parse().ok()?;
    let mut name = rest_after(line, t[m + 2].1);
    if kind == 'l' {
        if let Some(i) = name.find(" -> ") {
            name = &name[..i];
        }
    }
    Some(ListEntry { name: name.to_string(), is_dir: kind == 'd', size })
}

fn parse_dos(line: &str) -> Option<ListEntry> {
    let t = tokens(line);
    if t.len() < 4 {
        return None;
    }
    let date = t[0].0;
    if !(date.contains('-') || date.contains('/')) || !date.chars().next()?.is_ascii_digit() {
        return None;
    }
    if !t[1].0.contains(':') {
        return None;
    }
    let (is_dir, size) = if t[2].0.eq_ignore_ascii_case("<DIR>") {
        (true, 0)
    } else {
        (false, t[2].0.replace(',', "").parse().ok()?)
    };
    let name = rest_after(line, t[2].1);
    Some(ListEntry { name: name.to_string(), is_dir, size })
}

/// Parses one `LIST` line. Returns `None` for lines that are not entries (`total 12`, blank
/// lines) and for `.` and `..`.
pub fn parse_line(line: &str) -> Option<ListEntry> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.trim().is_empty() {
        return None;
    }
    let e = parse_unix(line).or_else(|| parse_dos(line))?;
    if e.name.is_empty() || e.name == "." || e.name == ".." {
        return None;
    }
    Some(e)
}

/// Parses a whole `LIST` reply, keeping the lines it could not read so a caller can report a
/// server whose format is new to it.
pub fn parse_listing<S: AsRef<str>>(lines: &[S]) -> (Vec<ListEntry>, Vec<String>) {
    let mut entries = Vec::new();
    let mut unparsed = Vec::new();
    for l in lines {
        let l = l.as_ref();
        match parse_line(l) {
            Some(e) => entries.push(e),
            None => {
                let trimmed = l.trim();
                let lower = trimmed.to_ascii_lowercase();
                let is_dot = trimmed.ends_with(" .") || trimmed.ends_with(" ..");
                if !trimmed.is_empty() && !lower.starts_with("total") && !is_dot {
                    unparsed.push(l.to_string());
                }
            }
        }
    }
    (entries, unparsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_with_group() {
        let e = parse_line("drwxr-xr-x   1 root  root         0 Jan 01  2001 Games").unwrap();
        assert_eq!(e, ListEntry { name: "Games".into(), is_dir: true, size: 0 });
        let e = parse_line("-rw-r--r--   1 xbox  xbox   1234567 Mar 14 12:30 default.xbe").unwrap();
        assert_eq!(e, ListEntry { name: "default.xbe".into(), is_dir: false, size: 1234567 });
    }

    #[test]
    fn unix_without_group_and_names_with_spaces() {
        let e = parse_line("drwxrwxrwx 1 xbox 0 Feb 03 2004 Halo 2 (Multiplayer)").unwrap();
        assert_eq!(e.name, "Halo 2 (Multiplayer)");
        assert!(e.is_dir);
        let e = parse_line("-rwxrwxrwx 1 owner group 4294967296 Dec 31 23:59  two  spaces.bin").unwrap();
        assert_eq!(e.name, "two  spaces.bin");
        assert_eq!(e.size, 4_294_967_296);
    }

    #[test]
    fn dos_style() {
        let e = parse_line("01-01-01  12:00AM       <DIR>          Games").unwrap();
        assert_eq!(e, ListEntry { name: "Games".into(), is_dir: true, size: 0 });
        let e = parse_line("03-14-04  12:30PM            1,234,567 Some File.xbe").unwrap();
        assert_eq!(e, ListEntry { name: "Some File.xbe".into(), is_dir: false, size: 1234567 });
    }

    #[test]
    fn skips_noise() {
        let lines = [
            "total 4",
            "drwxr-xr-x 1 root root 0 Jan 01 2001 .",
            "drwxr-xr-x 1 root root 0 Jan 01 2001 ..",
            "",
            "-rw-r--r-- 1 root root 10 Jan 01 2001 a.bin",
            "something unexpected",
        ];
        let (entries, unparsed) = parse_listing(&lines);
        assert_eq!(entries.len(), 1);
        assert_eq!(unparsed, vec!["something unexpected".to_string()]);
    }
}
