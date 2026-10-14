-- Runs once when a test database container starts (see compose.yaml).
-- Rowbit reads InnoDB's foreign key list, which needs PROCESS.
GRANT PROCESS ON *.* TO 'rowbit'@'%';
-- A least-privilege user, to test that Rowbit refuses what it cannot check.
CREATE USER 'limited'@'%' IDENTIFIED BY 'limited';
GRANT SELECT, INSERT, UPDATE, DELETE, CREATE TEMPORARY TABLES ON rowbit_test.* TO 'limited'@'%';
-- Can see triggers but not InnoDB's foreign key list.
CREATE USER 'noprocess'@'%' IDENTIFIED BY 'noprocess';
GRANT SELECT, INSERT, UPDATE, DELETE, CREATE TEMPORARY TABLES, TRIGGER ON rowbit_test.* TO 'noprocess'@'%';
