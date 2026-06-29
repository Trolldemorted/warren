-- Create "admin_sessions" table
CREATE TABLE "public"."admin_sessions" (
  "token" character varying NOT NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  "expires_at" timestamptz NOT NULL,
  PRIMARY KEY ("token")
);
-- Create index "admin_sessions_expires_idx" to table: "admin_sessions"
CREATE INDEX "admin_sessions_expires_idx" ON "public"."admin_sessions" ("expires_at");
-- Create "agents" table
CREATE TABLE "public"."agents" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "name" character varying NOT NULL,
  "class" character varying NOT NULL,
  "kind" character varying NULL,
  "model" character varying NOT NULL,
  "prompt" character varying NOT NULL DEFAULT '',
  "authtoken" character varying NOT NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY ("id"),
  CONSTRAINT "agents_authtoken_key" UNIQUE ("authtoken"),
  CONSTRAINT "agents_name_key" UNIQUE ("name")
);
-- Create index "agents_class_kind_idx" to table: "agents"
CREATE INDEX "agents_class_kind_idx" ON "public"."agents" ("class", "kind");
-- Create "requests" table
CREATE TABLE "public"."requests" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "target_class" character varying NOT NULL,
  "target_type" character varying NULL,
  "payload" jsonb NOT NULL,
  "response" jsonb NULL,
  "status" smallint NOT NULL DEFAULT 0,
  "sender_agent_id" uuid NULL,
  "claimed_by" uuid NULL,
  "claimed_at" timestamptz NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  "responded_at" timestamptz NULL,
  PRIMARY KEY ("id"),
  CONSTRAINT "fk-requests-claimed_by" FOREIGN KEY ("claimed_by") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION,
  CONSTRAINT "fk-requests-sender_agent_id" FOREIGN KEY ("sender_agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION
);
-- Create index "requests_inbox_idx" to table: "requests"
CREATE INDEX "requests_inbox_idx" ON "public"."requests" ("target_class", "target_type") WHERE ((status = 1) AND (claimed_by IS NULL));
-- Create index "requests_sender_idx" to table: "requests"
CREATE INDEX "requests_sender_idx" ON "public"."requests" ("sender_agent_id", "created_at" DESC);
-- Create index "requests_status_idx" to table: "requests"
CREATE INDEX "requests_status_idx" ON "public"."requests" ("status", "created_at" DESC);
