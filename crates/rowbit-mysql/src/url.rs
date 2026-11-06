//! Connection URLs: the driver's own parameters, plus the TLS parameters
//! other MySQL clients use (`ssl-mode`, `sslaccept`, `ssl`, `ssl-ca`), so a
//! `DATABASE_URL` written for an application works here too.

use std::path::PathBuf;

use mysql_async::{Opts, OptsBuilder, UrlError};

/// How much of the server's certificate to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tls {
    /// No TLS.
    Off,
    /// Encrypt, but accept any certificate.
    Encrypt,
    /// Check the certificate's chain, not its name.
    VerifyCa,
    /// Check the chain and that it names the host.
    VerifyIdentity,
}

/// The connection options for `url`. TLS parameters from other clients
/// become the driver's; anything else unknown is refused by the driver.
pub fn opts(url: &str) -> Result<Opts, UrlError> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, query),
        None => (url, ""),
    };
    let mut tls: Option<Tls> = None;
    let mut root: Option<PathBuf> = None;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let lower = value.to_ascii_lowercase();
        let invalid = || UrlError::InvalidParamValue {
            param: key.to_owned(),
            value: value.to_owned(),
        };
        match key.to_ascii_lowercase().replace('_', "-").as_str() {
            "ssl-mode" | "sslmode" => {
                tls = Some(match lower.replace('_', "-").as_str() {
                    // Rowbit doesn't fall back to plain text, so "prefer"
                    // means the same as not asking.
                    "disabled" | "disable" | "preferred" | "prefer" => Tls::Off,
                    "required" | "require" => Tls::Encrypt,
                    "verify-ca" => Tls::VerifyCa,
                    "verify-identity" | "verify-full" => Tls::VerifyIdentity,
                    _ => return Err(invalid()),
                })
            }
            "sslaccept" => {
                tls = Some(match lower.as_str() {
                    "strict" => Tls::VerifyIdentity,
                    "accept-invalid-certs" | "accept_invalid_certs" => Tls::Encrypt,
                    _ => return Err(invalid()),
                })
            }
            // `ssl=true`, or a JSON object as some Node drivers write it.
            "ssl" | "tls" | "usessl" => {
                tls = Some(match lower.as_str() {
                    "false" | "0" => Tls::Off,
                    _ => Tls::VerifyIdentity,
                })
            }
            "ssl-ca" | "sslrootcert" | "sslca" => root = Some(PathBuf::from(decode(value))),
            "ssl-cert" | "ssl-key" | "sslcert" | "sslkey" | "sslidentity" | "sslpassword" => {
                return Err(UrlError::FeatureRequired {
                    feature: "client certificates, which Rowbit doesn't support yet".into(),
                    param: key.to_owned(),
                });
            }
            _ => kept.push(pair),
        }
    }
    let url = if kept.is_empty() {
        base.to_owned()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    let opts = Opts::from_url(&url)?;
    let tls = match (tls, &root) {
        (Some(tls), _) => tls,
        // A CA without a mode means checking against it.
        (None, Some(_)) => Tls::VerifyIdentity,
        (None, None) => return Ok(opts),
    };
    if tls == Tls::Off {
        return Ok(OptsBuilder::from_opts(opts).ssl_opts(None).into());
    }
    let mut ssl = opts
        .ssl_opts()
        .cloned()
        .unwrap_or_default()
        .with_danger_accept_invalid_certs(tls == Tls::Encrypt)
        .with_danger_skip_domain_validation(tls != Tls::VerifyIdentity);
    if let Some(root) = root {
        ssl = ssl.with_root_certs(vec![root.into()]);
    }
    Ok(OptsBuilder::from_opts(opts).ssl_opts(ssl).into())
}

/// Whether the URL asks for TLS, by any of the parameters [`opts`] reads.
pub fn wants_tls(url: &str) -> bool {
    opts(url).is_ok_and(|o| o.ssl_opts().is_some())
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Some(b) = std::str::from_utf8(hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What the options check, for tests and messages.
pub fn describe(opts: &Opts) -> &'static str {
    match opts.ssl_opts() {
        None => "no TLS",
        Some(s) if s.accept_invalid_certs() => "TLS, any certificate",
        Some(s) if s.skip_domain_validation() => "TLS, certificate chain checked",
        Some(_) => "TLS, certificate and host name checked",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(url: &str) -> &'static str {
        describe(&opts(url).unwrap())
    }

    #[test]
    fn other_clients_tls_parameters_are_understood() {
        assert_eq!(d("mysql://a@h/db"), "no TLS");
        assert_eq!(d("mysql://a@h/db?ssl-mode=DISABLED"), "no TLS");
        assert_eq!(d("mysql://a@h/db?ssl-mode=PREFERRED"), "no TLS");
        assert_eq!(
            d("mysql://a@h/db?ssl-mode=REQUIRED"),
            "TLS, any certificate"
        );
        assert_eq!(d("mysql://a@h/db?sslmode=require"), "TLS, any certificate");
        assert_eq!(
            d("mysql://a@h/db?ssl_mode=VERIFY_CA"),
            "TLS, certificate chain checked"
        );
        assert_eq!(
            d("mysql://a@h/db?ssl-mode=VERIFY_IDENTITY"),
            "TLS, certificate and host name checked"
        );
        assert_eq!(
            d("mysql://a@h/db?sslaccept=strict"),
            "TLS, certificate and host name checked"
        );
        assert_eq!(
            d("mysql://a@h/db?ssl={\"rejectUnauthorized\":true}"),
            "TLS, certificate and host name checked"
        );
        assert_eq!(
            d("mysql://a@h/db?require_ssl=true"),
            "TLS, certificate and host name checked"
        );
        assert_eq!(
            d("mysql://a@h/db?require_ssl=true&verify_identity=false"),
            "TLS, certificate chain checked"
        );
        let o = opts("mysql://a@h/db?ssl-ca=%2Fetc%2Fca.pem&wait_timeout=60").unwrap();
        assert_eq!(describe(&o), "TLS, certificate and host name checked");
        assert_eq!(o.ssl_opts().unwrap().root_certs().len(), 1);
        assert_eq!(o.wait_timeout(), Some(60));
    }

    #[test]
    fn what_rowbit_cant_do_is_refused() {
        assert!(matches!(
            opts("mysql://a@h/db?ssl-mode=SOMETIMES"),
            Err(UrlError::InvalidParamValue { .. })
        ));
        assert!(matches!(
            opts("mysql://a@h/db?ssl-cert=/c.pem"),
            Err(UrlError::FeatureRequired { .. })
        ));
        assert!(matches!(
            opts("mysql://a@h/db?charset=utf8"),
            Err(UrlError::UnknownParameter { .. })
        ));
        assert!(wants_tls("mysql://a@h/db?sslaccept=strict"));
        assert!(!wants_tls("mysql://a@h/db"));
    }
}
