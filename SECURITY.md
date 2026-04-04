# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| latest release | Yes |
| older releases | No |

Security fixes are applied to the latest release only.

## Reporting a Vulnerability

**Please do not open public GitHub issues for security vulnerabilities.**

If you discover a security vulnerability in Trading Gateway, please report it
responsibly by emailing **security@tektii.com**.

Include:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

## Response Timeline

- **Acknowledgement:** within 48 hours
- **Initial assessment:** within 1 week
- **Fix or mitigation:** depends on severity, but we aim for 30 days

## Safe Harbour

We will not pursue legal action against security researchers who discover and
report vulnerabilities in good faith, following this policy. We consider
security research conducted in accordance with this policy to be authorised.

## Scope

This policy covers the `tektii-gateway` repository. Vulnerabilities in
third-party broker APIs (Alpaca, Binance, Oanda, Saxo) should be reported
directly to those providers.

## Important Note

Trading Gateway supports optional API key authentication via the `GATEWAY_API_KEY`
environment variable (see the [Security section](README.md#security-and-authentication)
in the README). However, the gateway is designed to run behind a private network
boundary as a defence-in-depth measure. Exposing the gateway to the public internet
without network-level access controls is not a supported configuration, even with
API key auth enabled.
