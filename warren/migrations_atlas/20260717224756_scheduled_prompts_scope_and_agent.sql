-- Modify "scheduled_prompts" table
ALTER TABLE "public"."scheduled_prompts" ALTER COLUMN "target_class" DROP NOT NULL, ADD COLUMN "scope" character varying NOT NULL DEFAULT 'team', ADD COLUMN "agent_id" uuid NULL, ADD COLUMN "ignore_pending_forgejo_work" boolean NOT NULL DEFAULT false, ADD CONSTRAINT "fk-scheduled_prompts-agent_id" FOREIGN KEY ("agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION;
-- Create index "scheduled_prompts_agent_idx" to table: "scheduled_prompts"
CREATE INDEX "scheduled_prompts_agent_idx" ON "public"."scheduled_prompts" ("agent_id");
