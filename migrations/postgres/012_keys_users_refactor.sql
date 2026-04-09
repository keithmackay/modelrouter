-- Drop legacy key columns from users
ALTER TABLE users DROP COLUMN IF EXISTS api_key;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old;
ALTER TABLE users DROP COLUMN IF EXISTS api_key_old_expires_at;

-- Add email field for future welcome-email feature
ALTER TABLE users ADD COLUMN IF NOT EXISTS email TEXT;
