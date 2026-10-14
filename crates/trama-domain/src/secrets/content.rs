//! Content rules: credential-like tokens inside text files.

use super::{SecretRule, entropy, redact};

/// Minimum length and entropy for [`SecretRule::HighEntropyString`]. 4.5 bits
/// keeps hex digests (≤ 4.0, e.g. Git OIDs) out while catching base64 keys.
const ENTROPY_MIN_LEN: usize = 32;
const ENTROPY_MIN_BITS: f64 = 4.5;

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=')
}

pub(super) fn scan(text: &str) -> Vec<(u32, SecretRule, String)> {
    let mut out = Vec::new();
    let gcp_sa = text.contains("\"service_account\"") && text.contains("\"private_key\"");
    for (i, line) in text.lines().enumerate() {
        let n = i as u32 + 1;
        let mut push = |rule, hint: String| out.push((n, rule, hint));
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            push(SecretRule::PrivateKeyBlock, "PEM private key".into());
        }
        if gcp_sa && line.contains("\"private_key\"") {
            push(SecretRule::GcpServiceAccount, "service account key".into());
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("aws_secret_access_key") || lower.contains("secretaccesskey") {
            if let Some(v) = assigned_value(line).filter(|v| v.len() == 40) {
                push(SecretRule::AwsSecretAccessKey, redact(v));
            }
        }
        for tok in line
            .split(|c: char| !token_char(c))
            .filter(|t| t.len() >= 16)
        {
            if let Some(rule) = token_rule(tok) {
                push(rule, redact(tok));
            } else if tok.len() >= ENTROPY_MIN_LEN
                && tok.len() <= 512
                && mixed(tok)
                && entropy::shannon(tok) >= ENTROPY_MIN_BITS
            {
                push(SecretRule::HighEntropyString, redact(tok));
            }
        }
    }
    out
}

fn token_rule(tok: &str) -> Option<SecretRule> {
    let alnum = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric());
    let upper_num = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    };
    for p in ["AKIA", "ASIA", "AGPA", "AROA"] {
        if let Some(rest) = tok.strip_prefix(p) {
            if rest.len() == 16 && upper_num(rest) {
                return Some(SecretRule::AwsAccessKeyId);
            }
        }
    }
    for p in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if let Some(rest) = tok.strip_prefix(p) {
            if rest.len() >= 36 && alnum(rest) {
                return Some(SecretRule::GithubToken);
            }
        }
    }
    if tok
        .strip_prefix("github_pat_")
        .is_some_and(|r| r.len() >= 22)
    {
        return Some(SecretRule::GithubToken);
    }
    if tok.strip_prefix("AIza").is_some_and(|r| r.len() == 35) {
        return Some(SecretRule::GcpApiKey);
    }
    if ["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"]
        .iter()
        .any(|p| tok.starts_with(p))
    {
        return Some(SecretRule::SlackToken);
    }
    None
}

/// Has letters and digits: excludes words, paths of letters, long numbers.
fn mixed(tok: &str) -> bool {
    tok.bytes().any(|b| b.is_ascii_digit()) && tok.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Value after `=` or `:`, unquoted.
fn assigned_value(line: &str) -> Option<&str> {
    let (_, v) = line.split_once(['=', ':'])?;
    let v = v.trim().trim_matches(|c| c == '"' || c == '\'' || c == ',');
    (!v.is_empty()).then_some(v)
}
