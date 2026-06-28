schema "public" {}

table "agents" {
  schema = schema.public
  column "id" {
    type    = uuid
    null    = false
    default = sql("gen_random_uuid()")
  }
  column "name" {
    type = text
    null = false
  }
  column "class" {
    type = text
    null = false
  }
  column "type" {
    type = text
    null = true
  }
  column "model" {
    type = text
    null = false
  }
  column "prompt" {
    type    = text
    null    = false
    default = ""
  }
  column "authtoken" {
    type = text
    null = false
  }
  column "created_at" {
    type    = timestamptz
    null    = false
    default = sql("now()")
  }
  primary_key {
    columns = [column.id]
  }
  index "agents_name_key" {
    unique  = true
    columns = [column.name]
  }
  index "agents_authtoken_key" {
    unique  = true
    columns = [column.authtoken]
  }
  index "agents_class_type_idx" {
    columns = [column.class, column.type]
  }
}

table "requests" {
  schema = schema.public
  column "id" {
    type    = uuid
    null    = false
    default = sql("gen_random_uuid()")
  }
  column "target_class" {
    type = text
    null = false
  }
  column "target_type" {
    type = text
    null = true
  }
  column "payload" {
    type = jsonb
    null = false
  }
  column "response" {
    type = jsonb
    null = true
  }
  column "status" {
    type    = smallint
    null    = false
    default = 0
  }
  column "claimed_by" {
    type = uuid
    null = true
  }
  column "claimed_at" {
    type = timestamptz
    null = true
  }
  column "created_at" {
    type    = timestamptz
    null    = false
    default = sql("now()")
  }
  column "responded_at" {
    type = timestamptz
    null = true
  }
  primary_key {
    columns = [column.id]
  }
  foreign_key "requests_claimed_by_fkey" {
    columns     = [column.claimed_by]
    ref_columns = [table.agents.column.id]
  }
  index "requests_inbox_idx" {
    columns = [column.target_class, column.target_type, column.status]
    where   = "status = 1 AND claimed_by IS NULL"
  }
  index "requests_status_idx" {
    columns = [column.status, column.created_at]
  }
}

table "admin_sessions" {
  schema = schema.public
  column "token" {
    type = text
    null = false
  }
  column "created_at" {
    type    = timestamptz
    null    = false
    default = sql("now()")
  }
  column "expires_at" {
    type    = timestamptz
    null    = false
  }
  primary_key {
    columns = [column.token]
  }
  index "admin_sessions_expires_idx" {
    columns = [column.expires_at]
  }
}