# Warren

## Development

### Database Migrations

The sea-orm entities in `warren/src/entity/` are the source of truth. To change the schema:

1. Edit the entity in `warren/src/entity/`.
2. Run `warren dump-schema > /tmp/schema.sql` to emit DDL from the entities.
3. Run `atlas migrate diff --from file:///tmp/schema.sql --to "$DATABASE_URL"` to produce a new migration file under `warren/migrations_atlas/`.
4. Apply with `atlas migrate apply --url "$DATABASE_URL"`.

Never edit a committed migration file. Use idempotent SQL (`IF NOT EXISTS`, `ADD COLUMN ... DEFAULT`) so changes are safe against a populated database.
