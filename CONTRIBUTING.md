# Contributing to trace

Thanks for your interest in contributing to `trace`! This document covers the development setup, the project's conventions, and how to submit changes.

## Development Environment

- **Rust 1.90+** (edition 2021; the MSRV is checked in CI)
- A C compiler (tree-sitter grammars and the bundled SQLite are compiled from C)

```bash
git clone https://github.com/ayoubzulfiqar/trace.git
cd trace
cargo build
```

## Checks

All of these must pass before submitting a change (CI runs them on Linux, macOS and Windows):

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests live in a `#[cfg(test)]` module at the bottom of each file and use `tempfile::TempDir`, so they are isolated and run in parallel. Never read or write the real `$HOME` in tests; build fixtures in a temp directory instead. For `trace setup`, point `HOME`/`XDG_CONFIG_HOME` at a temp directory when trying it by hand.

## Code Style

- Run `cargo fmt`; keep `cargo clippy` warning-free.
- Doc comments on public items; comments explain *why*, not *what*.
- Errors crossing the MCP boundary are `String` messages written for the model to act on ("missing required argument 'path'"). CLI code uses `anyhow`.
- Never panic on user input: tool handlers run inside `catch_unwind`, but a panic is still a bug.
- Every user-supplied path goes through `root::resolve_in_root`.
- Anything reading source must stay iterative or depth-bounded: extraction runs on daemon connection threads with default stack sizes.

## Project Structure

| Module | Responsibility |
|--------|---------------|
| `tree_sitter_detector.rs` | Language registry: extensions ↔ tree-sitter grammars (single source of truth) |
| `structural.rs` | Single-pass extraction (symbols, imports, calls, routes), graph queries, import resolution |
| `scan.rs` | Parallel gitignore-aware walk, fingerprints, content hashes, index deltas |
| `invariant.rs` | Rules loading/validation and plan evaluation |
| `adr.rs` | ADR parsing, ranked search, recording and superseding |
| `store.rs` | SQLite persistence and schema migrations |
| `mcp.rs` | JSON-RPC/MCP protocol, tool registry and handlers |
| `daemon.rs` | stdio server, per-project daemon, reconnecting shim |
| `root.rs` | Project root discovery and path confinement |
| `setup.rs` | Agent discovery and config editing |
| `service.rs` | systemd / launchd integration |
| `model.rs`, `humanize.rs` | Shared records and helpers |
| `main.rs` | CLI |

## Adding a New MCP Tool

1. Write a handler `fn tool_x(s: &Server, session: &Session, a: &Args) -> ToolResult` in `src/mcp.rs`, using the `Args` helpers for validation.
2. Add a `schema_x()` returning the JSON Schema for its arguments.
3. Register a `ToolSpec` in `TOOLS` with a precise description and correct `read_only`/`idempotent` hints.
4. Add a test driving it through `handle_message` (see the `mcp::tests` helpers).
5. Document it in the README tool table.

## Adding a New Language

1. Add the grammar crate to `Cargo.toml` and a `Lang` variant with its extensions and grammar in `tree_sitter_detector.rs`.
2. Add a `visit_<lang>` method to the extractor in `structural.rs`. Visitors do their per-node work immediately and *schedule* children and scope/caller pops on the `Queue`; they never recurse.
3. Add import resolution to `Resolver` and, if relevant, route conventions to `extract_routes`.
4. Bump `STRUCTURAL_EXTRACTOR_VERSION` so cached facts are re-extracted.
5. Add tests for symbols, imports, calls and routes.

Use a scratch program that prints `node.kind()` and field names to learn a grammar's node types; don't guess them.

## Adding a New Rule Type

1. Add the field to `ArchitecturalRule` in `invariant.rs` (it uses `deny_unknown_fields`, so older binaries reject configs they don't understand).
2. Compile it in `CompiledRule::compile` and evaluate it in `CompiledRule::check`.
3. Add a `ViolationKind` if needed, tests, and the README rules table entry.

## Changing Persistent Formats

- Changing the SQLite schema: add a `SCHEMA_V<n>` migration in `store.rs` and bump `SCHEMA_VERSION`.
- Changing what the extractor produces: bump `STRUCTURAL_EXTRACTOR_VERSION`.
- Changing the daemon wire behaviour: the socket name embeds the crate version, so release a new version.

## Packaging

Release binaries and Linux packages are built by GitHub Actions (`.github/workflows/release.yml`); nothing needs to be built locally. The workflow runs `packaging/build-package.sh` inside each target distribution's container, so every package links against that distribution's libraries:

| Package | Built in | Tool | Configured by |
|---|---|---|---|
| Debian `.deb` (amd64, arm64) | `debian:bookworm` | `cargo deb` | `[package.metadata.deb]` in `Cargo.toml` |
| Fedora `.rpm` (x86_64, aarch64) | `fedora:latest` | `cargo generate-rpm` | `[package.metadata.generate-rpm]` in `Cargo.toml` |
| Arch `.pkg.tar.zst` (x86_64) | `archlinux:base-devel` | `makepkg` | `packaging/arch/PKGBUILD` |

Every package ships the binary, the man page (`trace man`), bash/zsh/fish completions (`trace completions`), the README, the usage guide and the changelog. When adding a file to the packages, update all three definitions.

The script can also be run by hand from the source root on a machine (or container) of the target distribution: `packaging/build-package.sh deb|rpm|arch`. As root it installs its own build dependencies, and packages land in `dist/`.

## Release Process

1. Bump `version` in `Cargo.toml` and run `cargo build` so `Cargo.lock` follows.
2. Set `pkgver` in `packaging/arch/PKGBUILD` (and reset `pkgrel=1`).
3. Add a section to `CHANGELOG.md` and update version examples in `README.md` and `docs/USAGE.md`.
4. Commit, then tag and push: `git tag v2.6.9 && git push origin v2.6.9`.

The release workflow then:

1. runs the tests;
2. builds the portable archives (Linux, macOS, Windows);
3. builds the Debian (amd64, arm64), Fedora (x86_64, aarch64) and Arch (x86_64) packages inside their distributions, installing each one as a smoke test;
4. publishes everything with `.sha256` files and `SHA256SUMS`.

A failed distribution package doesn't block the portable binaries. To rebuild an existing tag, run the workflow manually with its `tag` input.

## Submitting Changes

1. Fork the repository and create a feature branch (`git checkout -b feature/my-feature`).
2. Write code and tests; make sure all checks pass.
3. Commit with a descriptive message and open a pull request explaining what changed and why.

## Pull Request Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
- [ ] New behaviour is covered by tests
- [ ] README / inline docs are updated

## Reporting Issues

- **Bug reports**: open a [bug report](https://github.com/ayoubzulfiqar/trace/issues/new?template=bug_report.md)
- **Feature requests**: open a [feature request](https://github.com/ayoubzulfiqar/trace/issues/new?template=feature_request.md)
- **Security vulnerabilities**: email contact@ayoubzulfiqar.com (see [SECURITY.md](SECURITY.md))

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
