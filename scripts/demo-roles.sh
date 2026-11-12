#!/bin/sh
# A read-only login for AI agents: the belt-and-braces setup the README
# recommends next to Tabula's own read-only mode.
set -e
psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$POSTGRES_DB" \
  -v pw="$AI_READONLY_PASSWORD" <<'SQL'
CREATE ROLE ai_readonly LOGIN PASSWORD :'pw';
GRANT CONNECT ON DATABASE demo TO ai_readonly;
GRANT USAGE ON SCHEMA shop TO ai_readonly;
GRANT SELECT ON ALL TABLES IN SCHEMA shop TO ai_readonly;
ALTER DEFAULT PRIVILEGES IN SCHEMA shop GRANT SELECT ON TABLES TO ai_readonly;
ALTER ROLE ai_readonly SET default_transaction_read_only = on;
SQL
