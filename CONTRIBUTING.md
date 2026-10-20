# Contributing to ullun-api

Thank you for your interest in contributing! This document provides guidelines and instructions.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/ullun-api.git`
3. Create a branch: `git checkout -b feature/your-feature-name`
4. Make your changes
5. Run tests: `cargo test`
6. Run examples: `cargo run --example hello_world`
7. Commit with a [Conventional Commit](#commit-messages) message: `git commit -am 'feat(router): add OPTIONS routes'`
8. Push: `git push origin feature/your-feature-name`
9. Create a Pull Request

## Development Setup

### Prerequisites

- Latest stable Rust (CI always builds with `stable`)
- Cargo

### Building

```bash
cargo build
```

### Running Tests

```bash
# Run all tests (default features, then everything)
cargo test
cargo test --all-features

# Run specific test
cargo test test_name

# Run with output
cargo test -- --nocapture
```

### Running Examples

```bash
cargo run --example hello_world
cargo run --example full_api
```

See [`examples/`](examples/) for scenario examples (microservice, public API, SPA backend, webhook receiver, HTML forms). Examples read `PORT` from the environment where it matters.

Test the examples:

```bash
# In another terminal
curl http://localhost:3000/
curl http://localhost:3000/hello/world
```

## Code Style

- Follow Rust standard style (`cargo fmt`)
- Run clippy: `cargo clippy --all-targets --all-features -- -D warnings` (CI also runs it with default features)
- Document public APIs with `///` comments
- Keep functions focused and concise
- Write tests for new features

## Commit Messages

All commits use [Conventional Commits](https://www.conventionalcommits.org/). Releases are derived from them, so the type matters:

| Type | Effect on release |
|------|-------------------|
| `fix:` | patch |
| `feat:` | minor |
| `feat!:` / `BREAKING CHANGE:` footer | major (minor while `0.x`) |
| `docs:` `refactor:` `perf:` `test:` `build:` `ci:` `chore:` `style:` | no release on their own, appear in the changelog where relevant |

Enable the local check once per clone:

```bash
git config core.hooksPath .githooks
```

CI lints the PR title and every commit of a PR. Do not add `Co-authored-by` trailers for tools.

## Testing Guidelines

- Add unit tests for new functionality
- Add integration tests for API endpoints
- Ensure all tests pass before submitting PR
- Include both success and error cases

Example test:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_something() {
        // Arrange
        let input = ...;
        
        // Act
        let result = function_under_test(input);
        
        // Assert
        assert_eq!(result, expected);
    }
}
```

## Pull Request Process

1. Update README.md if adding new features
2. Make sure `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass
3. Use a Conventional Commit PR title (it becomes the squash commit on `main`)
4. Update documentation if needed
5. Request review from maintainers

Do not edit `CHANGELOG.md` or the version in `Cargo.toml` by hand; see below.

## Releases

Fully automated by [release-plz](https://release-plz.dev) in the `release` job of `.github/workflows/ci.yml`:

1. A merge to `main` runs format, clippy, rustdoc, audit, tests (Linux/macOS/Windows) and the release build.
2. Only if all of them are green does the `release` job run. It never runs on pull requests.
3. From the Conventional Commits since the last tag, it opens or updates a **release PR** (version bump + `CHANGELOG.md`).
4. Merging the release PR tags `vX.Y.Z`, creates the GitHub release and publishes to crates.io.

Required repository secrets: `CARGO_REGISTRY_TOKEN` (crates.io). Optional `RELEASE_TOKEN` (a PAT or GitHub App token): without it the release PR is created with `GITHUB_TOKEN`, and GitHub does not start CI on such PRs, so lint/test must be triggered by pushing to the PR branch or closing/reopening it. Settings → Actions → "Allow GitHub Actions to create and approve pull requests" must be enabled.

## Feature Requests

Open an issue with:
- Clear description of the feature
- Use case / motivation
- Example API (if applicable)

## Bug Reports

Open an issue with:
- Clear description of the bug
- Steps to reproduce
- Expected vs actual behavior
- Rust version and OS
- Minimal code example (if possible)

## Code of Conduct

- Be respectful and inclusive
- Focus on constructive feedback
- Help others learn and grow
- Collaborate in good faith

## Questions?

- Open a GitHub issue
- Discussion board (coming soon)

## License

By contributing, you agree that your contributions will be licensed under MIT or Apache-2.0 (dual license).
