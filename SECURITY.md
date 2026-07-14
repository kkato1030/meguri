# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities in meguri privately through GitHub's
[Private Vulnerability Reporting](https://github.com/kkato1030/meguri/security/advisories/new)
(repo → **Security** tab → **Report a vulnerability**) instead of opening a
public issue.

This repository started private and is going public in stages; private
vulnerability reporting is enabled as part of that rollout (tracked in the
public-launch checklist). If the "Report a vulnerability" button isn't
available yet, email **kakato1030@gmail.com** instead — include enough detail
to reproduce (affected commit, steps, impact).

meguri is a single-maintainer, best-effort open-source project, so there's no
formal SLA, but security reports get priority over regular issues. Please
allow time to investigate and prepare a fix before any public disclosure.

## Scope

This policy covers the `meguri` orchestrator itself (this repository). It does
not cover the behavior of the third-party agent CLIs it launches (`claude`,
`codex`, ...) or of `git` / `gh` / `tmux` / `herdr` — please report those
upstream.

For meguri's threat model — what access the agent gets, and the mitigations
in place — see the [Security](README.md#security) section of the README.

## Supported versions

meguri doesn't publish tagged releases yet; only the latest commit on `main`
is supported. Please make sure an issue reproduces there before reporting it.
