# Contributing to trace

Thanks for your interest in contributing to `trace`! This document covers how to set up a development environment, the project's conventions, and how to submit changes.

## Development Environment

### Prerequisites

- **Rust 1.75+** (uses edition 2021)
- **Cargo** (comes with Rust)

### Setup

```bash
git clone git@github.com:ayoubzulfiqar/trace.git
cd trace
cargo build
```

## Testing

All tests must pass before submitting a change:

```bash
cargo test -- --test-threads=1
```

Each feature module has unit tests in a `#[cfg(test)]` block at the bottom of the file. Tests use process-unique temporary directories so they can run in parallel, but serial execution is recommended for full isolation.

## Code Style

- Run `cargo fmt` before committing — the project uses `rustfmt`
- Run `cargo clippy` and ensure no warnings (the project builds with zero warnings)
- Use `context.Context` patterns for cancellation where applicable
- Error types live in `src/error.rs`; prefer structured error types over stringly-typed errors
- All public functions should have doc comments

## Project Structure

The codebase is organized into modules under `src/`:

| Module | Responsibility |
|--------|---------------|
| `structural.rs` | AST symbol/import extraction via Syn and tree-sitter visitors |
| `scan.rs` | Incremental file scanning and change detection |
| `tree_sitter_detector.rs` | Language detection by file extension |
| `invariant.rs` | Constraint engine: glob matching, plan evaluation |
| `adr.rs` | Architecture Decision Records (parse, search, record) |
| `store.rs` | SQLite-backed execution memory for sessions and touched files |
| `mcp.rs` | MCP server: JSON-RPC 2.0 dispatch and tool handlers |
| `error.rs` | Error types across all modules |
| `main.rs` | CLI entry point |

## Adding a New MCP Tool

1. Add the tool to the `TOOLS` array in `src/mcp.rs`
2. Add a handler case in the `handle_tool_call` function
3. Call the relevant business logic from the appropriate module (`structural.rs`, `invariant.rs`, `adr.rs`, or `store.rs`)
4. Add a unit test for the handler

## Adding a New Parser / Language

1. Add the language to `tree_sitter_detector.rs` (`language_for_path`)
2. Add the parser crate to `Cargo.toml` dependencies
3. Add extraction logic in `structural.rs` (`extract_file` match arm)
4. Add tests for the new language

## Adding a New Rule Type

1. Update the `Rule` struct in `invariant.rs` to add the new field
2. Update `check_file` to evaluate the new field
3. Update the JSON schema example in `README.md`
4. Add a test

## Submitting Changes

1. Fork the repository
2. Create a feature branch (`git checkout -b feature/my-feature`)
3. Write code and tests
4. Ensure `cargo fmt`, `cargo clippy`, and `cargo test -- --test-threads=1` all pass
5. Commit with a descriptive message
6. Open a pull request with a clear description of what was changed and why

## Pull Request Checklist

- [ ] Code builds with `cargo build`
- [ ] All tests pass with `cargo test -- --test-threads=1`
- [ ] `cargo fmt` produces no diffs
- [ ] `cargo clippy` produces no warnings
- [ ] New code is covered by tests
- [ ] Documentation is updated (README, inline docs)

## Reporting Issues

- **Bug reports**: Open a [bug report](https://github.com/ayoubzulfiqar/trace/issues/new?template=bug_report.md)
- **Feature requests**: Open a [feature request](https://github.com/ayoubzulfiqar/trace/issues/new?template=feature_request.md)
- **Security vulnerabilities**: Email contact@ayoubzulfiqar.com (see [SECURITY.md](SECURITY.md))

## License

By contributing, you agree that your contributions will be licensed under the MIT License.
