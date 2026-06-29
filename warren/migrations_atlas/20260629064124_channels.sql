-- Create "channels" table
CREATE TABLE "public"."channels" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "sender_class" character varying NOT NULL,
  "sender_kind" character varying NULL,
  "receiver_class" character varying NOT NULL,
  "receiver_kind" character varying NULL,
  "description" character varying NOT NULL DEFAULT '',
  "created_at" timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY ("id")
);
-- Create index "channels_uniq_idx" to table: "channels"
CREATE UNIQUE INDEX "channels_uniq_idx" ON "public"."channels" ("sender_class", "sender_kind", "receiver_class", "receiver_kind");
-- Modify "requests" table
ALTER TABLE "public"."requests" ADD COLUMN "channel_id" uuid NULL, ADD CONSTRAINT "fk-requests-channel_id" FOREIGN KEY ("channel_id") REFERENCES "public"."channels" ("id") ON UPDATE NO ACTION ON DELETE SET NULL;
