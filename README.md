# Marshall

Marshall is an internal execution bot.

Documentation entry points:
- [docs/README.md](docs/README.md) — documentation map
- [docs/product-spec.md](docs/product-spec.md) — product behavior
- [docs/worker/technical-spec.md](docs/worker/technical-spec.md) — technical structure and invariants

## Configuration

- **Non-sensitive settings** are in `config.json` at the repo root (e.g. `sqlitePath`, poll interval, Linear filters, ETA model).
- **Secrets** are in `.env` or environment variables: `DISCORD_BOT_TOKEN`, `LINEAR_API_KEY`, `OPENAI_API_KEY`. Copy `.env.example` to `.env` and fill in only what you need.

## User Mapping

User mappings (Linear ↔ Discord) are stored in `user_map.csv` at the repo root. The file is committed and loaded automatically on app startup.

**To add or update a mapping:**
1. Edit `user_map.csv` (uncomment example rows or add new rows)
2. Restart the app (`npm run dev` or restart Docker)
3. Mappings are automatically imported and upserted into SQLite

**CSV format:**
```csv
linear_user_id,discord_user_id,discord_username,timezone
abc123,456789012345678901,username#1234,Europe/Belgrade
```

- `linear_user_id`: Required. Linear user ID (from Linear API/user object)
- `discord_user_id`: Required. Discord user ID (numeric, 17-19 digits)
- `discord_username`: Optional. Discord username for reference (e.g., "username#1234")
- `timezone`: Optional. IANA timezone (e.g., "Europe/Belgrade", "America/New_York")

**How to find Discord user_id:**
- Enable Developer Mode in Discord (Settings → Advanced → Developer Mode)
- Right-click a user → Copy User ID (or use `\@username` in a message and copy the ID)
- The ID is a numeric string (17-19 digits)

Lines starting with `#` and blank lines are ignored.

## Local Development

1. Install dependencies:
   ```bash
   npm install
   ```

2. Copy environment file (for secrets only):
   ```bash
   cp .env.example .env
   ```

3. Run migrations:
   ```bash
   npm run migrate
   ```

4. Start development server:
   ```bash
   npm run dev
   ```

## Tests

Run tests:
```bash
npm test
```

## Docker

Build and run with Docker Compose:
```bash
docker compose up --build
```

The app reads `config.json`; the default `sqlitePath` is `./data/marshall.sqlite`. The compose file mounts a volume at `/app/data`, so the database is persisted.

### SQLite Database Location

- **Local**: `./data/marshall.sqlite` (set in `config.json`)
- **Docker**: `/app/data/marshall.sqlite` (persisted via volume `marshall-data` mounted at `/app/data`)

## Verifying initial DMs (Step 4)

For the bot to send initial DMs you need:

- **DISCORD_BOT_TOKEN** — Bot token from Discord Developer Portal. In the Discord Developer Portal, enable **Message Content Intent** for the bot so it can read DM content.
- **LINEAR_API_KEY** — So the poller can fetch issues and assignees
- **user_map.csv** — At least one row mapping a Linear assignee `linear_user_id` to a Discord `discord_user_id` (that assignee must be on a matching REQ-1 issue)

After a poll cycle where a matching issue had a mapped assignee and no outstanding request:

- **requests**: `SELECT * FROM requests WHERE prompt_kind = 'initial';` — one row per initial DM attempt (`send_status` = `sent` or `failed`).
- **issue_engagements**: `SELECT * FROM issue_engagements WHERE state = 'waiting_reply';` — one row per issue we sent an initial DM for (until a reply is handled or a follow-up is scheduled).
