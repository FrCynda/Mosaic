//! `t(key)` resolves a UI string against the active language, then its base language ("pt-PT" → "pt"), then
//! English, then the raw key itself (useful mid-translation). Each language is a `src/lang/<code>.txt` of
//! `key=value` lines (the value is everything after the first `=`, spaces included); a regional variant's file
//! only needs the strings that differ from its base language's.

use std::collections::HashMap;
use std::sync::OnceLock;

static LANG: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

pub fn lang() -> String {
    let l = LANG.read().unwrap();
    if l.is_empty() { "en".into() } else { l.clone() }
}

pub fn set_lang(code: &str) {
    *LANG.write().unwrap() = code.to_string();
}

/// A language the UI speaks: its code, its own name for itself (never translated: a UI in Russian shouldn't
/// call French "Французский"), and its English name, so it can be found either way.
pub struct Lang {
    pub code: &'static str,
    pub name: &'static str,
    pub english: &'static str,
    text: &'static str,
}

macro_rules! langs {
    ($(($code:literal, $name:literal, $english:literal)),* $(,)?) => {
        pub static LANGS: &[Lang] = &[$(Lang { code: $code, name: $name, english: $english, text: include_str!(concat!("lang/", $code, ".txt")) }),*];
    };
}

// by English name, so the list reads in one order whatever the UI's language
langs![
    ("af", "Afrikaans", "Afrikaans"),
    ("sq", "Shqip", "Albanian"),
    ("am", "አማርኛ", "Amharic"),
    ("ar", "العربية", "Arabic"),
    ("hy", "Հայերեն", "Armenian"),
    ("az", "Azərbaycan", "Azerbaijani"),
    ("eu", "Euskara", "Basque"),
    ("be", "Беларуская", "Belarusian"),
    ("bn", "বাংলা", "Bengali"),
    ("bg", "Български", "Bulgarian"),
    ("ca", "Català", "Catalan"),
    ("zh", "简体中文", "Chinese (Simplified)"),
    ("zh-Hant", "繁體中文", "Chinese (Traditional)"),
    ("hr", "Hrvatski", "Croatian"),
    ("cs", "Čeština", "Czech"),
    ("da", "Dansk", "Danish"),
    ("nl", "Nederlands", "Dutch"),
    ("en-GB", "English (UK)", "English (UK)"),
    ("en", "English (US)", "English (US)"),
    ("et", "Eesti", "Estonian"),
    ("fil", "Filipino", "Filipino"),
    ("fi", "Suomi", "Finnish"),
    ("fr", "Français", "French"),
    ("gl", "Galego", "Galician"),
    ("ka", "ქართული", "Georgian"),
    ("de", "Deutsch", "German"),
    ("de-CH", "Deutsch (Schweiz)", "German (Switzerland)"),
    ("el", "Ελληνικά", "Greek"),
    ("gu", "ગુજરાતી", "Gujarati"),
    ("ha", "Hausa", "Hausa"),
    ("hi", "हिन्दी", "Hindi"),
    ("hu", "Magyar", "Hungarian"),
    ("is", "Íslenska", "Icelandic"),
    ("ig", "Igbo", "Igbo"),
    ("id", "Bahasa Indonesia", "Indonesian"),
    ("it", "Italiano", "Italian"),
    ("ja", "日本語", "Japanese"),
    ("kn", "ಕನ್ನಡ", "Kannada"),
    ("kk", "Қазақ тілі", "Kazakh"),
    ("ko", "한국어", "Korean"),
    ("lv", "Latviešu", "Latvian"),
    ("lt", "Lietuvių", "Lithuanian"),
    ("mk", "Македонски", "Macedonian"),
    ("ms", "Bahasa Melayu", "Malay"),
    ("ml", "മലയാളം", "Malayalam"),
    ("mr", "मराठी", "Marathi"),
    ("ne", "नेपाली", "Nepali"),
    ("nb", "Norsk bokmål", "Norwegian"),
    ("om", "Afaan Oromoo", "Oromo"),
    ("fa", "فارسی", "Persian"),
    ("pt", "Português (Brasil)", "Portuguese (Brazil)"),
    ("pt-PT", "Português (Portugal)", "Portuguese (Portugal)"),
    ("pa", "ਪੰਜਾਬੀ", "Punjabi"),
    ("ro", "Română", "Romanian"),
    ("ru", "Русский", "Russian"),
    ("sr", "Српски", "Serbian (Cyrillic)"),
    ("sr-Latn", "Srpski", "Serbian (Latin)"),
    ("sk", "Slovenčina", "Slovak"),
    ("sl", "Slovenščina", "Slovenian"),
    ("so", "Soomaali", "Somali"),
    ("es", "Español (España)", "Spanish (Spain)"),
    ("es-419", "Español (Latinoamérica)", "Spanish (Latin America)"),
    ("sw", "Kiswahili", "Swahili"),
    ("sv", "Svenska", "Swedish"),
    ("ta", "தமிழ்", "Tamil"),
    ("te", "తెలుగు", "Telugu"),
    ("th", "ไทย", "Thai"),
    ("tr", "Türkçe", "Turkish"),
    ("uk", "Українська", "Ukrainian"),
    ("ur", "اردو", "Urdu"),
    ("uz", "Oʻzbek", "Uzbek"),
    ("vi", "Tiếng Việt", "Vietnamese"),
    ("wo", "Wolof", "Wolof"),
    ("yo", "Yorùbá", "Yoruba"),
    ("zu", "isiZulu", "Zulu"),
];

