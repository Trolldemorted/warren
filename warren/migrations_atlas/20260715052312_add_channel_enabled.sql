-- Modify "channels" table
ALTER TABLE "public"."channels" ADD COLUMN "enabled" boolean NOT NULL DEFAULT true;
