-- migrations/005_api_key_expiry.sql
ALTER TABLE api_keys ADD COLUMN expires_at TEXT;
