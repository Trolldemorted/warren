schema "public" {}

extension "pgcrypto" {
  url     = "https://github.com/ariga/atlas-orb/raw/master/sqlx/schema.sql"
  version = "1.3"
}

table "agents" {
  schema = schema.public
  column "id"         { type = uuid        null = false default = sql("gen_random_uuid()") }
  column "name"       { type = text        null = false }
  column "class"      { type = text        null = false }
  column "type"       { type = text        null = true }
  column "model"      { type = text        null = false }
  column "authtoken"  { type = text        null = false }
  column "created_at" { type = timestamptz null = false default = sql("now()") }
  primary_key { columns = [column.id] }
  index "agents_name_key"       { unique = true columns = [column.name] }
  index "agents_authtoken_key"  { unique = true columns = [column.authtoken] }
  index "agents_class_type_idx" { columns = [column.class, column.type] }
}

table "requests" {
  schema = schema.public
  column "id"           { type = uuid        null = false default = sql("gen_random_uuid()") }
  column "target_class" { type = text        null = false }
  column "target_type"  { type = text        null = true }
  column "payload"      { type = jsonb       null = false }
  column "response"     { type = jsonb       null = true }
  column "status" {
    type    = text
    null    = false
    default = "'pending'"
    check   { expr = "status IN ('pending','approved','rejected','responded')" }
  }
  column "claimed_by"   { type = uuid        null = true }
  column "claimed_at"   { type = timestamptz null = true }
  column "created_at"   { type = timestamptz null = false default = sql("now()") }
  column "responded_at" { type = timestamptz null = true }
  primary_key { columns = [column.id] }
  foreign_key "requests_claimed_by_fkey" {
    columns     = [column.claimed_by]
    ref_columns = [table.agents.column.id]
  }
  index "requests_inbox_idx" {
    columns = [column.target_class, column.target_type, column.status]
    where   = "status = 'approved' AND claimed_by IS NULL"
  }
  index "requests_status_idx" { columns = [column.status, sql("created_at DESC")] }
}

table "memos" {
  schema = schema.public
  column "id"           { type = uuid        null = false default = sql("gen_random_uuid()") }
  column "target_class" { type = text        null = false }
  column "target_type"  { type = text        null = true }
  column "payload"      { type = jsonb       null = false }
  column "status" {
    type    = text
    null    = false
    default = "'pending'"
    check   { expr = "status IN ('pending','approved','rejected')" }
  }
  column "created_at"   { type = timestamptz null = false default = sql("now()") }
  primary_key { columns = [column.id] }
  index "memos_inbox_idx" {
    columns = [column.target_class, column.target_type, column.status]
    where   = "status = 'approved'"
  }
  index "memos_status_idx" { columns = [column.status, sql("created_at DESC")] }
}

table "memo_acks" {
  schema = schema.public
  column "memo_id"         { type = uuid        null = false }
  column "agent_id"        { type = uuid        null = false }
  column "acknowledged_at" { type = timestamptz null = false default = sql("now()") }
  primary_key { columns = [column.memo_id, column.agent_id] }
  foreign_key "memo_acks_memo_id_fkey"  {
    columns     = [column.memo_id]
    ref_columns = [table.memos.column.id]
    on_delete   = "CASCADE"
  }
  foreign_key "memo_acks_agent_id_fkey" {
    columns     = [column.agent_id]
    ref_columns = [table.agents.column.id]
    on_delete   = "CASCADE"
  }
}

table "admin_sessions" {
  schema = schema.public
  column "token"      { type = text        null = false }
  column "created_at" { type = timestamptz null = false default = sql("now()") }
  column "expires_at" { type = timestamptz null = false }
  primary_key { columns = [column.token] }
  index "admin_sessions_expires_idx" { columns = [column.expires_at] }
}
