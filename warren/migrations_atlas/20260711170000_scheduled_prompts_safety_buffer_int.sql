-- The original `scheduled_prompts` migration declared
-- `weekly_safety_buffer_pct` and `session_safety_buffer_pct` as
-- `real`, but the SeaORM entity models them as `i32` (whole
-- percent, range 0..=100 — the only validated range the API and UI
-- accept). At runtime `psql`-side `FLOAT4` cannot decode into `i32`,
-- so every scheduler tick that read the schedule table errored out
-- with `mismatched types`. Align the columns with the entity.

ALTER TABLE "public"."scheduled_prompts"
  ALTER COLUMN "weekly_safety_buffer_pct" TYPE integer USING weekly_safety_buffer_pct::integer;

ALTER TABLE "public"."scheduled_prompts"
  ALTER COLUMN "session_safety_buffer_pct" TYPE integer USING session_safety_buffer_pct::integer;