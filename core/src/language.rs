//! Language tags as containers write them, made BCP 47 and compared by language and script.

/// Bytes of the longest tag `bcp47` makes, a three letter language, a script and a region of
/// three digits.
pub const MAX_TAG: usize = 12;

/// A BCP 47 tag of a language, its script and its region, in the canonical case, empty for
/// none. Kept inline, a track list holds one per track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tag {
    b: [u8; MAX_TAG],
    n: u8,
}

impl Tag {
    pub fn as_str(&self) -> &str {
        // only ASCII letters, digits and hyphens are ever put in
        std::str::from_utf8(&self.b[..usize::from(self.n)]).unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The language subtag, `zh` of `zh-Hant-TW`.
    pub fn language(&self) -> &str {
        self.as_str().split('-').next().unwrap_or_default()
    }

    /// The script subtag, empty when the tag names none.
    pub fn script(&self) -> &str {
        self.as_str().split('-').nth(1).filter(|s| s.len() == 4).unwrap_or_default()
    }

    fn put(&mut self, s: &[u8]) {
        if s.is_empty() {
            return;
        }
        let at = usize::from(self.n);
        let sep = usize::from(at > 0);
        if at + sep + s.len() > MAX_TAG {
            return;
        }
        if sep == 1 {
            self.b[at] = b'-';
        }
        self.b[at + sep..at + sep + s.len()].copy_from_slice(s);
        self.n = (at + sep + s.len()) as u8;
    }
}

/// ISO 639-2 codes, bibliographic and terminological, and the withdrawn ones files still
/// carry, by the ISO 639-1 code of each, sorted for a binary search.
const TWO: [([u8; 3], [u8; 2]); 207] = include!("language/two.in");

/// Two letter codes BCP 47 replaced, ISO 639-1's withdrawn ones.
const RENAMED: [([u8; 2], [u8; 2]); 5] =
    [(*b"in", *b"id"), (*b"iw", *b"he"), (*b"ji", *b"yi"), (*b"jw", *b"jv"), (*b"mo", *b"ro")];

/// Chinese regions and the script each implies, the scripts are not interchangeable and
/// files tag them either way.
const ZH_SCRIPT: [([u8; 2], &[u8; 4]); 6] = [
    (*b"CN", b"Hans"),
    (*b"HK", b"Hant"),
    (*b"MO", b"Hant"),
    (*b"MY", b"Hans"),
    (*b"SG", b"Hans"),
    (*b"TW", b"Hant"),
];

/// ISO 639-1 or the BCP 47 tag of a language and its English name.
const NAMES: [(&str, &str); 54] = include!("language/names.in");

/// The tag of a language by its English name or a Norwegian name of it, any case, as Ogg's free
/// text states it.
fn named(name: &str) -> Option<&'static str> {
    match name.to_lowercase().as_str() {
        "norsk" => Some("no"),
        "bokm\u{e5}l" | "bokmal" | "norsk bokm\u{e5}l" | "norsk bokmal" => Some("nb"),
        "nynorsk" | "norsk nynorsk" => Some("nn"),
        _ => NAMES.iter().find(|l| l.1.eq_ignore_ascii_case(name)).map(|l| l.0),
    }
}

fn alpha(s: &str) -> bool {
    s.bytes().all(|c| c.is_ascii_alphabetic())
}

/// The ISO 639-1 code of a language subtag where it has one, `de` of `ger`, `deu` and `de`.
fn short(lang: &[u8]) -> Option<[u8; 2]> {
    match *lang {
        [a, b] => {
            let two = [a, b];
            Some(RENAMED.iter().find(|r| r.0 == two).map_or(two, |r| r.1))
        }
        [a, b, c] => TWO.binary_search_by_key(&[a, b, c], |e| e.0).ok().map(|i| TWO[i].1),
        _ => None,
    }
}

