-- Drop legacy key columns from users
ALTER TABLE users DROP COLUMN api_key;
ALTER TABLE users DROP COLUMN api_key_old;
ALTER TABLE users DROP COLUMN api_key_old_expires_at;

-- Add email field for future welcome-email feature
ALTER TABLE users ADD COLUMN email TEXT;
