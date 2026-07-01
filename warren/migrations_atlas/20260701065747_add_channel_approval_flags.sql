-- Modify "channels" table
ALTER TABLE "public"."channels" ADD COLUMN "requires_request_approval" boolean NOT NULL DEFAULT true, ADD COLUMN "requires_response_approval" boolean NOT NULL DEFAULT true;
