-- Create "agent_events" table
CREATE TABLE "public"."agent_events" ("id" uuid NOT NULL, "agent_id" uuid NOT NULL, "seq" bigint NOT NULL, "ts" timestamptz NOT NULL DEFAULT now(), "kind" character varying NOT NULL, "payload" json NOT NULL, PRIMARY KEY ("id"), CONSTRAINT "fk-agent_events-agent_id" FOREIGN KEY ("agent_id") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION);
-- Create index "agent_events_agent_seq_idx" to table: "agent_events"
CREATE UNIQUE INDEX "agent_events_agent_seq_idx" ON "public"."agent_events" ("agent_id", "seq");
-- Create index "agent_events_agent_ts_idx" to table: "agent_events"
CREATE INDEX "agent_events_agent_ts_idx" ON "public"."agent_events" ("agent_id", "ts" DESC);
