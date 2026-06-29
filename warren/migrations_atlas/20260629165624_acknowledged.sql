-- Modify "requests" table to track when an agent acks the response
ALTER TABLE "public"."requests" ADD COLUMN "acknowledged_at" timestamptz NULL;
