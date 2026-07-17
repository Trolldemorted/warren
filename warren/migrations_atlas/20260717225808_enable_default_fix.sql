-- Modify "scheduled_prompts" table
ALTER TABLE "public"."scheduled_prompts" ALTER COLUMN "enabled" SET DEFAULT true, ALTER COLUMN "ignore_inbox_state" SET DEFAULT false, ALTER COLUMN "weekly_safety_buffer_pct" SET DEFAULT 0, ALTER COLUMN "session_safety_buffer_pct" SET DEFAULT 0;
