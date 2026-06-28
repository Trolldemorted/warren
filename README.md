# Warren

## Migrations

The sea-orm entities in `warren/src/entity/` are the source of truth. To change the schema:

1. Edit the entity.
2. `cargo run --bin warren -- dump-schema > /tmp/schema.sql`
3. `atlas migrate diff <name> --dev-url "$DATABASE_URL" --to file:///tmp/schema.sql --dir file://warren/migrations_atlas`
4. Apply with `atlas migrate apply --url "$DATABASE_URL" --dir file://warren/migrations_atlas` (or `warren applyMigration`, a thin wrapper).

Never edit a committed migration file.