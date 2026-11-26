//! PII masking for corpus safety.
//!
//! Order matters: e-mail first (it contains `@`), then Luhn-validated card
//! numbers (long digit runs), then dotted IPv4 addresses (which would otherwise
//! look like phone numbers) and finally phone numbers. Every replacement is
//! validated before masking so ordinary numbers (years, prices, ids) survive.

use once_cell::sync::Lazy;
use regex::Regex;

static EMAIL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+\-]+@[A-Z0-9.\-]+\.[A-Z]{2,24}\b").expect("static regex: email")
});
static CARD_CANDIDATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:\d[ -]?){12,18}\d\b").expect("static regex: card candidate"));
static IPV4_CANDIDATE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("static regex: ipv4"));
/// Phone formats that are specific enough to avoid swallowing account numbers:
/// area code in parentheses, international `+` prefix, national trunk `0` prefix
/// and the classic 3-3-4 grouping.
static PHONE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(
            r"(?:\+\d{1,3}[\s.\-]?)?(?:\(\d{2,4}\)[\s.\-]?)\d{2,4}[\s.\-]\d{2,4}(?:[\s.\-]\d{2,4})?",
        )
        .expect("static regex: phone with area code"),
        Regex::new(
            r"\+\d{1,3}[\s.\-]?\d{2,4}[\s.\-]?\d{3,4}[\s.\-]?\d{2,4}(?:[\s.\-]?\d{2,4})?",
        )
        .expect("static regex: international phone"),
        Regex::new(r"\b0\d{2,3}[\s.\-]\d{3,4}[\s.\-]\d{2}[\s.\-]\d{2}\b")
            .expect("static regex: national phone"),
        Regex::new(r"\b\d{3}[\s.\-]\d{3}[\s.\-]\d{4}\b")
            .expect("static regex: grouped phone"),
    ]
});

/// Luhn checksum for a digit string.
pub fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    let mut count = 0usize;
    for ch in digits.chars().rev() {
        let Some(digit) = ch.to_digit(10) else {
            return false;
        };
        count += 1;
        let mut value = digit;
        if double {
            value *= 2;
            if value > 9 {
                value -= 9;
            }
        }
        sum += value;
        double = !double;
    }
    count >= 13 && sum % 10 == 0
}

fn digits_only(input: &str) -> String {
    input.chars().filter(|c| c.is_ascii_digit()).collect()
}

fn valid_ipv4(candidate: &str) -> bool {
    let parts: Vec<&str> = candidate.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            part.parse::<u16>()
                .map(|value| value <= 255)
                .unwrap_or(false)
        })
}

/// Mask e-mails, card numbers, IPv4 addresses and phone numbers.
///
/// Returns the masked text and the number of replacements performed.
pub fn mask_pii(text: &str) -> (String, usize) {
    let mut count = 0usize;

    let without_emails = EMAIL.replace_all(text, |_: &regex::Captures<'_>| {
        count += 1;
        "<EMAIL>".to_string()
    });

    let without_cards =
        CARD_CANDIDATE.replace_all(&without_emails, |caps: &regex::Captures<'_>| {
            let candidate = &caps[0];
            if luhn_valid(&digits_only(candidate)) {
                count += 1;
                "<CREDIT_CARD>".to_string()
            } else {
                candidate.to_string()
            }
        });

    let without_ips = IPV4_CANDIDATE.replace_all(&without_cards, |caps: &regex::Captures<'_>| {
        let candidate = &caps[0];
        if valid_ipv4(candidate) {
            count += 1;
            "<IP_ADDRESS>".to_string()
        } else {
            candidate.to_string()
        }
    });

    let mut masked = without_ips.into_owned();
    for pattern in PHONE_PATTERNS.iter() {
        masked = pattern
            .replace_all(&masked, |caps: &regex::Captures<'_>| {
                let candidate = &caps[0];
                let digits = digits_only(candidate);
                if (9..=15).contains(&digits.len()) {
                    count += 1;
                    "<PHONE>".to_string()
                } else {
                    candidate.to_string()
                }
            })
            .into_owned();
    }

    (masked, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_email_addresses() {
        let (masked, count) = mask_pii("Write to jane.doe+rust@example.co.uk for details.");
        assert_eq!(masked, "Write to <EMAIL> for details.");
        assert_eq!(count, 1);
    }

    #[test]
    fn masks_luhn_valid_card_numbers_only() {
        // 4111 1111 1111 1111 is a classic valid test card number.
        let (masked, count) = mask_pii("Card 4111 1111 1111 1111 was used.");
        assert_eq!(masked, "Card <CREDIT_CARD> was used.");
        assert_eq!(count, 1);

        let (masked, count) = mask_pii("Order 1234 5678 9012 3456 shipped.");
        assert!(masked.contains("1234 5678 9012 3456"));
        assert_eq!(count, 0);
    }

    #[test]
    fn masks_ipv4_addresses_but_not_versions() {
        let (masked, count) = mask_pii("Server 192.168.1.10 answered; release 1.2.3 is old.");
        assert!(masked.contains("<IP_ADDRESS>"));
        assert!(masked.contains("1.2.3"));
        assert_eq!(count, 1);

        let (masked, _) = mask_pii("Invalid 999.1.1.1 stays.");
        assert!(masked.contains("999.1.1.1"));
    }

    #[test]
    fn masks_phone_numbers_with_and_without_country_code() {
        let (masked, count) = mask_pii("Call +90 (532) 123 45 67 or 0212 555 12 34 today.");
        assert!(masked.contains("<PHONE>"), "masked: {masked}");
        assert!(!masked.contains("532"));
        assert!(count >= 1);
    }

    #[test]
    fn leaves_prose_and_plain_numbers_untouched() {
        let text = "In 2026 the project shipped version 3 of its toolkit to 120 users.";
        let (masked, count) = mask_pii(text);
        assert_eq!(masked, text);
        assert_eq!(count, 0);
    }

    #[test]
    fn four_by_four_digit_blocks_are_not_phone_numbers() {
        let text = "Reference 9876 5432 1098 7654 was archived.";
        let (masked, count) = mask_pii(text);
        assert!(masked.contains("9876 5432 1098 7654"), "masked: {masked}");
        assert_eq!(count, 0);
    }

    #[test]
    fn luhn_validation_is_correct() {
        assert!(luhn_valid("4111111111111111"));
        assert!(luhn_valid("5500005555555559"));
        assert!(!luhn_valid("1234567890123456"));
        assert!(!luhn_valid("abc"));
    }
}
