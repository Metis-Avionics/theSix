# Security Policy

## Scope

`thesix` is a **cache library**, not a network service. It has no listener, no
authentication server, and no privileged operation of its own. A finding here
almost always concerns the data plane's integrity guarantees — for example a value
served under the wrong key, a corrupt entry returned as valid, or a tenant able to
address another's key.

Two limits are worth stating before anything else, because they bound what a report
can meaningfully claim:

* **The content digest is not cryptographic.** `ContentDigest` is FNV-1a-64,
  unkeyed. It detects accidental corruption and truncation. It does **not**
  resist a determined adversary who can write to the store, and no claim of
  tamper-resistance is made anywhere in the crate. A report framed as "the digest
  can be forged" is a restatement of this, not a vulnerability.
* **Multi-tenant isolation is a caller responsibility at the edges.** The crate
  folds a tenant into the key with an explicit separator and rejects an oversized
  frame rather than truncating it. It cannot prevent a caller from
  mis-authenticating: `IdentityContext` is supplied by the caller, so a caller that
  presents the wrong tenant gets the wrong tenant's data, by design.

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.4.x   | :white_check_mark: |
| < 0.4   | :x:                |

Only `0.4.x` receives fixes. `0.4.0` is the current version in `Cargo.toml`; the
most recent version published to crates.io is `0.2.3`, so a published 0.2.x consumer
should treat the gap as unpatched rather than rely on the table above.

## Reporting a Vulnerability

Report privately through GitHub's **Security → Report a vulnerability** advisory on
this repository. Do not open a public issue.

Please include the crate version, the tier topology, the `IdentityContext` /
`CacheContext` construction, and a minimal reproducer. A reproducer that drives the
existing test harness is worth more than a description.

What to expect:

* **Acknowledgement** within three working days.
* **Triage** within ten working days: whether the report is accepted, and if not,
  why. Some reports will be a documented limitation rather than a defect — the two
  above are the usual examples, and both are stated in the crate's own docs so you
  should not have to file them.
* **Fix or withdraw** — an accepted report is fixed on `main` with a regression test
  named after the failure, not the feature. If a report cannot be reproduced, the
  withdrawal explains what was tried.

## Verification expectations

Every fix lands with the full gate matrix green (`cargo xtask gates`), and findings
are tracked in `bugs.toml` with a rationale. Branch `main` is intended to be
protected by a required `Verification (all gates)` check; that rule was disabled to
land the 0.4.0 series and is tracked as finding `B21`.
