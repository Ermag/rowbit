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

/// The connection options for `url`. TLS parameters, the driver's and other
/// clients', become one setting; anything else unknown is refused by the
/// driver. Keys are compared as written, not percent-decoded, so an encoded
/// key fails closed in the driver.
pub fn opts(url: &str) -> Result<Opts, UrlError> {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, query),
        None => (url, ""),
    };
    // Every TLS setting the URL makes, with the key that made it.
    let mut asked: Vec<(String, Tls)> = Vec::new();
    // The driver's own require_ssl, loosened by verify_ca / verify_identity.
    let (mut require_ssl, mut verify_ca, mut verify_identity) = (None, true, true);
    let mut root: Option<PathBuf> = None;
    let mut kept = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let lower = decode(value).to_ascii_lowercase();
        let invalid = || UrlError::InvalidParamValue {
            param: key.to_owned(),
            value: value.to_owned(),
        };
        let flag = || match lower.as_str() {
            "true" | "1" | "yes" | "on" => Ok(true),
            "false" | "0" | "no" | "off" => Ok(false),
            _ => Err(invalid()),
        };
        let setting = match key.to_ascii_lowercase().replace('_', "-").as_str() {
            "ssl-mode" | "sslmode" => match lower.replace('_', "-").as_str() {
                "disabled" | "disable" => Tls::Off,
                // Rowbit never falls back to plain text, so "prefer" means
                // encrypting, as "required" does.
                "preferred" | "prefer" | "required" | "require" => Tls::Encrypt,
                "verify-ca" => Tls::VerifyCa,
                "verify-identity" | "verify-full" => Tls::VerifyIdentity,
                _ => return Err(invalid()),
            },
            "sslaccept" => match lower.as_str() {
                "strict" => Tls::VerifyIdentity,
                "accept-invalid-certs" | "accept_invalid_certs" => Tls::Encrypt,
                _ => return Err(invalid()),
            },
            // `ssl=true`, or a JSON object as some Node drivers write it.
            "ssl" | "tls" | "usessl" if lower.starts_with('{') => {
                let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
                if compact.contains("\"rejectunauthorized\":false") {
                    Tls::Encrypt
                } else {
                    Tls::VerifyIdentity
                }
            }
            "ssl" | "tls" | "usessl" => {
                if flag()? {
                    Tls::VerifyIdentity
                } else {
                    Tls::Off
                }
            }
            "require-ssl" => {
                require_ssl = Some(flag()?);
                continue;
            }
            "verify-ca" => {
                verify_ca = flag()?;
                continue;
            }
            "verify-identity" => {
                verify_identity = flag()?;
                continue;
            }
            "ssl-ca" | "sslrootcert" | "sslca" => {
                let path = PathBuf::from(decode(value));
                if !path.is_absolute() {
                    return Err(UrlError::InvalidParamValue {
                        param: key.to_owned(),
                        value: format!("{value} (give the CA file's absolute path)"),
                    });
                }
                root = Some(path);
                continue;
            }
            "ssl-cert" | "ssl-key" | "sslcert" | "sslkey" | "sslidentity" | "sslpassword" => {
                return Err(UrlError::FeatureRequired {
                    feature: "client certificates, which Rowbit doesn't support yet".into(),
                    param: key.to_owned(),
                });
            }
            _ => {
                kept.push(pair);
                continue;
            }
        };
        asked.push((key.to_owned(), setting));
    }
    if let Some(on) = require_ssl {
        let setting = match (on, verify_ca, verify_identity) {
            (false, ..) => Tls::Off,
            (true, false, _) => Tls::Encrypt,
            (true, true, false) => Tls::VerifyCa,
            (true, true, true) => Tls::VerifyIdentity,
        };
        asked.push(("require_ssl".into(), setting));
    }
    let tls = match asked.first() {
        Some((_, first)) => {
            if let Some((key, _)) = asked.iter().find(|(_, t)| t != first) {
                return Err(UrlError::InvalidParamValue {
                    param: key.clone(),
                    value: format!(
                        "{} (it disagrees with {}; ask for TLS one way)",
                        query, asked[0].0
                    ),
                });
            }
            Some(*first)
        }
        None => None,
    };
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
        (None, None) => Tls::Off,
    };
    if tls == Tls::VerifyCa && root.is_none() {
        // Without a CA of its own, any publicly issued certificate has a
        // chain to Mozilla's roots, so checking the chain alone stops no one.
        return Err(UrlError::InvalidParamValue {
            param: "ssl-mode".into(),
            value: "VERIFY_CA (it checks the chain against a CA you name: add ssl-ca=<file>, or use VERIFY_IDENTITY)".into(),
        });
    }
    if tls == Tls::Off {
        if opts.enable_cleartext_plugin() {
            return Err(UrlError::InvalidParamValue {
                param: "enable_cleartext_plugin".into(),
                value: "true (it sends the password as it is, so it needs TLS)".into(),
            });
        }
        return Ok(OptsBuilder::from_opts(opts).ssl_opts(None).into());
    }
    if let Some(socket) = opts.socket() {
        return Err(UrlError::InvalidParamValue {
            param: "socket".into(),
            value: format!(
                "{socket} (the driver doesn't use TLS over a Unix socket; connect over TCP to use TLS)"
            ),
        });
    }
    let mut ssl = mysql_async::SslOpts::default()
        .with_danger_accept_invalid_certs(tls == Tls::Encrypt)
        .with_danger_skip_domain_validation(tls != Tls::VerifyIdentity);
    if let Some(root) = root {
        // Only the CA named, as other MySQL clients do.
        ssl = ssl
            .with_root_certs(vec![root.into()])
            .with_disable_built_in_roots(true);
    }
    // The driver switches to a Unix socket on loopback when it can, and
    // doesn't use TLS there.
    Ok(OptsBuilder::from_opts(opts)
        .prefer_socket(false)
        .ssl_opts(ssl)
        .into())
}