/// Written right to left.
pub fn is_rtl(code: &str) -> bool {
    matches!(base(code), "ar" | "fa" | "ur")
}

/// The active language is written right to left.
pub fn rtl() -> bool {
    is_rtl(LANG.read().unwrap().as_str())
}

/// "pt-PT" → "pt".
fn base(code: &str) -> &str {
    code.split_once('-').map_or(code, |b| b.0)
}

fn tables() -> &'static HashMap<&'static str, HashMap<&'static str, &'static str>> {
    static T: OnceLock<HashMap<&'static str, HashMap<&'static str, &'static str>>> = OnceLock::new();
    T.get_or_init(|| LANGS.iter().map(|l| (l.code, parse(l.text))).collect())
}

fn parse(text: &'static str) -> HashMap<&'static str, &'static str> {
    text.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_once('=')).map(|(k, v)| (k.trim(), v)).collect()
}

/// `key` in `lang`, or in its base language.
fn table(lang: &str, key: &str) -> Option<&'static str> {
    let t = tables();
    t.get(lang).and_then(|m| m.get(key)).or_else(|| t.get(base(lang))?.get(key)).copied()
}

/// Resolves `key` in the active language, then English, then the key itself.
pub fn t(key: &str) -> &'static str {
    // called for every label of every frame: no String copy of the language per call
    let found = table(LANG.read().unwrap().as_str(), key).or_else(|| table("en", key));
    found.unwrap_or_else(|| {
        // a missing key is leaked once, not once per paint
        static MISSING: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let mut m = MISSING.lock().unwrap();
        match m.iter().find(|k| **k == key) {
            Some(k) => k,
            None => {
                let k: &'static str = Box::leak(key.to_string().into_boxed_str());
                m.push(k);
                k
            }
        }
    })
}

/// CLDR plural-category rules (which `item.<category>` key `t_count` looks up), per language.
/// `n` is always a non-negative item count here, never a fraction, so each rule only needs its
/// integer branches.
fn plural_category(lang: &str, count: i64) -> &'static str {
    let n = count.unsigned_abs();
    let (n10, n100) = (n % 10, n % 100);
    let teen = (11..=19).contains(&n100);
    if lang == "pt-PT" {
        return if n == 1 { "one" } else { "other" }; // unlike Brazil's, 0 is plural
    }
    match base(lang) {
        // zero/one/two/few(3-10)/many(11-99)/other
        "ar" => match n {
            0 => "zero",
            1 => "one",
            2 => "two",
            _ if (3..=10).contains(&n100) => "few",
            _ if (11..=99).contains(&n100) => "many",
            _ => "other",
        },
        // one (…1 but not …11) / few (…2-4 but not …12-14) / many
        "ru" | "uk" | "be" => match () {
            _ if n10 == 1 && n100 != 11 => "one",
            _ if (2..=4).contains(&n10) && !(12..=14).contains(&n100) => "few",
            _ => "many",
        },
        // as Russian, but the rest is "other"
        "sr" | "hr" => match () {
            _ if n10 == 1 && n100 != 11 => "one",
            _ if (2..=4).contains(&n10) && !(12..=14).contains(&n100) => "few",
            _ => "other",
        },
        "cs" | "sk" => match n {
            1 => "one",
            2..=4 => "few",
            _ => "other",
        },
        "sl" => match n100 {
            1 => "one",
            2 => "two",
            3 | 4 => "few",
            _ => "other",
        },
        "lt" => match () {
            _ if n10 == 1 && !teen => "one",
            _ if n10 >= 2 && !teen => "few",
            _ => "other",
        },
        "lv" => match () {
            _ if n10 == 0 || teen => "zero",
            _ if n10 == 1 && n100 != 11 => "one",
            _ => "other",
        },
        "ro" => match () {
            _ if n == 1 => "one",
            _ if n == 0 || (2..=19).contains(&n100) => "few",
            _ => "other",
        },
        // one for …1 but not …11
        "is" | "mk" => {
            if n10 == 1 && n100 != 11 {
                "one"
            } else {
                "other"
            }
        }
        // 0 and 1 both read as singular ("0 fichier", "0 arquivo")
        "fr" | "pt" | "hi" | "bn" | "gu" | "kn" | "fa" | "am" | "zu" | "pa" | "hy" => {
            if n <= 1 {
                "one"
            } else {
                "other"
            }
        }
        // no grammatical number
        "zh" | "ja" | "ko" | "vi" | "th" | "id" | "ms" | "yo" | "ig" | "wo" | "fil" => "other",
        _ => {
            if n == 1 {
                "one"
            } else {
                "other"
            }
        }
    }
}

