-- Create "agent_forgejo_configs" table
CREATE TABLE "public"."agent_forgejo_configs" (
  "id" uuid NOT NULL DEFAULT gen_random_uuid(),
  "agent_id" uuid NOT NULL,
  "forgejo_username" character varying NOT NULL,
  "base_url" character varying NOT NULL,
  "owner" character varying NOT NULL,
  "repo" character varying NOT NULL,
  "access_token" character varying NOT NULL,
  "created_at" timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY ("id"),
  CONSTRAINT "fk-agent_forgejo_configs-agent_id" FOREIGN KEY ("agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION
);
-- Create index "agent_forgejo_configs_agent_idx" to table: "agent_forgejo_configs"
CREATE INDEX "agent_forgejo_configs_agent_idx" ON "public"."agent_forgejo_configs" ("agent_id");
