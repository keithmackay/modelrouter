-- migrations/020_group_membership_indexes.sql
-- Indexes for common access patterns
CREATE INDEX idx_group_memberships_group_id ON group_memberships(group_id);
CREATE INDEX idx_group_memberships_user_id  ON group_memberships(user_id);
