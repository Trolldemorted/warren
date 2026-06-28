-- Create "admin_sessions" table
CREATE TABLE "public"."admin_sessions" (
  "token" text NOT NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  "expires_at" timestamptz NOT NULL,
  PRIMARY KEY ("token")
);
-- Create index "admin_sessions_expires_idx" to table: "admin_sessions"
CREATE INDEX "admin_sessions_expires_idx" ON "public"."admin_sessions" ("expires_at");
-- Create "agents" table
CREATE TABLE "public"."agents" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "name" text NOT NULL,
  "class" text NOT NULL,
  "type" text NULL,
  "model" text NOT NULL,
  "prompt" text NOT NULL DEFAULT '',
  "authtoken" text NOT NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY ("id")
);
-- Create index "agents_authtoken_key" to table: "agents"
CREATE UNIQUE INDEX "agents_authtoken_key" ON "public"."agents" ("authtoken");
-- Create index "agents_class_type_idx" to table: "agents"
CREATE INDEX "agents_class_type_idx" ON "public"."agents" ("class", "type");
-- Create index "agents_name_key" to table: "agents"
CREATE UNIQUE INDEX "agents_name_key" ON "public"."agents" ("name");
-- Create "requests" table
CREATE TABLE "public"."requests" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "target_class" text NOT NULL,
  "target_type" text NULL,
  "payload" jsonb NOT NULL,
  "response" jsonb NULL,
  "status" smallint NOT NULL DEFAULT 0,
  "claimed_by" uuid NULL,
  "claimed_at" timestamptz NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  "responded_at" timestamptz NULL,
  PRIMARY KEY ("id"),
  CONSTRAINT "requests_claimed_by_fkey" FOREIGN KEY ("claimed_by") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION
);
-- Create index "requests_inbox_idx" to table: "requests"
CREATE INDEX "requests_inbox_idx" ON "public"."requests" ("target_class", "target_type", "status") WHERE ((status = 1) AND (claimed_by IS NULL));
-- Create index "requests_status_idx" to table: "requests"
CREATE INDEX "requests_status_idx" ON "public"."requests" ("status", "created_at");
