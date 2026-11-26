//! Language detection via `whatlang` with ISO-639-1 normalization.
//!
//! `whatlang` reports ISO-639-3 codes; corpora are usually described with
//! ISO-639-1 codes. The small table below maps the common cases so
//! `target_languages = ["en", "tr"]` matches what users expect, while unknown
//! languages still fall back to their ISO-639-3 code (so they can be targeted
//! explicitly if desired).

use crate::config::FilterConfig;
use crate::filter::RejectReason;

/// Map an ISO-639-3 code to ISO-639-1 when one exists.
pub fn to_iso639_1(code: &str) -> Option<&'static str> {
    let mapped = match code {
        "tur" => "tr",
        "eng" => "en",
        "deu" => "de",
        "fra" => "fr",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt",
        "nld" => "nl",
        "rus" => "ru",
        "ara" => "ar",
        "zho" => "zh",
        "jpn" => "ja",
        "kor" => "ko",
        "hin" => "hi",
        "ben" => "bn",
        "pol" => "pl",
        "ukr" => "uk",
        "ces" => "cs",
        "swe" => "sv",
        "dan" => "da",
        "fin" => "fi",
        "nor" => "no",
        "ell" => "el",
        "heb" => "he",
        "ind" => "id",
        "vie" => "vi",
        "tha" => "th",
        "ron" => "ro",
        "hun" => "hu",
        "bul" => "bg",
        "srp" => "sr",
        "hrv" => "hr",
        "slk" => "sk",
        "slv" => "sl",
        "lit" => "lt",
        "lav" => "lv",
        "est" => "et",
        "cat" => "ca",
        "epo" => "eo",
        "lat" => "la",
        "aze" => "az",
        "kaz" => "kk",
        "uzb" => "uz",
        "tam" => "ta",
        "tel" => "te",
        "mar" => "mr",
        "guj" => "gu",
        "kan" => "kn",
        "mal" => "ml",
        "pan" => "pa",
        "sin" => "si",
        "nep" => "ne",
        "pes" => "fa",
        "urd" => "ur",
        "sqi" => "sq",
        "mkd" => "mk",
        "bos" => "bs",
        "bel" => "be",
        "tgl" => "tl",
        "jav" => "jv",
        "msa" => "ms",
        "amh" => "am",
        "swa" => "sw",
        "zul" => "zu",
        "afr" => "af",
        "yor" => "yo",
        "ibo" => "ig",
        "hau" => "ha",
        "sna" => "sn",
        "som" => "so",
        "kin" => "rw",
        "tuk" => "tk",
        "tgk" => "tg",
        "kir" => "ky",
        "mya" => "my",
        "khm" => "km",
        "lao" => "lo",
        "mon" => "mn",
        "mlt" => "mt",
        "isl" => "is",
        "gle" => "ga",
        "cym" => "cy",
        "eus" => "eu",
        "glg" => "gl",
        _ => return None,
    };
    Some(mapped)
}

/// Detect the language of `text`, returning `(code, confidence)`.
///
/// `code` is ISO-639-1 when a mapping exists, otherwise the ISO-639-3 code.
pub fn detect(text: &str) -> Option<(String, f64)> {
    let info = whatlang::detect(text)?;
    let iso3 = info.lang().code();
    let code = to_iso639_1(iso3).unwrap_or(iso3);
    Some((code.to_string(), info.confidence()))
}

/// Detect and enforce the configured language policy.
pub fn detect_and_check(
    text: &str,
    config: &FilterConfig,
) -> std::result::Result<(String, f64), RejectReason> {
    let detected = detect(text);
    if !config.enable_language_filter {
        return Ok(detected.unwrap_or_else(|| ("und".to_string(), 0.0)));
    }
    let (code, confidence) = detected.ok_or(RejectReason::LanguageUndetermined)?;
    if confidence < config.min_lang_confidence {
        return Err(RejectReason::LowLanguageConfidence);
    }
    let accepted = config
        .target_languages
        .iter()
        .any(|target| target.eq_ignore_ascii_case(&code));
    if !accepted {
        return Err(RejectReason::UnsupportedLanguage);
    }
    Ok((code, confidence))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENGLISH: &str = "Rust is a systems programming language focused on safety, speed and concurrency. \
        It accomplishes its goals through a strong type system and an ownership model that is checked at compile time. \
        The language avoids a garbage collector while still preventing memory errors in safe code. \
        Programmers use it to build operating systems, web services, command line tools and embedded firmware.";

    const TURKISH: &str = "Rust sistem programlama alanında güvenlik, hız ve eşzamanlılık hedefleriyle geliştirilmiş bir dildir. \
        Sahiplik modeli sayesinde bellek hataları derleme aşamasında yakalanır ve çalışma zamanında çöp toplayıcıya ihtiyaç duyulmaz. \
        Geliştiriciler bu dili işletim sistemleri, web servisleri ve gömülü yazılımlar için tercih ediyor. \
        Derleyici mesajları yeni başlayanlara yol gösterir ve öneriler sunar.";

    #[test]
    fn maps_iso639_3_to_iso639_1() {
        assert_eq!(to_iso639_1("tur"), Some("tr"));
        assert_eq!(to_iso639_1("eng"), Some("en"));
        assert_eq!(to_iso639_1("xyz"), None);
    }

    #[test]
    fn detects_english_and_turkish() {
        let (english, confidence) = detect(ENGLISH).expect("english detected");
        assert_eq!(english, "en");
        assert!(confidence > 0.5);
        let (turkish, confidence) = detect(TURKISH).expect("turkish detected");
        assert_eq!(turkish, "tr");
        assert!(confidence > 0.3);
    }

    #[test]
    fn accepts_configured_languages_only() {
        let config = FilterConfig {
            min_lang_confidence: 0.3,
            target_languages: vec!["en".into()],
            ..FilterConfig::default()
        };
        let (code, _) = detect_and_check(ENGLISH, &config).expect("english accepted");
        assert_eq!(code, "en");
        assert_eq!(
            detect_and_check(TURKISH, &config),
            Err(RejectReason::UnsupportedLanguage)
        );
    }

    #[test]
    fn confidence_threshold_rejects_ambiguous_text() {
        let config = FilterConfig {
            min_lang_confidence: 0.99,
            ..FilterConfig::default()
        };
        let ambiguous = "ok the la der il que y el 12 3 x";
        let result = detect_and_check(ambiguous, &config);
        assert!(
            result.is_err(),
            "ambiguous text must be rejected: {result:?}"
        );
    }

    #[test]
    fn disabled_filter_returns_fallback_code() {
        let config = FilterConfig {
            enable_language_filter: false,
            ..FilterConfig::default()
        };
        let (code, score) = detect_and_check("", &config).expect("disabled filter accepts");
        assert_eq!(code, "und");
        assert_eq!(score, 0.0);
    }
}
