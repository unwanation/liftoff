# Security

## Reporting

Report vulnerabilities privately to the maintainers (see `authors` in
`Cargo.toml`). Do not open public issues for security problems. Expect an
acknowledgement within 72 hours.

## Boundaries this template assumes

The binary is designed to run **behind an ingress** (reverse proxy, service
mesh or API gateway) that terminates TLS, authenticates callers and enforces
rate limits. Inside that boundary the service still defends itself:

| Risk                           | Control                                                        |
|--------------------------------|----------------------------------------------------------------|
| Oversized / slow requests      | REST body limit + timeout |
| Malformed input                | Parsed into validated types (`ProjectName`, `Host`, ...) before reaching the domain; `deny_unknown_fields` on every payload and config |
| Information leakage            | Problem/Status messages carry only reviewed text; internal causes go to logs with the request id |
| Log injection                  | Control characters rejected at the boundary; logs are structured JSON |
| Credential leakage in traces   | `Authorization`, `Cookie`, `Proxy-Authorization` marked sensitive |
| Browser-context abuse          | `nosniff`, `default-src 'none'`, `no-referrer`, `no-store`; no CORS unless you add it |
| Handler panics                 | Contained per request (`CatchPanicLayer`) |
| Integer overflow in release    | `overflow-checks = true`; `arithmetic_side_effects` lint on by default |
| Memory safety                  | `unsafe_code = "forbid"` in every crate                        |
| Vulnerable dependencies        | `cargo deny` (advisories) in hooks/CI, daily `rustsec/audit-check`, Dependabot |
| License / provenance           | `cargo deny` license allow-list; registry allow-list; git sources denied |
| Supply-chain of the toolchain  | Toolchain and every dev tool version pinned (`rust-toolchain.toml`, `mise.toml`); Actions pinned by SHA; base images by digest |
| Secrets in git                 | gitleaks pre-commit and in CI                                  |
| Deployed artefact audit        | `cargo auditable` embeds the dependency list; CycloneDX SBOM attached to releases |
| Container                      | Distroless static image, non-root, no shell                    |

## What you must add for your context

- Authentication/authorization (middleware or at the ingress).
- Rate limiting and connection limits at the edge (`tower_governor` if in-process).
- TLS (`axum-server` + rustls) if there is no ingress.
- Audit logging for privileged operations.
