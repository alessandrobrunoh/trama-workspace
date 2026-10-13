//! Secret egress preflight: scan what a share would export for credentials.
//!
//! Any finding blocks the share (v1 has no "send anyway" override). This is
//! risk reduction, **not** a guarantee (plan 06 §6): rules are deliberately
//! broad, and findings never contain the matched secret, only a redacted hint.

mod content;
mod entropy;
mod path;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

/// Files larger than this are not content-scanned and are reported as
/// [`SecretRule::TooLargeToScan`], which also blocks: unknown is unsafe.
pub const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretRule {
    /// `.env`, `.env.local`… (templates like `.env.example` are allowed).
    DotEnvFile,
    /// Key/cert containers: `*.pem`, `*.key`, `*.p12`, `id_rsa`…
    PrivateKeyFile,
    /// Credential stores: `.aws/credentials`, `.netrc`, `.npmrc`, `.git/config`…
    CredentialFile,
    /// `-----BEGIN … PRIVATE KEY-----`.
    PrivateKeyBlock,
    AwsAccessKeyId,
    AwsSecretAccessKey,
    GcpServiceAccount,
    GcpApiKey,
    GithubToken,
    SlackToken,
    /// Long, random-looking token (Shannon entropy over threshold).
    HighEntropyString,
    TooLargeToScan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretFinding {
    /// Path relative to the repository root, `/`-separated.
    pub path: String,
    /// 1-based line; `None` for path rules.
    pub line: Option<u32>,
    pub rule: SecretRule,
    /// Safe hint such as `ghp_…(40 chars)`; never the secret.
    pub hint: String,
}

/// Result of a preflight. A non-empty report blocks the share.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretScanReport {
    pub findings: Vec<SecretFinding>,
}

impl SecretScanReport {
    pub fn blocks_share(&self) -> bool {
        !self.findings.is_empty()
    }

    /// Scans one file (path rules, then content unless binary) and records
    /// findings.
    pub fn scan_file(&mut self, path: &str, bytes: &[u8]) {
        let path = path.replace('\\', "/");
        if let Some(rule) = path::classify(&path) {
            self.push(&path, None, rule, String::new());
        }
        if bytes.len() > MAX_SCAN_BYTES {
            self.push(
                &path,
                None,
                SecretRule::TooLargeToScan,
                format!("{} bytes", bytes.len()),
            );
            return;
        }
        if bytes.contains(&0) {
            return; // binary: path rules only
        }
        let text = String::from_utf8_lossy(bytes);
        for (line, rule, hint) in content::scan(&text) {
            self.push(&path, Some(line), rule, hint);
        }
    }

    fn push(&mut self, path: &str, line: Option<u32>, rule: SecretRule, hint: String) {
        self.findings.push(SecretFinding {
            path: path.to_owned(),
            line,
            rule,
            hint,
        });
    }
}

/// Scans a set of `(path, bytes)` files.
pub fn scan_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> SecretScanReport {
    let mut report = SecretScanReport::default();
    for (path, bytes) in files {
        report.scan_file(path, bytes);
    }
    report
}

/// Redacted hint: first 4 chars and the length.
pub(crate) fn redact(secret: &str) -> String {
    let head: String = secret.chars().take(4).collect();
    format!("{head}…({} chars)", secret.chars().count())
}
