# Contributing to ullun-api

Thank you for your interest in contributing! This document provides guidelines and instructions.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/ullun-api.git`
3. Create a branch: `git checkout -b feature/your-feature-name`
4. Make your changes
5. Run tests: `cargo test`
6. Run examples: `cargo run --example hello_world`
7. Commit: `git commit -am 'Add some feature'`
8. Push: `git push origin feature/your-feature-name`
9. Create a Pull Request

## Development Setup

### Prerequisites

- Rust 1.96 or later (latest stable recommended)
- Cargo

### Building

```bash
cargo build
```

### Running Tests

```bash
# Run all tests
cargo test

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

Test the examples:

```bash
# In another terminal
curl http://localhost:3000/
curl http://localhost:3000/hello/world
```

## Code Style

- Follow Rust standard style (`cargo fmt`)
- Run clippy: `cargo clippy -- -D warnings`
- Document public APIs with `///` comments
- Keep functions focused and concise
- Write tests for new features

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
2. Update CHANGELOG.md under `[Unreleased]`
3. Ensure all tests pass
4. Update documentation if needed
5. Request review from maintainers

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
