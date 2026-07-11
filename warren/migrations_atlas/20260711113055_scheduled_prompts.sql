-- Create "scheduled_prompts" table
CREATE TABLE "public"."scheduled_prompts" (
  "id" uuid NOT NULL,
  "agent_id" uuid NOT NULL,
  "name" text NOT NULL,
  "prompt_text" text NOT NULL,
  "interval_seconds" bigint NOT NULL,
  "enabled" boolean NOT NULL DEFAULT true,
  "ignore_inbox_state" boolean NOT NULL DEFAULT false,
  "weekly_safety_buffer_pct" real NOT NULL DEFAULT 0,
  "session_safety_buffer_pct" real NOT NULL DEFAULT 0,
  "last_fired_at" timestamptz NULL,
  "last_finished_at" timestamptz NULL,
  "next_fire_at" timestamptz NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  "updated_at" timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY ("id"),
  CONSTRAINT "fk-scheduled_prompts-agent_id" FOREIGN KEY ("agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE CASCADE
);
-- Create index "scheduled_prompts_next_fire_idx" to table: "scheduled_prompts"
CREATE INDEX "scheduled_prompts_next_fire_idx" ON "public"."scheduled_prompts" ("next_fire_at") WHERE enabled = true;
-- Create index "scheduled_prompts_agent_idx" to table: "scheduled_prompts"
CREATE INDEX "scheduled_prompts_agent_idx" ON "public"."scheduled_prompts" ("agent_id");

-- Create "scheduled_prompt_runs" table
CREATE TABLE "public"."scheduled_prompt_runs" (
  "id" uuid NOT NULL,
  "scheduled_prompt_id" uuid NOT NULL,
  "agent_id" uuid NOT NULL,
  "fired_at" timestamptz NOT NULL DEFAULT now(),
  "finished_at" timestamptz NULL,
  "outcome" text NOT NULL,
  "skip_reason" text NULL,
  "prompt_id" uuid NULL,
  "outcome_error" text NULL,
  "usage_weekly_pct" real NULL,
  "usage_session_pct" real NULL,
  PRIMARY KEY ("id"),
  CONSTRAINT "fk-scheduled_prompt_runs-scheduled_prompt_id" FOREIGN KEY ("scheduled_prompt_id") REFERENCES "public"."scheduled_prompts" ("id") ON UPDATE NO ACTION ON DELETE CASCADE,
  CONSTRAINT "fk-scheduled_prompt_runs-agent_id" FOREIGN KEY ("agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE CASCADE
);
-- Create index "scheduled_prompt_runs_prompt_idx" to table: "scheduled_prompt_runs"
CREATE INDEX "scheduled_prompt_runs_prompt_idx" ON "public"."scheduled_prompt_runs" ("scheduled_prompt_id", "fired_at" DESC);
-- Create index "scheduled_prompt_runs_agent_idx" to table: "scheduled_prompt_runs"
CREATE INDEX "scheduled_prompt_runs_agent_idx" ON "public"."scheduled_prompt_runs" ("agent_id", "fired_at" DESC);