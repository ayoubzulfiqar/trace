# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 2.6.x   | Yes       |
| < 2.6   | No        |

## Reporting a Vulnerability

We take security vulnerabilities seriously. Please report them responsibly.

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, report them via one of:

- **Email**: contact@ayoubzulfiqar.com
- **GitHub private vulnerability reporting**: Use the "Report a vulnerability" button on the [security advisories page](https://github.com/ayoubzulfiqar/trace/security/advisories/new)

Include the following in your report:

1. **Summary**: A brief description of the vulnerability
2. **Steps to reproduce**: Detailed steps to reproduce the issue
3. **Impact**: What could happen if the vulnerability is exploited
4. **Proposed fix** (optional): If you have a suggested fix, include it

### What to expect

- You will receive an acknowledgment within 48 hours
- We will investigate and confirm the vulnerability
- We will work on a fix and coordinate a release timeline
- We will credit you in the release notes (unless you prefer to remain anonymous)
- We will not pursue legal action against you for acting in good faith

### Out of scope

The following are not considered security vulnerabilities:

- Issues in dependencies (report to the upstream project)
- Vulnerabilities in code that is clearly marked as experimental or unstable
- Issues that require physical access to the machine running the software

## Security Considerations

trace runs with the privileges of the user who starts it. Do not run it with elevated permissions.

What it reads and writes:

- **Reads** source files inside the project root only. Every path argument from an MCP client is resolved and confined to the root; `..`, absolute paths and symlinks pointing outside are rejected.
- **Writes** `<root>/.trace/` (index cache and session history, kept out of git), ADR Markdown files in the project's ADR directory (only through `record_decision`), and `~/.trace/` (daemon sockets, locks, logs).
- **Edits agent configuration files** only when you run `trace setup`. Edits are atomic, and the previous file is kept as `<file>.trace-backup`.
- **Installs services** only when you run `trace service install` (a user-level systemd unit or launchd agent, never system-wide).

Daemon isolation:

- The per-project daemon listens on a Unix socket created with mode `0600` inside a directory with mode `0700`.
- When the socket path falls back to the runtime or temp directory, trace verifies the directory is owned by the current user and is not a symlink before using it, so other local users cannot connect to or impersonate your daemon.

trace does not:

- make network requests (only `install.sh` downloads the release, verifying its SHA-256 checksum);
- execute code from the repository or run shell commands on behalf of MCP clients;
- collect or transmit telemetry.
