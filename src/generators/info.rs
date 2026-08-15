//! Information-type generators (SPEC §6): realistic synthetic PII
//! replacements. All are data-free (the input only drives seeding through
//! the engine's consistency mechanism, never leaks into the output), with
//! the exception of shape/format preservation noted per generator.
//!
//! Linkable generators (name, address, geo) derive every part of one entity
//! from the same RNG stream in a fixed draw order, so columns sharing a link
//! label (and thus a seed) produce jointly coherent values.

use super::data::*;
use super::{require_type, CellCtx, Generator, Opts};
use crate::rng::Rng;
use crate::schema::Column;
use crate::value::{ColumnType, Value};
use crate::{verr, Result};

fn text_or_null(input: &Value) -> Option<String> {
    match input {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ---- person entity ----

struct Person {
    first: &'static str,
    last: &'static str,
}

#[allow(clippy::explicit_auto_deref)]
fn draw_person(rng: &mut Rng) -> Person {
    Person {
        first: *rng.choose(FIRST_NAMES),
        last: *rng.choose(LAST_NAMES),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum NamePart {
    First,
    Last,
    Full,
}

struct NameGen {
    part: NamePart,
}

impl Generator for NameGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let p = draw_person(rng);
        Ok(Value::Text(match self.part {
            NamePart::First => p.first.to_string(),
            NamePart::Last => p.last.to_string(),
            NamePart::Full => format!("{} {}", p.first, p.last),
        }))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn name(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["part"])?;
    require_type("name", column, |t| t == ColumnType::Text, "text")?;
    let part = match opts.str_or("part", "full")? {
        "first" => NamePart::First,
        "last" => NamePart::Last,
        "full" => NamePart::Full,
        other => return Err(verr(format!("generator \"name\": part must be first|last|full, got {other:?}"))),
    };
    Ok(Box::new(NameGen { part }))
}

// ---- email ----

struct EmailGen {
    preserve_domain: bool,
}

impl Generator for EmailGen {
    fn generate(&self, input: &Value, ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        let p = draw_person(rng);
        let style = rng.below(3);
        let mut local = match style {
            0 => format!("{}.{}", p.first.to_lowercase(), p.last.to_lowercase()),
            1 => format!("{}{}", p.first.to_lowercase().chars().next().unwrap(), p.last.to_lowercase()),
            _ => format!("{}{}", p.first.to_lowercase(), rng.below(90) + 10),
        };
        // Uniqueness retries get a disambiguating tag.
        if ctx.attempt > 0 {
            local.push_str(&format!("+{}", rng.below(1_000_000)));
        }
        let domain = if self.preserve_domain {
            src.rsplit_once('@').map(|(_, d)| d.to_string())
        } else {
            None
        };
        let domain = domain.unwrap_or_else(|| rng.choose(EMAIL_DOMAINS).to_string());
        Ok(Value::Text(format!("{local}@{domain}")))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn email(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["preserve_domain"])?;
    require_type("email", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(EmailGen { preserve_domain: opts.bool_or("preserve_domain", false)? }))
}

// ---- phone: format-preserving digit replacement, NANP-plausible ----

struct PhoneGen;

impl Generator for PhoneGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        // Replace each digit; make the first digit of a 3-digit run that
        // starts an area/exchange code 2-9 for NANP plausibility.
        let digit_positions: Vec<usize> = src
            .char_indices()
            .filter(|(_, c)| c.is_ascii_digit())
            .map(|(i, _)| i)
            .collect();
        let n = digit_positions.len();
        let mut fresh: Vec<char> = Vec::with_capacity(n);
        for k in 0..n {
            // Area code (position 0 of last 10) and exchange (position 3 of
            // last 10) must start 2-9.
            let from_end = n - k;
            let nanp_lead = n >= 10 && (from_end == 10 || from_end == 7);
            let d = if nanp_lead { 2 + rng.below(8) } else { rng.below(10) };
            fresh.push(char::from(b'0' + d as u8));
        }
        // A leading country "1" (11 digits) is preserved as 1.
        if n == 11 {
            fresh[0] = '1';
        }
        let mut it = fresh.into_iter();
        let out: String =
            src.chars().map(|c| if c.is_ascii_digit() { it.next().unwrap() } else { c }).collect();
        Ok(Value::Text(out))
    }

    fn is_data_free(&self) -> bool {
        true // digits are fresh; only punctuation shape is preserved
    }
}

pub fn phone(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("phone", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(PhoneGen))
}

// ---- address entity ----

struct AddressEntity {
    number: u64,
    street: String,
    city: &'static str,
    state: &'static str,
    zip: String,
}

fn draw_address(rng: &mut Rng) -> AddressEntity {
    let number = 1 + rng.below(9899);
    let street = format!("{} {}", rng.choose(STREET_NAMES), rng.choose(STREET_SUFFIXES));
    let (city, state, zip3) = *rng.choose(CITIES);
    let zip = format!("{zip3}{:02}", rng.below(100));
    AddressEntity { number, street, city, state, zip }
}

#[derive(Clone, Copy, PartialEq)]
enum AddressPart {
    Street,
    City,
    State,
    Zip,
    Full,
}

struct AddressGen {
    part: AddressPart,
}

impl Generator for AddressGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let a = draw_address(rng);
        Ok(Value::Text(match self.part {
            AddressPart::Street => format!("{} {}", a.number, a.street),
            AddressPart::City => a.city.to_string(),
            AddressPart::State => a.state.to_string(),
            AddressPart::Zip => a.zip,
            AddressPart::Full => {
                format!("{} {}, {}, {} {}", a.number, a.street, a.city, a.state, a.zip)
            }
        }))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn address(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["part"])?;
    require_type("address", column, |t| t == ColumnType::Text, "text")?;
    let part = match opts.str_or("part", "full")? {
        "street" => AddressPart::Street,
        "city" => AddressPart::City,
        "state" => AddressPart::State,
        "zip" => AddressPart::Zip,
        "full" => AddressPart::Full,
        other => {
            return Err(verr(format!(
                "generator \"address\": part must be street|city|state|zip|full, got {other:?}"
            )))
        }
    };
    Ok(Box::new(AddressGen { part }))
}

// ---- business_name ----

struct BusinessName;

impl Generator for BusinessName {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        let head = rng.choose(BUSINESS_HEADS);
        let tail = rng.choose(BUSINESS_TAILS);
        Ok(Value::Text(if rng.below(2) == 0 {
            format!("{head} {} {tail}", rng.choose(BUSINESS_MIDS))
        } else {
            format!("{head} {tail}")
        }))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn business_name(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("business_name", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(BusinessName))
}

// ---- ssn ----

struct SsnGen;

impl Generator for SsnGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        // Valid-shape but never-issued-adjacent: area 900-999 is reserved for
        // ITINs and never issued as SSN area numbers... but that range is
        // recognizable. Instead use ordinary shape while avoiding the
        // forbidden values (area 000/666, group 00, serial 0000).
        let area = loop {
            let a = 1 + rng.below(898); // 001..=899, skips 000 and 900+
            if a != 666 {
                break a;
            }
        };
        let group = 1 + rng.below(99);
        let serial = 1 + rng.below(9999);
        let dashed = src.contains('-');
        Ok(Value::Text(if dashed {
            format!("{area:03}-{group:02}-{serial:04}")
        } else {
            format!("{area:03}{group:02}{serial:04}")
        }))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn ssn(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("ssn", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(SsnGen))
}

// ---- ip_address ----

struct IpGen {
    documentation_range: bool,
}

impl Generator for IpGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        let v6 = src.contains(':');
        Ok(Value::Text(if v6 {
            if self.documentation_range {
                // 2001:db8::/32 (RFC 3849)
                format!(
                    "2001:db8:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
                    rng.below(0x10000),
                    rng.below(0x10000),
                    rng.below(0x10000),
                    rng.below(0x10000),
                    rng.below(0x10000),
                    rng.below(0x10000)
                )
            } else {
                let mut parts = Vec::with_capacity(8);
                for _ in 0..8 {
                    parts.push(format!("{:x}", rng.below(0x10000)));
                }
                parts.join(":")
            }
        } else if self.documentation_range {
            // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 (RFC 5737)
            let base = ["192.0.2", "198.51.100", "203.0.113"][rng.below(3) as usize];
            format!("{base}.{}", rng.below(256))
        } else {
            format!(
                "{}.{}.{}.{}",
                1 + rng.below(223), // avoid 0 and multicast/reserved 224+
                rng.below(256),
                rng.below(256),
                rng.below(256)
            )
        }))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn ip_address(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["documentation_range"])?;
    require_type("ip_address", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(IpGen { documentation_range: opts.bool_or("documentation_range", true)? }))
}

// ---- mac_address ----

struct MacGen;

impl Generator for MacGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        let sep = if src.contains('-') { '-' } else { ':' };
        let upper = src.chars().any(|c| c.is_ascii_uppercase());
        // Set the locally-administered bit, clear multicast: x2/x6/xA/xE.
        let first = (rng.below(256) as u8 & 0xfc) | 0x02;
        let mut parts = vec![format!("{first:02x}")];
        for _ in 0..5 {
            parts.push(format!("{:02x}", rng.below(256)));
        }
        let mut s = parts.join(&sep.to_string());
        if upper {
            s = s.to_uppercase();
        }
        Ok(Value::Text(s))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn mac_address(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("mac_address", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(MacGen))
}

// ---- hostname / url ----

fn draw_hostname(rng: &mut Rng) -> String {
    format!(
        "{}-{}.{}",
        rng.choose(HOST_WORDS),
        rng.choose(HOST_WORDS),
        rng.choose(TLDS)
    )
}

struct HostnameGen;

impl Generator for HostnameGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        Ok(Value::Text(draw_hostname(rng)))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn hostname(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("hostname", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(HostnameGen))
}

struct UrlGen;

impl Generator for UrlGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        // Preserve scheme and path depth; replace host and path words.
        let scheme = if src.starts_with("http://") { "http" } else { "https" };
        let path_depth = src
            .split_once("://")
            .map(|(_, rest)| rest.split('/').skip(1).filter(|s| !s.is_empty()).count())
            .unwrap_or(0)
            .min(6);
        let host = draw_hostname(rng);
        let mut url = format!("{scheme}://{host}");
        for _ in 0..path_depth {
            url.push('/');
            url.push_str(rng.choose(URL_PATH_WORDS).as_ref());
        }
        Ok(Value::Text(url))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn url(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("url", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(UrlGen))
}

// ---- file_name ----

struct FileNameGen;

impl Generator for FileNameGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        let ext = src.rsplit_once('.').map(|(_, e)| e.to_string());
        let base = format!("{}_{}", rng.choose(FILE_WORDS), rng.below(10_000));
        Ok(Value::Text(match ext {
            Some(e) if !e.is_empty() && e.len() <= 8 => format!("{base}.{e}"),
            _ => base,
        }))
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn file_name(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("file_name", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(FileNameGen))
}

// ---- geo ----

#[derive(Clone, Copy, PartialEq)]
enum GeoPart {
    Lat,
    Lon,
}

struct GeoGen {
    part: GeoPart,
    is_text: bool,
}

impl Generator for GeoGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        if input.is_null() {
            return Ok(Value::Null);
        }
        // Draw lat then lon from one stream so linked columns cohere.
        let lat = rng.range_f64(-56.0, 71.0); // populated latitudes
        let lon = rng.range_f64(-180.0, 180.0);
        let v = match self.part {
            GeoPart::Lat => lat,
            GeoPart::Lon => lon,
        };
        let v = (v * 1e6).round() / 1e6;
        Ok(if self.is_text { Value::Text(format!("{v}")) } else { Value::Float(v) })
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn geo(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&["part"])?;
    require_type("geo", column, |t| t.is_numeric() || t == ColumnType::Text, "numeric or text")?;
    let part = match opts.str("part")? {
        Some("lat") => GeoPart::Lat,
        Some("lon") | Some("lng") => GeoPart::Lon,
        None => return Err(verr("generator \"geo\": option \"part\" (lat|lon) is required")),
        Some(other) => return Err(verr(format!("generator \"geo\": part must be lat|lon, got {other:?}"))),
    };
    Ok(Box::new(GeoGen { part, is_text: column.ty == ColumnType::Text }))
}

