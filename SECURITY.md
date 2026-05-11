# Security Notes

## Current `cargo audit` Findings

`cargo audit` currently reports vulnerabilities in `rustls-webpki` through Rocket's TLS stack, plus unmaintained transitive crates `rustls-pemfile` and `paste`.

These are acceptable for now because this application is currently a local development tool:

- The Rocket API is configured to bind to `127.0.0.1`, not a public or LAN-facing interface.
- Rocket TLS is used only to serve a local HTTPS endpoint with local development certificates.
- The application is not acting as a general TLS client that validates arbitrary remote certificates.
- The application does not use mutual TLS or client certificate validation.
- The `paste` warning comes through `netdev`/netlink dependencies and is an unmaintained-crate warning, not a known vulnerability.

## When This Stops Being Acceptable

Treat the `rustls-webpki` findings as release blockers if any of these become true:

- The API binds outside localhost.
- The app is deployed beyond local development.
- Rocket TLS is used as a production TLS terminator.
- Client certificate validation is added.
- CI or release policy requires a clean `cargo audit` result.

## Preferred Remediation

For production or shared environments, do not rely on Rocket's current TLS dependency stack. Prefer one of these approaches:

- Run Rocket over localhost HTTP and terminate TLS in a maintained external proxy such as Caddy or nginx.
- Remove Rocket's `tls` feature if HTTPS is not required inside the Rust process.
- Revisit the dependency once Rocket updates to a Rustls stack with fixed `rustls-webpki` versions.

If `paste` remains the only audit warning, either accept it with this documented rationale or replace `netdev` with a small Linux-specific default-route lookup.