/// Whether `host` is this machine, where no TLS loses nothing.
pub fn is_local(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
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
        assert_eq!(
            d("mysql://a@h/db?ssl-mode=PREFERRED"),
            "TLS, any certificate"
        );
        assert_eq!(
            d("mysql://a@h/db?ssl-mode=REQUIRED"),
            "TLS, any certificate"
        );
        assert_eq!(d("mysql://a@h/db?sslmode=require"), "TLS, any certificate");
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
            d("mysql://a@h/db?ssl={\"rejectUnauthorized\": false}"),
            "TLS, any certificate"
        );
        assert_eq!(d("mysql://a@h/db?ssl=off"), "no TLS");
        assert_eq!(
            d("mysql://a@h/db?require_ssl=true"),
            "TLS, certificate and host name checked"
        );
        assert_eq!(
            d("mysql://a@h/db?require_ssl=true&verify_ca=false"),
            "TLS, any certificate"
        );
        // Agreeing settings are fine.
        assert_eq!(
            d("mysql://a@h/db?ssl-mode=REQUIRED&sslaccept=accept_invalid_certs"),
            "TLS, any certificate"
        );
    }

    #[test]
    fn a_named_ca_is_the_only_one_trusted() {
        let o = opts("mysql://a@h/db?ssl-ca=%2Fetc%2Fca.pem&wait_timeout=60").unwrap();
        assert_eq!(describe(&o), "TLS, certificate and host name checked");
        let ssl = o.ssl_opts().unwrap();
        assert_eq!(ssl.root_certs().len(), 1);
        assert!(ssl.disable_built_in_roots());
        assert!(!o.prefer_socket());
        assert_eq!(o.wait_timeout(), Some(60));
        let o = opts("mysql://a@h/db?ssl-mode=VERIFY_CA&ssl-ca=/etc/ca.pem").unwrap();
        assert_eq!(describe(&o), "TLS, certificate chain checked");
        // Mozilla's roots otherwise, which only mean something with the host name.
        let o = opts("mysql://a@h/db?ssl-mode=VERIFY_IDENTITY").unwrap();
        assert!(!o.ssl_opts().unwrap().disable_built_in_roots());
    }

    #[test]
    fn what_rowbit_cant_do_is_refused() {
        let refused = |url: &str| opts(url).unwrap_err().to_string();
        assert!(refused("mysql://a@h/db?ssl-mode=SOMETIMES").contains("SOMETIMES"));
        assert!(refused("mysql://a@h/db?ssl-mode=VERIFY_CA").contains("ssl-ca"));
        assert!(
            refused("mysql://a@h/db?require_ssl=true&verify_identity=false").contains("ssl-ca")
        );
        assert!(refused("mysql://a@h/db?ssl-ca=ca.pem").contains("absolute path"));
        assert!(refused("mysql://a@h/db?sslaccept=strict&ssl-mode=DISABLED").contains("disagrees"));
        assert!(refused("mysql://a@h/db?ssl-mode=VERIFY_IDENTITY&ssl=false").contains("disagrees"));
        assert!(refused("mysql://a@h/db?ssl=maybe").contains("maybe"));
        assert!(refused("mysql://a@h/db?ssl-cert=/c.pem").contains("client certificates"));
        assert!(refused("mysql://a@h/db?enable_cleartext_plugin=true").contains("needs TLS"));
        assert!(
            refused("mysql://a@h/db?sslmode=require&socket=/tmp/m.sock").contains("Unix socket")
        );
        assert!(matches!(
            opts("mysql://a@h/db?charset=utf8"),
            Err(UrlError::UnknownParameter { .. })
        ));
    }
}