// ---- credit_card ----

pub(crate) fn luhn_checksum(digits: &[u8]) -> u8 {
    // Compute the check digit that makes the sequence Luhn-valid.
    let mut sum = 0u32;
    // Positions counted from the right of the FULL number (incl. check digit
    // at rightmost, position 0). digits here excludes the check digit.
    for (i, &d) in digits.iter().rev().enumerate() {
        let mut v = d as u32;
        if i % 2 == 0 {
            // Doubled positions (adjacent to check digit).
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    ((10 - (sum % 10)) % 10) as u8
}

pub fn luhn_valid(digits: &[u8]) -> bool {
    if digits.len() < 2 {
        return false;
    }
    let (body, check) = digits.split_at(digits.len() - 1);
    luhn_checksum(body) == check[0]
}

struct CreditCardGen;

impl Generator for CreditCardGen {
    fn generate(&self, input: &Value, _ctx: &CellCtx, rng: &mut Rng) -> Result<Value> {
        let src = match text_or_null(input) {
            None => return Ok(Value::Null),
            Some(s) => s,
        };
        let src_digits: Vec<u8> = src
            .chars()
            .filter(|c| c.is_ascii_digit())
            .map(|c| c as u8 - b'0')
            .collect();
        let len = if (12..=19).contains(&src_digits.len()) { src_digits.len() } else { 16 };
        // Preserve the leading digit (brand family) when plausible, else 4.
        let lead = match src_digits.first() {
            Some(&d) if (3..=6).contains(&d) => d,
            _ => 4,
        };
        let mut digits = vec![lead];
        while digits.len() < len - 1 {
            digits.push(rng.below(10) as u8);
        }
        digits.push(luhn_checksum(&digits));
        // Re-flow into the source grouping when shapes align, else 4-4-4-4.
        let mut it = digits.iter();
        let same_shape = src_digits.len() == len;
        let out = if same_shape {
            src.chars()
                .map(|c| {
                    if c.is_ascii_digit() {
                        char::from(b'0' + *it.next().unwrap())
                    } else {
                        c
                    }
                })
                .collect::<String>()
        } else {
            digits
                .chunks(4)
                .map(|ch| ch.iter().map(|d| char::from(b'0' + d)).collect::<String>())
                .collect::<Vec<_>>()
                .join(" ")
        };
        Ok(Value::Text(out))
    }

    fn supports_uniqueness(&self) -> bool {
        true
    }

    fn is_data_free(&self) -> bool {
        true
    }
}

pub fn credit_card(opts: &Opts, column: &Column) -> Result<Box<dyn Generator>> {
    opts.check_known(&[])?;
    require_type("credit_card", column, |t| t == ColumnType::Text, "text")?;
    Ok(Box::new(CreditCardGen))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::*;
    use super::*;

    #[test]
    fn name_parts() {
        let full = quick("name", "{}", "Original Person");
        let s = full.as_text().unwrap();
        assert!(s.contains(' '));
        assert_ne!(s, "Original Person");
        let first = quick("name", r#"{"part": "first"}"#, "X");
        assert!(FIRST_NAMES.contains(&first.as_text().unwrap()));
    }

    #[test]
    fn email_shape_and_domain_preservation() {
        let v = quick("email", "{}", "real.person@corp.com");
        let s = v.as_text().unwrap();
        assert!(s.contains('@'));
        assert!(!s.contains("real"));
        assert!(EMAIL_DOMAINS.iter().any(|d| s.ends_with(d)), "{s}");
        let v = quick("email", r#"{"preserve_domain": true}"#, "real.person@corp.com");
        assert!(v.as_text().unwrap().ends_with("@corp.com"));
    }

    #[test]
    fn phone_preserves_format() {
        let v = quick("phone", "{}", "(555) 123-4567");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 14);
        assert!(s.starts_with('('));
        assert_eq!(&s[4..6], ") ");
        assert_eq!(s.chars().nth(9), Some('-'));
        // NANP: area code and exchange lead digits 2-9
        let digits: Vec<char> = s.chars().filter(|c| c.is_ascii_digit()).collect();
        assert!(digits[0] >= '2');
        assert!(digits[3] >= '2');
        // Country code preserved
        let v = quick("phone", "{}", "+1-800-555-0100");
        assert!(v.as_text().unwrap().starts_with("+1-"));
    }

    #[test]
    fn ssn_valid_shape() {
        let v = quick("ssn", "{}", "123-45-6789");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 11);
        let area: u32 = s[0..3].parse().unwrap();
        assert!(area != 0 && area != 666 && area < 900);
        assert_ne!(&s[4..6], "00");
        assert_ne!(&s[7..11], "0000");
        // Dashless input → dashless output
        let v = quick("ssn", "{}", "123456789");
        assert_eq!(v.as_text().unwrap().len(), 9);
    }

    #[test]
    fn ip_documentation_ranges() {
        let v = quick("ip_address", "{}", "10.1.2.3");
        let s = v.as_text().unwrap();
        assert!(
            s.starts_with("192.0.2.") || s.starts_with("198.51.100.") || s.starts_with("203.0.113."),
            "{s}"
        );
        let v6 = quick("ip_address", "{}", "fe80::1");
        assert!(v6.as_text().unwrap().starts_with("2001:db8:"));
    }

    #[test]
    fn mac_local_bit() {
        let v = quick("mac_address", "{}", "aa:bb:cc:dd:ee:ff");
        let s = v.as_text().unwrap();
        let first = u8::from_str_radix(&s[0..2], 16).unwrap();
        assert_eq!(first & 0x03, 0x02); // locally administered, unicast
        let v = quick("mac_address", "{}", "AA-BB-CC-DD-EE-FF");
        assert!(v.as_text().unwrap().contains('-'));
    }

    #[test]
    fn url_preserves_shape() {
        let v = quick("url", "{}", "https://internal.corp.com/a/b/c");
        let s = v.as_text().unwrap();
        assert!(s.starts_with("https://"));
        assert_eq!(s.matches('/').count(), 5); // scheme 2 + 3 path segments
        assert!(!s.contains("corp.com"));
    }

    #[test]
    fn file_name_keeps_extension() {
        let v = quick("file_name", "{}", "salary_data_2024.xlsx");
        let s = v.as_text().unwrap();
        assert!(s.ends_with(".xlsx"));
        assert!(!s.contains("salary"));
    }

    #[test]
    fn luhn() {
        // 4539578763621486 is a classic Luhn-valid test number.
        let d: Vec<u8> = "4539578763621486".bytes().map(|b| b - b'0').collect();
        assert!(luhn_valid(&d));
        let mut bad = d.clone();
        bad[15] = (bad[15] + 1) % 10;
        assert!(!luhn_valid(&bad));
    }

    #[test]
    fn credit_card_luhn_valid_and_shaped() {
        let v = quick("credit_card", "{}", "4111 1111 1111 1111");
        let s = v.as_text().unwrap();
        assert_eq!(s.len(), 19);
        assert_eq!(s.matches(' ').count(), 3);
        let digits: Vec<u8> = s.chars().filter(|c| c.is_ascii_digit()).map(|c| c as u8 - b'0').collect();
        assert_eq!(digits.len(), 16);
        assert_eq!(digits[0], 4); // brand preserved
        assert!(luhn_valid(&digits));
        assert_ne!(s, "4111 1111 1111 1111");
    }

    #[test]
    fn geo_bounds() {
        let table = test_table();
        let c = col(&table, "f");
        let opts = crate::json::Json::parse(r#"{"part": "lat"}"#).unwrap();
        let b = super::super::build("geo", &opts, &table, &c).unwrap();
        match run_gen(&b, Value::Float(12.0), &table, vec![Value::Null; 7]) {
            Value::Float(lat) => assert!((-90.0..=90.0).contains(&lat)),
            other => panic!("expected float, got {other:?}"),
        }
    }

    #[test]
    fn linked_name_and_email_cohere() {
        // Same seed (same link entity) → same underlying person.
        let table = test_table();
        let c = col(&table, "c");
        let opts = crate::json::Json::parse(r#"{"part": "first"}"#).unwrap();
        let first_gen = super::super::build("name", &opts, &table, &c).unwrap();
        let opts2 = crate::json::Json::parse(r#"{"part": "last"}"#).unwrap();
        let last_gen = super::super::build("name", &opts2, &table, &c).unwrap();
        let opts3 = crate::json::Json::parse("{}").unwrap();
        let full_gen = super::super::build("name", &opts3, &table, &c).unwrap();

        let digest = crate::crypto::hmac_parts(b"k", &[b"link", b"person", b"row-1"]);
        let ctx = CellCtx { table: &table, row: &vec![Value::Null; 7], row_ordinal: 0, attempt: 0, secret: b"k" };
        let f = first_gen.gen.generate(&Value::Text("x".into()), &ctx, &mut Rng::from_digest(&digest)).unwrap();
        let l = last_gen.gen.generate(&Value::Text("y".into()), &ctx, &mut Rng::from_digest(&digest)).unwrap();
        let full = full_gen.gen.generate(&Value::Text("z".into()), &ctx, &mut Rng::from_digest(&digest)).unwrap();
        assert_eq!(
            format!("{} {}", f.as_text().unwrap(), l.as_text().unwrap()),
            full.as_text().unwrap()
        );
    }
}
