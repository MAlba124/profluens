//! Language tags as containers write them, compared by their primary subtag across ISO 639-1 and
//! 639-2.

/// ISO 639-1 codes with their ISO 639-2 terminology and bibliographic codes.
const CODES: [[&str; 3]; 62] = [
    ["ar", "ara", "ara"],
    ["bg", "bul", "bul"],
    ["bn", "ben", "ben"],
    ["bo", "bod", "tib"],
    ["bs", "bos", "bos"],
    ["ca", "cat", "cat"],
    ["cs", "ces", "cze"],
    ["cy", "cym", "wel"],
    ["da", "dan", "dan"],
    ["de", "deu", "ger"],
    ["el", "ell", "gre"],
    ["en", "eng", "eng"],
    ["es", "spa", "spa"],
    ["et", "est", "est"],
    ["eu", "eus", "baq"],
    ["fa", "fas", "per"],
    ["fi", "fin", "fin"],
    ["fr", "fra", "fre"],
    ["ga", "gle", "gle"],
    ["gl", "glg", "glg"],
    ["gu", "guj", "guj"],
    ["he", "heb", "heb"],
    ["hi", "hin", "hin"],
    ["hr", "hrv", "hrv"],
    ["hu", "hun", "hun"],
    ["hy", "hye", "arm"],
    ["id", "ind", "ind"],
    ["is", "isl", "ice"],
    ["it", "ita", "ita"],
    ["ja", "jpn", "jpn"],
    ["ka", "kat", "geo"],
    ["kk", "kaz", "kaz"],
    ["km", "khm", "khm"],
    ["kn", "kan", "kan"],
    ["ko", "kor", "kor"],
    ["lt", "lit", "lit"],
    ["lv", "lav", "lav"],
    ["mk", "mkd", "mac"],
    ["ml", "mal", "mal"],
    ["mr", "mar", "mar"],
    ["ms", "msa", "may"],
    ["mt", "mlt", "mlt"],
    ["my", "mya", "bur"],
    ["nb", "nob", "nob"],
    ["nl", "nld", "dut"],
    ["nn", "nno", "nno"],
    ["no", "nor", "nor"],
    ["pa", "pan", "pan"],
    ["pl", "pol", "pol"],
    ["pt", "por", "por"],
    ["ro", "ron", "rum"],
    ["ru", "rus", "rus"],
    ["sk", "slk", "slo"],
    ["sl", "slv", "slv"],
    ["sq", "sqi", "alb"],
    ["sr", "srp", "srp"],
    ["sv", "swe", "swe"],
    ["ta", "tam", "tam"],
    ["th", "tha", "tha"],
    ["tr", "tur", "tur"],
    ["uk", "ukr", "ukr"],
    ["zh", "zho", "chi"],
];

/// The tags that name no language.
const NONE: [&str; 4] = ["und", "mul", "zxx", "mis"];

pub fn primary(tag: &str) -> &str {
    let tag = tag.trim();
    tag.split(['-', '_']).next().unwrap_or(tag)
}

/// Whether `tag` names a language. Empty, `und` and the like do not.
pub fn known(tag: &str) -> bool {
    let p = primary(tag);
    !p.is_empty() && !NONE.iter().any(|n| p.eq_ignore_ascii_case(n))
}

fn row(p: &str) -> Option<usize> {
    CODES.iter().position(|codes| codes.iter().any(|c| p.eq_ignore_ascii_case(c)))
}

/// Whether `a` and `b` name the same language. Never for a tag that names none.
pub fn same(a: &str, b: &str) -> bool {
    if !known(a) || !known(b) {
        return false;
    }
    let (a, b) = (primary(a), primary(b));
    match (row(a), row(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.eq_ignore_ascii_case(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_of_one_language_match_whatever_code_they_are_in() {
        assert!(same("fr", "fre"));
        assert!(same("fra", "fre"));
        assert!(same("fr-CA", "fra"));
        assert!(same("ENG", "en_US"));
        assert!(same("de", "ger"));
        assert!(same("zh-Hant", "chi"));
        assert!(same("haw", "HAW"));
        assert!(!same("haw", "en"));
        assert!(!same("fr", "de"));
        assert!(!same("nb", "no"));
    }

    #[test]
    fn a_tag_that_names_no_language_matches_none() {
        assert!(!same("und", "und"));
        assert!(!same("", ""));
        assert!(!same("mul", "en"));
        assert!(!known(" "));
        assert!(known("en"));
        assert_eq!(primary("pt-BR"), "pt");
    }
}
