//! Textanalyse: Tokenisierung, Normalisierung (Kleinschreibung, Akzente entfernen),
//! Stemming (Deutsch/Englisch) und einfache Spracherkennung.

use rust_stemmers::{Algorithm, Stemmer};
use std::sync::OnceLock;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

pub const MAX_TOKEN_BYTES: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Lang {
    Other = 0,
    De = 1,
    En = 2,
}

impl Lang {
    pub fn from_u8(v: u8) -> Lang {
        match v {
            1 => Lang::De,
            2 => Lang::En,
            _ => Lang::Other,
        }
    }
    pub fn code(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
            Lang::Other => "",
        }
    }
    pub fn from_code(code: &str) -> Lang {
        let c = code.trim().to_ascii_lowercase();
        if c.starts_with("de") {
            Lang::De
        } else if c.starts_with("en") {
            Lang::En
        } else {
            Lang::Other
        }
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Zerlegt `text` in Roh-Token (Byte-Offsets inklusive). Callback: (token, start, end).
pub fn tokenize<F: FnMut(&str, usize, usize)>(text: &str, mut f: F) {
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if is_cjk(c) {
                if let Some(s) = start.take() {
                    emit(text, s, i, &mut f);
                }
                emit(text, i, i + c.len_utf8(), &mut f);
            } else if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            emit(text, s, i, &mut f);
        }
    }
    if let Some(s) = start {
        emit(text, s, text.len(), &mut f);
    }
}

#[inline]
fn emit<F: FnMut(&str, usize, usize)>(text: &str, s: usize, e: usize, f: &mut F) {
    if e - s <= MAX_TOKEN_BYTES {
        f(&text[s..e], s, e);
    }
}

/// Kleinschreibung + Entfernen diakritischer Zeichen (ä→a, é→e, ß→ss).
pub fn fold(word: &str) -> String {
    if word.is_ascii() {
        return word.to_ascii_lowercase();
    }
    let lower = word.to_lowercase();
    let mut out = String::with_capacity(lower.len());
    for c in lower.nfd() {
        if is_combining_mark(c) {
            continue;
        }
        if c == 'ß' {
            out.push_str("ss");
        } else {
            out.push(c);
        }
    }
    out
}

struct Stemmers {
    de: Stemmer,
    en: Stemmer,
}

fn stemmers() -> &'static Stemmers {
    static S: OnceLock<Stemmers> = OnceLock::new();
    S.get_or_init(|| Stemmers {
        de: Stemmer::create(Algorithm::German),
        en: Stemmer::create(Algorithm::English),
    })
}

/// Erzeugt den Indexterm für ein Roh-Token in der gegebenen Sprache.
pub fn index_term(raw: &str, lang: Lang) -> String {
    match lang {
        Lang::Other => fold(raw),
        Lang::De => {
            let lower = raw.to_lowercase();
            fold(&stemmers().de.stem(&lower))
        }
        Lang::En => {
            let f = fold(raw);
            stemmers().en.stem(&f).into_owned()
        }
    }
}

/// Alle Indexterme, unter denen ein Query-Wort in Dokumenten beliebiger Sprache
/// indexiert sein kann (ungestemmt, deutsch, englisch).
pub fn query_variants(raw: &str) -> Vec<String> {
    let mut v = vec![fold(raw)];
    for lang in [Lang::De, Lang::En] {
        let t = index_term(raw, lang);
        if !v.contains(&t) {
            v.push(t);
        }
    }
    v
}

/// Analysiert einen Text vollständig zu Indextermen.
pub fn analyze(text: &str, lang: Lang, out: &mut Vec<String>) {
    tokenize(text, |t, _, _| out.push(index_term(t, lang)));
}

const STOP_DE: &[&str] = &[
    "der", "die", "das", "und", "ist", "nicht", "ein", "eine", "einen", "zu", "den", "mit", "von",
    "sich", "auf", "fur", "im", "dem", "des", "auch", "es", "an", "werden", "aus", "er", "hat",
    "dass", "sie", "nach", "wird", "bei", "einer", "um", "am", "sind", "noch", "wie", "einem",
    "uber", "so", "zum", "kann", "nur", "oder", "aber", "vor", "zur", "bis", "mehr", "durch",
    "man", "sein", "wurde", "ich", "wir", "was", "wenn", "ihr", "als", "in", "fuer",
];
const STOP_EN: &[&str] = &[
    "the", "of", "and", "to", "a", "in", "is", "it", "you", "that", "he", "was", "for", "on",
    "are", "with", "as", "i", "his", "they", "be", "at", "one", "have", "this", "from", "or",
    "had", "by", "but", "what", "some", "we", "can", "out", "other", "were", "all", "there",
    "when", "your", "how", "an", "which", "their", "if", "will", "do", "does", "not", "about",
];

/// Ist das (gefaltete) Wort ein häufiges Füllwort?
pub fn is_stopword(folded: &str) -> bool {
    STOP_DE.contains(&folded) || STOP_EN.contains(&folded)
}

/// Einfache Spracherkennung über Füllwort-Häufigkeiten.
pub fn detect_lang(text: &str, hint: Option<&str>) -> Lang {
    if let Some(h) = hint {
        let l = Lang::from_code(h);
        if l != Lang::Other {
            return l;
        }
    }
    let (mut de, mut en, mut total) = (0usize, 0usize, 0usize);
    tokenize(text, |t, _, _| {
        if total >= 3000 {
            return;
        }
        total += 1;
        if t.len() <= 6 {
            let f = fold(t);
            if STOP_DE.contains(&f.as_str()) {
                de += 1;
            }
            if STOP_EN.contains(&f.as_str()) {
                en += 1;
            }
        }
    });
    let min = (total / 50).max(2);
    if de >= min && de > en {
        Lang::De
    } else if en >= min && en >= de {
        Lang::En
    } else {
        Lang::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_with_offsets() {
        let mut toks = Vec::new();
        tokenize("Hallo, Wörld! 東京 x2", |t, s, e| {
            toks.push((t.to_string(), s, e))
        });
        let words: Vec<_> = toks.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(words, vec!["Hallo", "Wörld", "東", "京", "x2"]);
        let s = "Hallo, Wörld! 東京 x2";
        for (w, a, b) in toks {
            assert_eq!(&s[a..b], w);
        }
    }

    #[test]
    fn folding_and_stemming() {
        assert_eq!(fold("Straße"), "strasse");
        assert_eq!(fold("Café"), "cafe");
        assert_eq!(index_term("Häuser", Lang::De), index_term("Haus", Lang::De));
        assert_eq!(index_term("running", Lang::En), "run");
        let v = query_variants("Suchmaschinen");
        assert!(v.contains(&"suchmaschinen".to_string()));
        assert!(v.contains(&index_term("Suchmaschine", Lang::De)));
    }

    #[test]
    fn detects_language() {
        assert_eq!(
            detect_lang(
                "Das ist ein Test und die Suche ist nicht schlecht, aber auch nicht gut.",
                None
            ),
            Lang::De
        );
        assert_eq!(
            detect_lang(
                "This is a test and the search is not bad, but it is not good either.",
                None
            ),
            Lang::En
        );
    }
}