/// Like `t_plural`, but substitutes `display` (e.g. a locale-formatted number) for `{n}` instead
/// of the raw count.
pub fn t_count(key: &str, count: i64, display: &str) -> String {
    let cat = plural_category(&lang(), count);
    t(&format!("{key}.{cat}")).replace("{n}", display)
}

/// `t("item", 3)` looks up `"item.other"` and substitutes `{n}`.
pub fn t_plural(key: &str, count: i64) -> String {
    t_count(key, count, &count.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LANG` is a process-global, so tests that call `set_lang` (unlike the rest of this module's tests,
    /// which call `table()`/`plural_category()` directly) must serialize against each other or they race
    /// under cargo's parallel test runner.
    static LANG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn falls_back_to_english_on_missing_key() {
        let _g = LANG_TEST_LOCK.lock().unwrap();
        set_lang("xx"); // an unsupported code
        assert_eq!(t("col.name"), "Name");
        set_lang("en");
    }

    #[test]
    fn falls_back_to_the_key_itself_when_wholly_unknown() {
        assert_eq!(t("nonexistent.key"), "nonexistent.key");
    }

    #[test]
    fn a_regional_variant_falls_back_to_its_base_language() {
        assert_eq!(table("pt-PT", "col.name"), table("pt", "col.name"));
        assert_ne!(table("en-GB", "menu.folder_color"), table("en", "menu.folder_color"));
    }

    #[test]
    fn values_keep_their_spaces() {
        assert!(table("en", "status.indexing").unwrap().starts_with(" | "));
    }

    #[test]
    fn english_plural_boundary() {
        let _g = LANG_TEST_LOCK.lock().unwrap();
        set_lang("en");
        assert_eq!(t_plural("item", 1), "1 item");
        assert_eq!(t_plural("item", 0), "0 items");
        assert_eq!(t_plural("item", 2), "2 items");
    }

    #[test]
    fn arabic_plural_boundary() {
        assert_eq!(plural_category("ar", 0), "zero");
        assert_eq!(plural_category("ar", 1), "one");
        assert_eq!(plural_category("ar", 2), "two");
        assert_eq!(plural_category("ar", 5), "few"); // 3..10
        assert_eq!(plural_category("ar", 15), "many"); // 11..99
        assert_eq!(plural_category("ar", 103), "few"); // 103 % 100 == 3
        assert_eq!(plural_category("ar", 100), "other");
    }

    #[test]
    fn slavic_plural_boundaries() {
        assert_eq!(plural_category("ru", 1), "one");
        assert_eq!(plural_category("ru", 21), "one"); // ends in 1, not 11
        assert_eq!(plural_category("ru", 11), "many"); // the x11-14 exception
        assert_eq!(plural_category("ru", 2), "few");
        assert_eq!(plural_category("ru", 5), "many");
        assert_eq!(plural_category("cs", 3), "few");
        assert_eq!(plural_category("sl", 102), "two");
        assert_eq!(plural_category("sr-Latn", 5), "other");
    }

    #[test]
    fn every_language_is_listed_once() {
        let mut codes: Vec<_> = LANGS.iter().map(|l| l.code).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), LANGS.len());
        assert!(!codes.contains(&"he"));
    }

    /// Every language must resolve every key English defines, and every plural category its own rule returns.
    #[test]
    fn every_language_covers_the_english_key_set() {
        let en = parse(include_str!("lang/en.txt"));
        for l in LANGS.iter().filter(|l| l.code != "en") {
            for key in en.keys().filter(|k| !k.starts_with("item.")) {
                assert!(table(l.code, key).is_some(), "{} is missing {key}", l.code);
            }
            for n in 0..200 {
                let cat = plural_category(l.code, n);
                assert!(table(l.code, &format!("item.{cat}")).is_some(), "{} is missing item.{cat} (n={n})", l.code);
            }
        }
    }

    /// A translation keeps English's `{placeholders}`: one lost or misspelt shows raw, or drops the number.
    #[test]
    fn translations_keep_their_placeholders() {
        let names = |s: &str| {
            let mut v: Vec<String> = s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|x| x.0.to_string())).collect();
            v.sort();
            v.dedup();
            v
        };
        let en = parse(include_str!("lang/en.txt"));
        for l in LANGS.iter() {
            for (key, v) in parse(l.text) {
                if let Some(e) = en.get(key).filter(|_| !key.starts_with("item.")) {
                    assert_eq!(names(v), names(e), "{} {key}", l.code);
                }
                assert!(en.contains_key(key), "{} has a key English doesn't: {key}", l.code);
            }
        }
    }
}
