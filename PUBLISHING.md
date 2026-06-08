# Publishing ullun-api to crates.io

## Two Publishing Methods

### Method 1: Automated via GitHub Actions (Recommended)
Push a git tag, CI handles the rest.

### Method 2: Manual via cargo publish
Publish directly from your machine.

---

## Method 1: Automated Publishing (CI)

### One-Time Setup

#### 1. Get crates.io API Token
1. Visit https://crates.io/settings/tokens
2. Click "New Token"
3. Name: "GitHub Actions - ullun-api"
4. Scopes: `publish-update` (default)
5. **Copy the token** (starts with `cio_`)

#### 2. Add Token to GitHub Secrets
1. Go to https://github.com/yedoma-labs/ullun-api/settings/secrets/actions
2. Click **"New repository secret"**
3. **Name:** `CARGO_REGISTRY_TOKEN`
4. **Value:** Paste your crates.io token
5. Click **"Add secret"**

✅ **Done!** CI will auto-publish on version tags.

### Publishing a Release

```bash
# 1. Update version in Cargo.toml
vim Cargo.toml  # Change version = "0.1.0" to "0.1.1"

# 2. Update CHANGELOG.md
vim CHANGELOG.md

# 3. Commit changes
git add Cargo.toml CHANGELOG.md
git commit -m "Bump version to 0.1.1"
git push origin main

# 4. Create and push tag (triggers CI publish)
git tag -a v0.1.1 -m "Release v0.1.1"
git push origin v0.1.1

# CI will:
# - Verify version matches tag
# - Run all tests
# - Publish to crates.io
# - Create GitHub release
```

**Monitor progress:**
- https://github.com/yedoma-labs/ullun-api/actions
- https://crates.io/crates/ullun-api

---

## Method 2: Manual Publishing

### Prerequisites

#### 1. Create crates.io Account
1. Visit https://crates.io
2. Click "Log in with GitHub"
3. Authorize the application

#### 2. Get API Token
1. Go to https://crates.io/settings/tokens
2. Click "New Token"
3. Name it (e.g., "ullun-api publish")
4. Copy the token

#### 3. Login via Cargo
```bash
cargo login
# Paste your API token when prompted
# Token saved to ~/.cargo/credentials.toml
```

## Pre-Release Checklist

- [x] CI passing on all platforms (Linux, macOS, Windows)
- [x] All tests passing (12/12)
- [x] Clippy clean (no warnings)
- [x] Formatted with rustfmt
- [x] README.md with examples
- [x] LICENSE files (MIT + Apache-2.0)
- [x] SECURITY.md policy
- [x] CHANGELOG.md updated
- [x] Cargo.toml metadata complete
- [ ] Version number set correctly
- [ ] Documentation review
- [ ] Package size check

## Publishing Steps

### 1. Verify Package Contents
```bash
# Dry-run to see what will be included
cargo package --list

# Build the package (creates .crate file in target/package/)
cargo package

# Check package size (should be < 10MB)
ls -lh target/package/*.crate
```

### 2. Test the Package
```bash
# Test the packaged version locally
cargo package --allow-dirty
cd target/package/ullun-api-0.1.0
cargo test --all-features
cd ../../..
```

### 3. Publish (Dry Run First)
```bash
# Dry-run (doesn't actually publish)
cargo publish --dry-run

# Review output for any warnings or errors
```

### 4. Publish for Real
```bash
# This is PERMANENT - cannot delete versions from crates.io
cargo publish

# Wait for confirmation
# Usually takes 1-2 minutes to index
```

### 5. Verify Publication
1. Visit https://crates.io/crates/ullun-api
2. Check docs at https://docs.rs/ullun-api
3. Test installation:
   ```bash
   cargo new test-ullun
   cd test-ullun
   cargo add ullun-api
   cargo build
   ```

## Post-Release

### 1. Tag the Release
```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

### 2. Create GitHub Release
1. Go to https://github.com/yedoma-labs/ullun-api/releases/new
2. Select tag `v0.1.0`
3. Title: `v0.1.0 - Initial Release`
4. Description: Copy from CHANGELOG.md
5. Publish release

### 3. Announce
- [ ] Twitter/X
- [ ] Reddit r/rust
- [ ] This Week in Rust
- [ ] Hacker News (if significant interest)

## Subsequent Releases

### Versioning (Semantic Versioning)
- `0.1.0` → `0.1.1` - Bug fixes, patch
- `0.1.0` → `0.2.0` - New features, backward compatible
- `0.1.0` → `1.0.0` - Breaking changes OR stable API

### Update Version
1. Edit `Cargo.toml` - bump `version`
2. Update `CHANGELOG.md`
3. Commit: `git commit -m "Bump version to X.Y.Z"`
4. Repeat publishing steps above

## Troubleshooting

### "crate name already exists"
- Name is taken, choose different name
- Check https://crates.io/crates/ullun-api

### "failed to verify package"
- Missing files or dependencies
- Run `cargo package` locally first
- Check `exclude` in Cargo.toml

### "package size too large"
- Max 10MB for crates.io
- Add more to `exclude` in Cargo.toml
- Remove unnecessary files

### "documentation failed to build"
- Check docs.rs build at https://docs.rs/crate/ullun-api/latest/builds
- Test locally: `cargo doc --no-deps --open`
- Fix any doc warnings

## Important Notes

⚠️ **Publishing is PERMANENT**
- Cannot delete published versions
- Can only yank (hide from search, but still downloadable)
- Be sure before publishing!

⚠️ **Crate Name Reservation**
- Once published, name is yours
- But unused crates may be reclaimed after 30 days of inactivity

✅ **What you CAN do:**
- Yank versions: `cargo yank --vers 0.1.0`
- Un-yank: `cargo yank --vers 0.1.0 --undo`
- Transfer ownership (in settings)

## Resources

- [Publishing Guide](https://doc.rust-lang.org/cargo/reference/publishing.html)
- [crates.io Policies](https://crates.io/policies)
- [Semantic Versioning](https://semver.org/)
- [Cargo Manifest](https://doc.rust-lang.org/cargo/reference/manifest.html)
