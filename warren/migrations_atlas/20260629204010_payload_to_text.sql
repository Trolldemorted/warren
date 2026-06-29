-- Switch payload and response from jsonb to plain text. Payloads are
-- conceptually strings (the user types / sends a string; the API only wraps
-- it in JSON for the wire); jsonb added nothing we used. Existing rows lose
-- their original-string form — the user must run `DELETE FROM requests;`
-- before this migration is applied.
ALTER TABLE "requests" ALTER COLUMN "payload" TYPE text USING payload::text;
ALTER TABLE "requests" ALTER COLUMN "response" TYPE text USING response::text;