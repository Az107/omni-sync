-- 0002_ops_base_version.sql
--
-- The op log records which version the sender based its change on, so an
-- overwrite can be attributed and reviewed. Nullable: an op creating a new path
-- has no base.

ALTER TABLE ops ADD COLUMN base_version_id INTEGER;
