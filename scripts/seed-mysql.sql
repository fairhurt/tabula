-- Demo data for trying Tabula with MySQL 8 (or MariaDB 10.6+): the small
-- shop from seed.sql, in a database called `shop`.
--
--   mysql -u root -p < scripts/seed-mysql.sql
DROP DATABASE IF EXISTS shop;
CREATE DATABASE shop CHARACTER SET utf8mb4;
USE shop;

CREATE TABLE customers (
    id          INT AUTO_INCREMENT PRIMARY KEY,
    email       VARCHAR(255) NOT NULL UNIQUE,
    name        VARCHAR(255) NOT NULL,
    country     CHAR(2),
    created_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    prefs       JSON
) COMMENT 'People who have an account';

CREATE TABLE products (
    id      INT AUTO_INCREMENT PRIMARY KEY,
    sku     VARCHAR(32) NOT NULL UNIQUE,
    title   VARCHAR(255) NOT NULL,
    price   DECIMAL(10,2) NOT NULL CHECK (price >= 0),
    tags    JSON NOT NULL,
    photo   VARBINARY(64)
);

CREATE TABLE orders (
    id           BIGINT AUTO_INCREMENT PRIMARY KEY,
    customer_id  INT NOT NULL,
    status       ENUM('pending', 'paid', 'shipped', 'cancelled') NOT NULL DEFAULT 'pending',
    placed_at    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    note         TEXT,
    CONSTRAINT orders_customer_id_fkey FOREIGN KEY (customer_id) REFERENCES customers (id),
    INDEX orders_customer_idx (customer_id),
    INDEX orders_placed_idx (placed_at DESC)
);

CREATE TABLE order_items (
    order_id    BIGINT NOT NULL,
    product_id  INT NOT NULL,
    quantity    INT NOT NULL DEFAULT 1,
    PRIMARY KEY (order_id, product_id),
    CONSTRAINT order_items_order_id_fkey FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE,
    CONSTRAINT order_items_product_id_fkey FOREIGN KEY (product_id) REFERENCES products (id)
);

SET SESSION cte_max_recursion_depth = 100000;

INSERT INTO customers (email, name, country, prefs)
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 2000)
SELECT CONCAT('user', n, '@example.com'), CONCAT('Customer ', n),
       ELT(n % 6 + 1, 'US', 'GB', 'DE', 'FR', 'AU', NULL),
       JSON_OBJECT('newsletter', n % 2 = 0, 'theme', IF(n % 3 = 0, 'dark', 'light'))
FROM g;

INSERT INTO products (sku, title, price, tags, photo)
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 500)
SELECT CONCAT('SKU-', LPAD(n, 5, '0')), CONCAT('Product ', n), (n * 7919 % 20000) / 100,
       JSON_ARRAY(CONCAT('tag', n % 5), CONCAT('tag', n % 7)),
       IF(n % 50 = 0, UNHEX(SHA2(n, 256)), NULL)
FROM g;

INSERT INTO orders (customer_id, status, placed_at, note)
WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 100000)
SELECT 1 + (n % 2000), ELT(n % 4 + 1, 'pending', 'paid', 'shipped', 'cancelled'),
       NOW() - INTERVAL n MINUTE, IF(n % 10 = 0, 'gift wrap', NULL)
FROM g;

INSERT INTO order_items (order_id, product_id, quantity)
SELECT id, 1 + (id % 500), 1 + (id % 3) FROM orders;

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.email, o.status, SUM(p.price * i.quantity) AS total
FROM orders o JOIN customers c ON c.id = o.customer_id
JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id
GROUP BY o.id, c.email, o.status;

DELIMITER //
CREATE FUNCTION customer_order_count(customer INT) RETURNS INT READS SQL DATA
BEGIN
    RETURN (SELECT COUNT(*) FROM orders WHERE customer_id = customer);
END //
DELIMITER ;

ANALYZE TABLE customers, products, orders, order_items;