/// `raw` as a BCP 47 tag: the language as ISO 639-1 where it has a code (`ger`, `deu` are
/// `de`), else its three letters, the script and the region in their case, variants,
/// extensions and private use left off, an extended language taking the language's place
/// (`zh-yue` is `yue`), a Chinese region without a script the script it implies (`zh-TW` is
/// `zh-Hant-TW`). Underscores count as hyphens and a POSIX locale's `.` or `@` ends it. A
/// language's English name gives its code, Ogg's free text. Empty for `und`, an empty tag or
/// one naming no language.
pub fn bcp47(raw: &str) -> Tag {
    let mut t = Tag::default();
    let raw = raw.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    let cut = raw.find(['.', '@']).map_or(raw, |i| &raw[..i]);
    let mut parts = cut.split(['-', '_']);
    let first = parts.next().unwrap_or_default();
    if !(2..=3).contains(&first.len()) || !alpha(first) {
        if let Some(code) = named(raw) {
            t.put(code.as_bytes());
        }
        return t;
    }
    let mut lang = [0u8; 3];
    let mut n = first.len();
    lang[..n].copy_from_slice(&first.as_bytes()[..n]);
    lang.make_ascii_lowercase();
    let mut script = [0u8; 4];
    let mut region = [0u8; 3];
    let (mut ns, mut nr) = (0, 0);
    let mut at = 0;
    for p in parts {
        match p.len() {
            // the first extended language, which BCP 47 writes as the language
            3 if at == 0 && first.len() == 2 && alpha(p) => {
                lang.copy_from_slice(p.as_bytes());
                lang.make_ascii_lowercase();
                n = 3;
                at = 1;
            }
            4 if at <= 1 && alpha(p) => {
                script.copy_from_slice(p.as_bytes());
                script.make_ascii_lowercase();
                script[0].make_ascii_uppercase();
                ns = 4;
                at = 2;
            }
            2 if at <= 2 && alpha(p) => {
                region[..2].copy_from_slice(p.as_bytes());
                region.make_ascii_uppercase();
                nr = 2;
                break;
            }
            3 if at <= 2 && p.bytes().all(|c| c.is_ascii_digit()) => {
                region.copy_from_slice(p.as_bytes());
                nr = 3;
                break;
            }
            _ => break,
        }
    }
    if &lang[..n] == b"und" {
        return t;
    }
    match short(&lang[..n]) {
        Some(two) => t.put(&two),
        None => t.put(&lang[..n]),
    }
    if ns == 0 && t.as_str() == "zh" && nr == 2 {
        let rg = [region[0], region[1]];
        if let Some(z) = ZH_SCRIPT.iter().find(|z| z.0 == rg) {
            script = *z.1;
            ns = 4;
        }
    }
    t.put(&script[..ns]);
    t.put(&region[..nr]);
    t
}

/// Whether the tags `a` and `b` name one language, each made BCP 47 first: the language the
/// same and the script too where both name one, so `zh` serves `zh-Hant` but `zh-Hans` does
/// not. A tag naming no language matches none. A saved key such as the server's `zh-hant`
/// compares as its tag.
pub fn same(a: &str, b: &str) -> bool {
    let (a, b) = (bcp47(a), bcp47(b));
    if a.is_empty() || a.language() != b.language() {
        return false;
    }
    a.script().is_empty() || b.script().is_empty() || a.script() == b.script()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_and_each_code_once() {
        assert!(TWO.windows(2).all(|w| w[0].0 < w[1].0));
        let lower = |e: &([u8; 3], [u8; 2])| e.0.iter().chain(&e.1).all(u8::is_ascii_lowercase);
        assert!(TWO.iter().all(lower));
    }

    #[test]
    fn every_container_form_gives_one_tag() {
        for (raw, want) in include!("language/cases.in") {
            let t = bcp47(raw);
            assert_eq!(t.as_str(), want, "{raw:?}");
            assert!(t.as_str().len() <= MAX_TAG);
            assert_eq!(bcp47(t.as_str()), t, "{raw:?} is its own tag once made");
        }
    }

    #[test]
    fn tags_compare_by_language_then_script() {
        assert!(same("ger", "de") && same("deu", "DE") && same("en-US", "eng"));
        assert!(same("zh", "zh-hant") && same("zh-TW", "zh-hant"));
        assert!(!same("zh-CN", "zh-hant") && !same("zh-Hans", "zh-Hant"));
        assert!(same("he", "iw") && !same("nb", "nn"));
        assert!(!same("und", "und") && !same("", "") && !same("en", ""));
    }
}
