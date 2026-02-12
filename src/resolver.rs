//! Shared resolver for YAML 1.1 implicit tag resolution.
//! Used by both the loader and dumper.

use std::sync::LazyLock;

use regex::Regex;

pub const DEFAULT_SCALAR_TAG: &str = "tag:yaml.org,2002:str";
pub const DEFAULT_SEQUENCE_TAG: &str = "tag:yaml.org,2002:seq";
pub const DEFAULT_MAPPING_TAG: &str = "tag:yaml.org,2002:map";

// Regex patterns for implicit tag resolution (matching PyYAML's Resolver)
static BOOL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:yes|Yes|YES|no|No|NO|true|True|TRUE|false|False|FALSE|on|On|ON|off|Off|OFF)$")
        .unwrap()
});

static INT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?(?:0|[1-9][0-9_]*)|[-+]?0x[0-9a-fA-F_]+|[-+]?[1-9][0-9_]*(?::[0-5]?[0-9])+)$")
        .unwrap()
});

static FLOAT_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+][0-9]+)?|\.[0-9][0-9_]*(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$")
        .unwrap()
});

static NULL_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:~|null|Null|NULL|)$").unwrap());

static TIMESTAMP_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]|[0-9][0-9][0-9][0-9]-[0-9][0-9]?-[0-9][0-9]?(?:[Tt]|[ \t]+)[0-9][0-9]?:[0-9][0-9]:[0-9][0-9](?:\.[0-9]*)?(?:[ \t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?)$")
        .unwrap()
});

static MERGE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^<<$").unwrap());

static VALUE_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^=$").unwrap());

/// Resolve the implicit tag for a scalar value.
///
/// When `plain_implicit` is true, the value came from a plain (unquoted) scalar
/// and we check all YAML 1.1 implicit patterns. When false, the value was quoted
/// and defaults to `str`.
pub fn resolve_scalar_tag(value: &str, plain_implicit: bool) -> &'static str {
    if !plain_implicit {
        return DEFAULT_SCALAR_TAG;
    }

    if value.is_empty() || NULL_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:null";
    }

    if BOOL_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:bool";
    }

    if INT_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:int";
    }

    if FLOAT_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:float";
    }

    if TIMESTAMP_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:timestamp";
    }

    if MERGE_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:merge";
    }

    if VALUE_PATTERN.is_match(value) {
        return "tag:yaml.org,2002:value";
    }

    DEFAULT_SCALAR_TAG
}
