-- Modify "scheduled_prompts" table
ALTER TABLE "public"."scheduled_prompts" ADD COLUMN "additional_labels" character varying[] NOT NULL DEFAULT '{}';
