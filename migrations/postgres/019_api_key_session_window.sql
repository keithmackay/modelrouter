-- Add per-key synthetic session window (seconds). NULL means use server default (28800 = 8 hours).
ALTER TABLE api_keys ADD COLUMN session_window_secs INTEGER;
