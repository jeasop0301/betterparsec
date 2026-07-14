# Paired Sunshine TLS pinning prototype

This isolated crate proves the cryptographic operations required to replace BetterParsec's
current no-op Rustls server verifier.

It is intentionally not wired into the application. The production integration is represented by
`../../patches/moonlight-common-rust-tls-pinning.patch`, which applies after the existing
BetterParsec Moonlight patch.

The prototype verifies:

- exact paired-certificate DER equality;
- OpenSSL parsing of a Sunshine-style self-signed certificate with serial zero;
- TLS 1.2 RSA-PKCS1 `CertificateVerify` signatures;
- TLS 1.3 RSA-PSS `CertificateVerify` signatures;
- wrong transcript and tampered signature rejection;
- unsupported signature-scheme rejection.

On this Windows checkout, tests can reuse the MSVC OpenSSL SDK produced by the main project's
release build:

```powershell
$env:OPENSSL_DIR = 'C:\Users\kje12\Desktop\Projects\betterparsec\target\release\build\openssl-sys-93d71fd0bd328521\out\openssl-build\install'
cargo test --manifest-path ./security/tls-pinning-prototype/Cargo.toml
```

The path above is machine-specific evidence, not a portable project setting. Normal BetterParsec
builds should continue to use `build-windows.ps1`, which discovers an SDK or builds vendored OpenSSL
with Strawberry Perl and NASM.

Before landing the integration patch:

1. merge it into the repository-owned Moonlight patch/bootstrap sequence;
2. run the prototype tests;
3. run a clean pinned-clone `cargo check` with both BetterParsec patches;
4. test pairing and HTTPS requests against stock Sunshine and Foundation Sunshine;
5. confirm a substituted certificate and a tampered handshake are rejected;
6. rerun the full BetterParsec Rust/browser test suite and release build.
