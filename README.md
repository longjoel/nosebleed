# nosebleed monorepo

Mixed Rust + Node/TypeScript repository for the arcade platform.

## Layout

- `apps/nosebleed`: Rust runtime service (binary program).
- `packages/player-sdk`: Browser TypeScript SDK.
- `docs`: Product and architecture docs for the virtual arcade.
- `scripts`: Repo-level helper scripts.

## Rust workspace

Run the runtime from repo root:

```bash
cargo run -p nosebleed -- --listen 0.0.0.0:8080
```

Run with example config:

```bash
cargo run -p nosebleed -- --config apps/nosebleed/nosebleed.config.json.example
```

## Node workspace

Install and build all JS/TS packages:

```bash
pnpm install
pnpm build
```

Build only the player SDK:

```bash
pnpm --filter @nosebleed/player-sdk build
```

## Docs

- Runtime guide: `apps/nosebleed/README.md`
- Public service API: `apps/nosebleed/docs/public-service.md`
- Express wrapper pattern: `apps/nosebleed/docs/express-wrapper.md`
- Virtual arcade blueprint: `docs/virtual-arcade-blueprint.md`
- Virtual arcade schematic: `docs/virtual-arcade-schematic.md`
