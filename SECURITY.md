# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
| < 0.1   | No        |

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

This project runs as an MCP server (stdio JSON-RPC). It reads files from the project directory and writes to a local SQLite database. It does not:

- Make network requests
- Execute arbitrary code
- Collect or transmit telemetry

The server runs with the privileges of the user who started it. Do not run it with elevated permissions.
