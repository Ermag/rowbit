-- The quickstart's shop: 1,500 orders, one a minute. The newest 1,284 are
-- still pending, so some were placed in the last hour.
CREATE DATABASE shop;
USE shop;

CREATE TABLE orders (
  id INT PRIMARY KEY,
  customer VARCHAR(40) NOT NULL,
  status VARCHAR(20) NOT NULL,
  total DECIMAL(10,2) NOT NULL,
  placed_at DATETIME NOT NULL
) ENGINE=InnoDB;

SET SESSION max_recursive_iterations = 2000;
INSERT INTO orders (id, customer, status, total, placed_at)
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1500)
SELECT i,
       CONCAT('customer-', i % 97),
       IF(i > 216, 'pending', 'shipped'),
       ROUND(5 + (i * 7919 % 20000) / 100, 2),
       NOW() - INTERVAL (1500 - i) MINUTE
FROM n;

-- Rowbit's own database user, with exactly the grants in docs/privileges.md.
-- Agents get no database user at all.
CREATE USER 'rowbit'@'%' IDENTIFIED BY 'rowbit-quickstart';
GRANT SELECT, INSERT, UPDATE, DELETE, CREATE TEMPORARY TABLES, TRIGGER ON shop.* TO 'rowbit'@'%';
GRANT PROCESS ON *.* TO 'rowbit'@'%';
