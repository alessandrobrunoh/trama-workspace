//! TLS client configuration for `https://` and `wss://` collab endpoints.
//!
//! Policy (ADR 0014): the certificate chain and the host name are always
//! verified with the platform trust store, optionally extended by one extra CA
//! bundle named by `TRAMA_COLLAB_CA_FILE` (a private CA). There is no option,
//! environment variable or build flag that turns verification off.
//!
//! Library choice: rustls with the `ring` provider, `tokio-rustls` for the
//! handshake and `rustls-native-certs` for the system roots. The workspace
//! lockfile already contains all of them (rustls 0.23 + ring through the UI
//! stack's HTTP client), so this adds no new native code or C toolchain
//! requirement; `aws-lc-rs` would, and `native-tls`/OpenSSL would link a
//! second TLS stack. System roots (rather than a bundled `webpki-roots` list)
//! let the operating system's revocation and enterprise-CA policy apply.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rustls::{ClientConfig, RootCertStore};
use rustls_pki_types::{CertificateDer, pem::PemObject};

/// Environment variable naming an extra PEM CA bundle to trust.
pub(crate) const CA_FILE_ENV: &str = "TRAMA_COLLAB_CA_FILE";
/// A CA bundle larger than this is refused (it is a handful of certificates).
const MAX_CA_FILE_BYTES: u64 = 1024 * 1024;

/// The CA file named by [`CA_FILE_ENV`], if set to a non-empty value.
pub(crate) fn ca_file_from_env() -> Option<PathBuf> {
    std::env::var_os(CA_FILE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// Client configuration trusting the system roots plus `extra_ca`, cached per
/// CA path for the life of the process (restart to pick up a replaced file).
pub(crate) fn client_config(extra_ca: Option<&Path>) -> Result<Arc<ClientConfig>, String> {
    static CACHE: OnceLock<Mutex<HashMap<Option<PathBuf>, Arc<ClientConfig>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let key = extra_ca.map(Path::to_path_buf);
    if let Some(config) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return Ok(config.clone());
    }
    let config = build_config(extra_ca)?;
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, config.clone());
    Ok(config)
}

fn build_config(extra_ca: Option<&Path>) -> Result<Arc<ClientConfig>, String> {
    let mut roots = RootCertStore::empty();
    let native = system_roots();
    roots.add_parsable_certificates(native.iter().cloned());
    if let Some(path) = extra_ca {
        let added = add_ca_file(&mut roots, path)?;
        if added == 0 {
            return Err(format!(
                "{CA_FILE_ENV} ({}) contains no usable CA certificate",
                path.display()
            ));
        }
    }
    if roots.is_empty() {
        return Err("no trusted root certificates are available on this system".into());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|err| format!("TLS setup failed: {err}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Both the request client and the WebSocket upgrade speak HTTP/1.1.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The platform trust store, read once per process.
fn system_roots() -> &'static [CertificateDer<'static>] {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| rustls_native_certs::load_native_certs().certs)
}

fn add_ca_file(roots: &mut RootCertStore, path: &Path) -> Result<usize, String> {
    let describe = |what: &str| format!("{CA_FILE_ENV} ({}): {what}", path.display());
    let metadata = std::fs::metadata(path).map_err(|err| describe(&err.to_string()))?;
    if !metadata.is_file() || metadata.len() > MAX_CA_FILE_BYTES {
        return Err(describe("must be a regular file of at most 1 MiB"));
    }
    let bytes = std::fs::read(path).map_err(|err| describe(&err.to_string()))?;
    let mut added = 0;
    // Only CERTIFICATE blocks are read; a private key in the same file is
    // never parsed or used.
    for certificate in CertificateDer::pem_slice_iter(&bytes) {
        let certificate =
            certificate.map_err(|_| describe("not a valid PEM certificate bundle"))?;
        roots
            .add(certificate)
            .map_err(|_| describe("contains a certificate that cannot be used as a CA"))?;
        added += 1;
    }
    Ok(added)
}
