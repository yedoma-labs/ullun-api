# Quick Release Guide

## Setup (One-Time)

### Add crates.io Token to GitHub
1. Get token: https://crates.io/settings/tokens
   - Click "New Token"
   - Name: "GitHub Actions - ullun-api"
   - Copy the token (starts with `cio_`)

2. Add to GitHub: https://github.com/yedoma-labs/ullun-api/settings/secrets/actions
   - Click "New repository secret"
   - **Name:** `CARGO_REGISTRY_TOKEN`
   - **Value:** Your crates.io token
   - Save

✅ Done! CI will auto-publish on tags.

---

## Releasing a New Version

### 1. Update Version
```bash
# Edit Cargo.toml
vim Cargo.toml
# Change: version = "0.1.0" → version = "0.1.1"
```

### 2. Update Changelog
```bash
vim CHANGELOG.md
# Add new version section at top
```

### 3. Commit & Tag
```bash
git add Cargo.toml CHANGELOG.md
git commit -m "Bump version to 0.1.1"
git push origin main

# Create tag (must match Cargo.toml version!)
git tag -a v0.1.1 -m "Release v0.1.1"
git push origin v0.1.1
```

### 4. Monitor CI
- GitHub Actions: https://github.com/yedoma-labs/ullun-api/actions
- Workflow will:
  1. ✓ Verify tag matches Cargo.toml version
  2. ✓ Run all tests
  3. ✓ Publish to crates.io
  4. ✓ Create GitHub release

### 5. Verify
- crates.io: https://crates.io/crates/ullun-api
- docs.rs: https://docs.rs/ullun-api
- GitHub: https://github.com/yedoma-labs/ullun-api/releases

---

## Version Numbering (SemVer)

- `0.1.0` → `0.1.1` - Bug fixes, patches
- `0.1.0` → `0.2.0` - New features, backward compatible
- `0.1.0` → `1.0.0` - Breaking changes OR stable API

**Format:** `vMAJOR.MINOR.PATCH`

---

## Troubleshooting

### "Version mismatch" error
**Cause:** Tag doesn't match Cargo.toml version
```bash
# Delete local tag
git tag -d v0.1.1

# Delete remote tag (if already pushed)
git push --delete origin v0.1.1

# Fix version in Cargo.toml, recommit, retag
```

### "Authentication failed"
**Cause:** `CARGO_REGISTRY_TOKEN` secret not set or expired
- Regenerate token: https://crates.io/settings/tokens
- Update GitHub secret: https://github.com/yedoma-labs/ullun-api/settings/secrets/actions

### "Crate already exists"
**Cause:** Version already published
- Bump to next version (can't overwrite published versions)
- Or yank old version: `cargo yank --vers 0.1.1`

### CI tests failing
**Cause:** Code doesn't pass CI checks
- Fix issues locally: `cargo test && cargo clippy`
- Push fix, delete tag, retag

---

## Manual Publishing (Fallback)

If CI fails and you need to publish manually:

```bash
cargo login  # One-time, paste your crates.io token
cargo publish --dry-run  # Test first
cargo publish  # Actually publish
```

Then manually create GitHub release:
https://github.com/yedoma-labs/ullun-api/releases/new

---

## First Release Checklist

Before pushing `v0.1.0`:

- [ ] CI passing (all platforms)
- [ ] README.md complete
- [ ] CHANGELOG.md has v0.1.0 entry
- [ ] Examples work
- [ ] Documentation reviewed
- [ ] CARGO_REGISTRY_TOKEN secret added to GitHub
- [ ] Cargo.toml version = "0.1.0"

Then:
```bash
git tag -a v0.1.0 -m "Initial release"
git push origin v0.1.0
```

**Watch the magic happen!** 🚀
