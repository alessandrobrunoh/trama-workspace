//! Path-based rules: files that are secrets by name regardless of content.

use super::SecretRule;

const ENV_TEMPLATES: &[&str] = &["example", "sample", "template", "dist", "defaults"];
const KEY_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "ppk", "asc", "gpg",
];
const KEY_NAMES: &[&str] = &["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"];
const CREDENTIAL_NAMES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".htpasswd",
];
const CREDENTIAL_SUFFIXES: &[&str] = &[
    ".aws/credentials",
    ".aws/config",
    ".git/config",
    ".docker/config.json",
    ".kube/config",
];

pub(super) fn classify(path: &str) -> Option<SecretRule> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if name == ".env" || name.starts_with(".env.") {
        let tail = &name[".env".len()..].trim_start_matches('.');
        if !ENV_TEMPLATES.contains(tail) {
            return Some(SecretRule::DotEnvFile);
        }
    }
    if KEY_NAMES.contains(&name)
        || name
            .rsplit_once('.')
            .is_some_and(|(_, ext)| KEY_EXTENSIONS.contains(&ext))
    {
        return Some(SecretRule::PrivateKeyFile);
    }
    let rooted = format!("/{lower}");
    if CREDENTIAL_NAMES.contains(&name)
        || CREDENTIAL_SUFFIXES
            .iter()
            .any(|s| rooted.ends_with(&format!("/{s}")))
    {
        return Some(SecretRule::CredentialFile);
    }
    None
}
