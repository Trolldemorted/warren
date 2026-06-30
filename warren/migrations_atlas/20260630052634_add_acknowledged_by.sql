-- Modify "requests" table
ALTER TABLE "public"."requests" ADD COLUMN "acknowledged_by" uuid NULL, ADD CONSTRAINT "fk-requests-acknowledged_by" FOREIGN KEY ("acknowledged_by") REFERENCES "public"."agents" ("id") ON UPDATE NO ACTION ON DELETE NO ACTION;
