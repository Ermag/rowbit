# Test TLS certificates

For the TLS-only MariaDB in `compose.yaml` and nothing else: a CA (`ca.pem`, whose key was thrown away) and a server certificate for `127.0.0.1` and `localhost` signed by it, valid until 2046. `server-key.pem` is public on purpose. Tests connect with `ssl-ca=<this directory>/ca.pem` to check that a properly verified connection works.
