-- migrations/postgres/033_backfill_legacy_admin_role.sql
--
-- Issue #51 replaced the legacy "admin" role with an explicit role vocabulary
-- {"superadmin", "viewer"}. Before the fix, OIDC provisioning defaulted to
-- "admin" (unrecognized), preventing those admins from holding superadmin.
--
-- This migration backfills any rows still holding the legacy 'admin' role to
-- 'superadmin', which is the correct post-#51 mapping for administrative users.
-- Idempotent: safe on databases with no such rows.

UPDATE admin_users
SET role = 'superadmin'
WHERE role = 'admin';
