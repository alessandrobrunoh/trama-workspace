use super::*;

// Fixtures are assembled at runtime so this file itself never contains a
// credential-shaped literal (keeps push protection and our own scan quiet).
fn fake(prefix: &str, body: &str, n: usize) -> String {
    format!("{prefix}{}", body.repeat(n))
}

fn rules(path: &str, content: &str) -> Vec<SecretRule> {
    scan_files([(path, content.as_bytes())])
        .findings
        .into_iter()
        .map(|f| f.rule)
        .collect()
}

#[test]
fn path_rules() {
    assert_eq!(rules(".env", ""), [SecretRule::DotEnvFile]);
    assert_eq!(rules("app/.env.production", ""), [SecretRule::DotEnvFile]);
    assert!(rules(".env.example", "").is_empty());
    assert_eq!(rules("certs/server.PEM", ""), [SecretRule::PrivateKeyFile]);
    assert_eq!(
        rules("home/.ssh/id_ed25519", ""),
        [SecretRule::PrivateKeyFile]
    );
    assert_eq!(rules(".aws/credentials", ""), [SecretRule::CredentialFile]);
    assert_eq!(rules(r"sub\.git\config", ""), [SecretRule::CredentialFile]);
    assert!(rules("src/main.rs", "fn main() {}").is_empty());
    assert!(rules("src/environment.rs", "").is_empty());
}

#[test]
fn content_rules() {
    let pem = format!("-----BEGIN {} PRIVATE KEY-----", "OPENSSH");
    assert_eq!(rules("a.txt", &pem), [SecretRule::PrivateKeyBlock]);
    let aws = fake("AKIA", "ABCD2345", 2);
    assert_eq!(
        rules("a.txt", &format!("id = {aws}")),
        [SecretRule::AwsAccessKeyId]
    );
    let gh = fake("ghp_", "aB3dE5fG7h", 4);
    assert_eq!(
        rules("a.txt", &format!("token: {gh}")),
        [SecretRule::GithubToken]
    );
    let gcp = fake("AIza", "Sy0_aB-7xQ", 3) + "12345";
    assert_eq!(
        rules("a.js", &format!("k='{gcp}'")),
        [SecretRule::GcpApiKey]
    );
    let slack = fake("xoxb-", "1234-", 4);
    assert_eq!(rules("a.txt", &slack), [SecretRule::SlackToken]);
    let sa = "{\"type\": \"service_account\",\n \"private_key\": \"x\"}";
    assert_eq!(rules("sa.json", sa), [SecretRule::GcpServiceAccount]);
}

#[test]
fn aws_secret_and_entropy() {
    let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    let found = rules("cfg.ini", &format!("aws_secret_access_key = {secret}"));
    assert!(found.contains(&SecretRule::AwsSecretAccessKey));
    let random = "Zx8qL2mNv5Rt9Wb3Kc7Hy1Pd4Gf6Js0Ue2Ao8Ti";
    assert_eq!(
        rules("a.rs", &format!("let k = \"{random}\";")),
        [SecretRule::HighEntropyString]
    );
}

#[test]
fn no_false_positives_on_common_text() {
    let oid = "3b18e512dba79e4c8300dd08aeb37f8e728b8dad";
    let text = format!(
        "commit {oid}\nuse std::collections::HashMap;\nhttps://example.com/some/long/path/to/resource\n\
         aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nlet very_long_identifier_name_here = 1;"
    );
    assert!(
        rules("a.rs", &text).is_empty(),
        "{:?}",
        rules("a.rs", &text)
    );
}

#[test]
fn findings_never_leak_secret_and_block() {
    let gh = fake("ghp_", "aB3dE5fG7h", 4);
    let report = scan_files([("x/y.txt", format!("\n\n{gh}").as_bytes())]);
    assert!(report.blocks_share());
    let f = &report.findings[0];
    assert_eq!((f.path.as_str(), f.line), ("x/y.txt", Some(3)));
    assert!(!f.hint.contains(&gh[4..]));
    assert!(!scan_files([("ok.txt", b"hello".as_slice())]).blocks_share());
}

#[test]
fn binary_and_oversize() {
    let mut bin = format!("-----BEGIN {} PRIVATE KEY-----", "RSA").into_bytes();
    bin.push(0);
    assert!(
        scan_files([("blob.bin", bin.as_slice())])
            .findings
            .is_empty()
    );
    let big = vec![b'a'; MAX_SCAN_BYTES + 1];
    assert_eq!(rules_bytes("big.txt", &big), [SecretRule::TooLargeToScan]);
}

fn rules_bytes(path: &str, b: &[u8]) -> Vec<SecretRule> {
    scan_files([(path, b)])
        .findings
        .into_iter()
        .map(|f| f.rule)
        .collect()
}
