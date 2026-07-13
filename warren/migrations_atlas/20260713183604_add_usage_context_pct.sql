-- Modify "scheduled_prompt_runs" table
ALTER TABLE "public"."scheduled_prompt_runs" ADD COLUMN "usage_context_pct" integer NULL;
