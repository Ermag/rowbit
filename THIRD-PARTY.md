# Third-party notices

Rowbit's binaries include Mozilla's list of trusted root certificates, through the `webpki-roots` crate, to check the database server's certificate. That list is made available under the [Community Data License Agreement – Permissive, Version 2.0](https://cdla.dev/permissive-2-0/).

Rowbit's other dependencies are under the MIT, Apache-2.0, BSD, ISC, Zlib and Unicode licenses; `cargo deny list` shows which crate uses which.
