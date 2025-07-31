# Contributing to Onwards

## Release Process

This project uses automated releases through [release-plz](https://release-plz.dev/). Here's how it works:

### How Releases Work

1. **Make changes** using [conventional commits](https://www.conventionalcommits.org/):
   - `feat:` for new features (minor version bump)
   - `fix:` for bug fixes (patch version bump)
   - `feat!:` or `fix!:` for breaking changes (major version bump)

2. **Create a pull request** with your changes

3. **Merge the PR** - This triggers the release-plz workflow

4. **Release PR appears** - release-plz automatically creates a PR with:
   - Updated version in `Cargo.toml`
   - Generated changelog
   - All changes since last release

5. **Review and merge** the release PR

6. **Automated publishing** - When the release PR is merged:
   - release-plz publishes the crate to crates.io
   - Creates a GitHub release with changelog
   - Triggers Docker image build and push

### Conventional Commit Examples

```bash
feat: add new proxy authentication method
fix: resolve connection timeout issues
docs: update API documentation
chore: update dependencies
feat!: change configuration file format (BREAKING CHANGE)
```

The release workflow will automatically handle version bumping and publishing based on your commit messages.