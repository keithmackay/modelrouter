-- migrations/010_admin_oidc.sql
ALTER TABLE admin_users ADD COLUMN oidc_subject TEXT;
ALTER TABLE admin_users ADD COLUMN email TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_admin_users_oidc_subject ON admin_users(oidc_subject) WHERE oidc_subject IS NOT NULL;
