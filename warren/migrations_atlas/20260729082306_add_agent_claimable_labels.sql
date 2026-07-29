-- Modify "agents" table
ALTER TABLE "public"."agents" ADD COLUMN "claimable_labels" character varying[] NOT NULL DEFAULT '{}';
