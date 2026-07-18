-- Modify "scheduled_prompts" table
ALTER TABLE "public"."scheduled_prompts" DROP COLUMN "context_clear_threshold_pct", ADD COLUMN "context_clear_threshold_tokens" bigint NULL;
