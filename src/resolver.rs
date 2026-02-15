//! Shared resolver for YAML 1.1 implicit tag resolution.
//! Used by both the loader and dumper.

pub const DEFAULT_SCALAR_TAG: &str = "tag:yaml.org,2002:str";
pub const DEFAULT_SEQUENCE_TAG: &str = "tag:yaml.org,2002:seq";
pub const DEFAULT_MAPPING_TAG: &str = "tag:yaml.org,2002:map";

/// Resolve the implicit tag for a scalar value.
///
/// When `plain_implicit` is true, the value came from a plain (unquoted) scalar
/// and we check all YAML 1.1 implicit patterns. When false, the value was quoted
/// and defaults to `str`.
pub fn resolve_scalar_tag(value: &str, plain_implicit: bool) -> &'static str {
    if !plain_implicit {
        return DEFAULT_SCALAR_TAG;
    }

    match value {
        "" | "~" | "null" | "Null" | "NULL" => "tag:yaml.org,2002:null",
        "yes" | "Yes" | "YES" | "no" | "No" | "NO" | "true" | "True" | "TRUE" | "false"
        | "False" | "FALSE" | "on" | "On" | "ON" | "off" | "Off" | "OFF" => {
            "tag:yaml.org,2002:bool"
        }
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" | "-.inf" | "-.Inf" | "-.INF"
        | ".nan" | ".NaN" | ".NAN" => "tag:yaml.org,2002:float",
        "<<" => "tag:yaml.org,2002:merge",
        "=" => "tag:yaml.org,2002:value",
        _ => {
            if is_int(value) {
                "tag:yaml.org,2002:int"
            } else if is_float(value) {
                "tag:yaml.org,2002:float"
            } else if is_timestamp(value) {
                "tag:yaml.org,2002:timestamp"
            } else {
                DEFAULT_SCALAR_TAG
            }
        }
    }
}

/// Match YAML 1.1 integer: binary (0b), octal (0), decimal, hex (0x), sexagesimal.
fn is_int(value: &str) -> bool {
    let b = value.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;

    // Optional sign
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
        if i >= b.len() {
            return false;
        }
    }

    // Binary: 0b[0-1_]+
    if b.len() - i >= 2 && b[i] == b'0' && b[i + 1] == b'b' {
        i += 2;
        return i < b.len() && b[i..].iter().all(|&c| matches!(c, b'0' | b'1' | b'_'));
    }

    // Hex: 0x[0-9a-fA-F_]+
    if b.len() - i >= 2 && b[i] == b'0' && b[i + 1] == b'x' {
        i += 2;
        return i < b.len() && b[i..].iter().all(|&c| c.is_ascii_hexdigit() || c == b'_');
    }

    // Octal: 0[0-7_]+
    if b.len() - i >= 2 && b[i] == b'0' && matches!(b[i + 1], b'0'..=b'7' | b'_') {
        return b[i + 1..].iter().all(|&c| matches!(c, b'0'..=b'7' | b'_'));
    }

    // Decimal zero
    if b[i] == b'0' && i + 1 == b.len() {
        return true;
    }

    // Decimal [1-9][0-9_]* or sexagesimal [1-9][0-9_]*(:[0-5]?[0-9])+
    if !(b[i] >= b'1' && b[i] <= b'9') {
        return false;
    }
    i += 1;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    if i == b.len() {
        return true;
    }

    // Sexagesimal suffix: (:[0-5]?[0-9])+
    is_sexa_suffix(&b[i..], false)
}

/// Match YAML 1.1 float (excluding inf/nan which are handled by the caller).
fn is_float(value: &str) -> bool {
    let b = value.as_bytes();
    if b.is_empty() {
        return false;
    }
    let mut i = 0;

    // Float starting with dot (no sign allowed): \.[0-9][0-9_]*([eE][-+][0-9]+)?
    if b[0] == b'.' {
        if b.len() < 2 || !b[1].is_ascii_digit() {
            return false;
        }
        i = 2;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
        return i == b.len() || is_exponent(&b[i..]);
    }

    // Optional sign
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
        if i >= b.len() {
            return false;
        }
    }

    // Leading digits: [0-9][0-9_]*
    if !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    if i >= b.len() {
        return false;
    }

    if b[i] == b'.' {
        // Regular float: [0-9][0-9_]*\.[0-9_]*([eE][-+][0-9]+)?
        i += 1;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
            i += 1;
        }
        return i == b.len() || is_exponent(&b[i..]);
    }

    if b[i] == b':' {
        // Sexagesimal float: [0-9][0-9_]*(:[0-5]?[0-9])+\.[0-9_]*
        return is_sexa_suffix(&b[i..], true);
    }

    false
}

