-- migrations/postgres/005_admin_oidc.sql
ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS oidc_subject TEXT;
ALTER TABLE admin_users ADD COLUMN IF NOT EXISTS email TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_admin_users_oidc_subject ON admin_users(oidc_subject) WHERE oidc_subject IS NOT NULL;
