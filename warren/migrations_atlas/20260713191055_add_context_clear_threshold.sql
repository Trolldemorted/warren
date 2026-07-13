-- Modify "scheduled_prompts" table
ALTER TABLE "public"."scheduled_prompts" ADD COLUMN "context_clear_threshold_pct" integer NULL;
