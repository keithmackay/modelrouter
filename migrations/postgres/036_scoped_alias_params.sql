-- Per-alias parameters for scoped alias overrides. See migrations/036_scoped_alias_params.sql.
ALTER TABLE scoped_aliases ADD COLUMN IF NOT EXISTS params TEXT NOT NULL DEFAULT '{}';
