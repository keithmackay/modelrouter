-- migrations/007_api_key_tag.sql
ALTER TABLE api_keys ADD COLUMN tag TEXT;
