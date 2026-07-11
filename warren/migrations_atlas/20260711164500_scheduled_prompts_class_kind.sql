-- Replace `agent_id` on `scheduled_prompts` with a (target_class,
-- target_kind) pair so a single schedule can target any connected idle
-- agent matching that pair — pools of homogeneous workers all serve
-- the same schedule. Also make `scheduled_prompt_runs.agent_id`
-- nullable since skips happen before an agent is chosen.

ALTER TABLE "public"."scheduled_prompts"
  DROP CONSTRAINT "fk-scheduled_prompts-agent_id";

ALTER TABLE "public"."scheduled_prompts"
  DROP COLUMN "agent_id";

ALTER TABLE "public"."scheduled_prompts"
  ADD COLUMN "target_class" text NOT NULL DEFAULT '';

ALTER TABLE "public"."scheduled_prompts"
  ADD COLUMN "target_kind" text NULL;

ALTER TABLE "public"."scheduled_prompts"
  ALTER COLUMN "target_class" DROP DEFAULT;

CREATE INDEX "scheduled_prompts_target_idx"
  ON "public"."scheduled_prompts" ("target_class", "target_kind");

ALTER TABLE "public"."scheduled_prompt_runs"
  ALTER COLUMN "agent_id" DROP NOT NULL;