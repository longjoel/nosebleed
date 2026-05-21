# Publishing nosebleed

This repo currently has two publishable surfaces:

1. Rust crate and CLI: `apps/nosebleed`
2. Browser TypeScript SDK: `packages/player-sdk`

The repo itself is intended to be public on GitHub under:

```text
https://github.com/longjoel/nosebleed
```

## Public-readiness checklist

Before the first public release:

- [ ] Run secret scan over tracked files.
- [ ] Confirm license choice.
- [ ] Confirm package names are available/owned:
  - [ ] crates.io: `nosebleed`
  - [ ] npm: `@nosebleed/player-sdk`
- [ ] Decide whether example arcade package should remain private/unpublished.
- [ ] Add GitHub Actions CI for Rust + TypeScript builds.
- [ ] Run publish dry-runs.
- [ ] Tag `v0.1.0` and create GitHub release.

## Rust crate dry-run

```bash
cargo package -p nosebleed --allow-dirty
cargo publish -p nosebleed --dry-run
```

Actual publish:

```bash
cargo publish -p nosebleed
```

## npm SDK dry-run

```bash
pnpm --filter @nosebleed/player-sdk build
pnpm --filter @nosebleed/player-sdk publish --dry-run --access public
```

Actual publish:

```bash
pnpm --filter @nosebleed/player-sdk publish --access public
```

## GitHub visibility

After public-readiness checks pass:

```bash
gh repo edit longjoel/nosebleed --visibility public
```

## GitHub release

```bash
git tag v0.1.0
git push origin v0.1.0
gh release create v0.1.0 --title "nosebleed v0.1.0" --generate-notes
```
