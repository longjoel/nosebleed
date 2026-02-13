# nosebleed monorepo

Mixed Rust + Node/TypeScript repository for the arcade platform.

## Layout

- `apps/nosebleed`: Rust runtime service (binary program).
- `packages/player-sdk`: Browser TypeScript SDK.
- `docs`: Product and architecture docs for the virtual arcade.
- `scripts`: Repo-level helper scripts.

## Command Surface (pnpm)

Use root `package.json` scripts as the single entrypoint for build/launch.

Install dependencies:

```bash
pnpm install
```

Launch runtime in dev mode:

```bash
pnpm launch
# alias: pnpm dev
# alias: pnpm start
```

Override port/address:

```bash
LISTEN_ADDR=127.0.0.1:8092 pnpm launch
```

Launch runtime with example config:

```bash
pnpm launch:config
```

Override config path:

```bash
NOSEBLEED_CONFIG=/path/to/match.config.json pnpm launch:config
```

Build website packages + application artifact:

```bash
pnpm build
```

Build only the release artifact:

```bash
pnpm build:app
```

Run built artifact directly:

```bash
pnpm launch:artifact
# alias: pnpm start:artifact
```

Create deploy bundle (binary + config + static assets + runtime launcher):

```bash
pnpm deploy
```

Override output directory:

```bash
DEPLOY_DIR=./dist/prod pnpm deploy
# or
pnpm deploy -- ./dist/prod
```

Run smoke verification:

```bash
pnpm smoke
```

## Docs

- Runtime guide: `apps/nosebleed/README.md`
- Public service API: `apps/nosebleed/docs/public-service.md`
- Express wrapper pattern: `apps/nosebleed/docs/express-wrapper.md`
- Virtual arcade blueprint: `docs/virtual-arcade-blueprint.md`
- Virtual arcade schematic: `docs/virtual-arcade-schematic.md`
