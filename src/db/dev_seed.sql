-- Dev seed: only run when MODELROUTER_DEV_SEED=true
-- Default dev user with key "mr-dev-key" (SHA-256: d1588e8796097c7df97f9307a6c4ab3e7949384c66e314bacb2844dff37c0e10)
INSERT OR IGNORE INTO users (name, api_key, enabled, created_at, metadata)
VALUES ('dev', 'd1588e8796097c7df97f9307a6c4ab3e7949384c66e314bacb2844dff37c0e10', 1, datetime('now'), '{}');