/// Match one or more sexagesimal groups `(:[0-5]?[0-9])+`, optionally followed
/// by `\.[0-9_]*` when `trailing_dot` is true. The slice must start at the first `:`.
fn is_sexa_suffix(b: &[u8], trailing_dot: bool) -> bool {
    if b.is_empty() || b[0] != b':' {
        return false;
    }
    let mut i = 0;
    while i < b.len() && b[i] == b':' {
        i += 1;
        if i >= b.len() || !b[i].is_ascii_digit() {
            return false;
        }
        // [0-5]?[0-9]: try two-digit form first, fall back to one digit
        if b[i] <= b'5' && i + 1 < b.len() && b[i + 1].is_ascii_digit() {
            i += 2;
        } else {
            i += 1;
        }
    }
    if !trailing_dot {
        return i == b.len();
    }
    // Require trailing \.[0-9_]*
    if i >= b.len() || b[i] != b'.' {
        return false;
    }
    i += 1;
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'_') {
        i += 1;
    }
    i == b.len()
}

/// Match an exponent suffix: [eE][-+][0-9]+
fn is_exponent(b: &[u8]) -> bool {
    b.len() >= 3
        && matches!(b[0], b'e' | b'E')
        && matches!(b[1], b'+' | b'-')
        && b[2..].iter().all(|c| c.is_ascii_digit())
}

/// Match YAML 1.1 timestamp: date-only (YYYY-MM-DD) or full datetime.
fn is_timestamp(value: &str) -> bool {
    let b = value.as_bytes();
    // Year: [0-9]{4}-
    if b.len() < 8
        || !b[0].is_ascii_digit()
        || !b[1].is_ascii_digit()
        || !b[2].is_ascii_digit()
        || !b[3].is_ascii_digit()
        || b[4] != b'-'
    {
        return false;
    }
    let mut i = 5;

    // Month: [0-9][0-9]?
    if !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    let two_digit_month = i < b.len() && b[i].is_ascii_digit();
    if two_digit_month {
        i += 1;
    }
    if i >= b.len() || b[i] != b'-' {
        return false;
    }
    i += 1;

    // Day: [0-9][0-9]?
    if i >= b.len() || !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    let two_digit_day = i < b.len() && b[i].is_ascii_digit();
    if two_digit_day {
        i += 1;
    }

    // Date-only form requires exactly YYYY-MM-DD
    if i == b.len() {
        return two_digit_month && two_digit_day;
    }

    // Separator: [Tt] or [ \t]+
    if b[i] == b'T' || b[i] == b't' {
        i += 1;
    } else if b[i] == b' ' || b[i] == b'\t' {
        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
    } else {
        return false;
    }

    // Hour: [0-9][0-9]?
    if i >= b.len() || !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    if i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }

    // :MM
    if i >= b.len() || b[i] != b':' {
        return false;
    }
    i += 1;
    if i + 1 >= b.len() || !b[i].is_ascii_digit() || !b[i + 1].is_ascii_digit() {
        return false;
    }
    i += 2;

    // :SS
    if i >= b.len() || b[i] != b':' {
        return false;
    }
    i += 1;
    if i + 1 >= b.len() || !b[i].is_ascii_digit() || !b[i + 1].is_ascii_digit() {
        return false;
    }
    i += 2;

    // Optional fractional seconds: \.[0-9]*
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }

    if i == b.len() {
        return true;
    }

    // Optional timezone: [ \t]*(Z|[-+][0-9][0-9]?(:[0-9][0-9])?)
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    if i >= b.len() {
        return false;
    }
    if b[i] == b'Z' {
        return i + 1 == b.len();
    }
    if b[i] != b'+' && b[i] != b'-' {
        return false;
    }
    i += 1;

    // Offset hours: [0-9][0-9]?
    if i >= b.len() || !b[i].is_ascii_digit() {
        return false;
    }
    i += 1;
    if i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == b.len() {
        return true;
    }

    // Optional offset minutes: :[0-9][0-9]
    b[i] == b':' && i + 3 == b.len() && b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit()
}
