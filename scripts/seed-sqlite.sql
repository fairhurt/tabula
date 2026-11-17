-- Demo data for trying Tabula with SQLite: the small shop from seed.sql.
--
--   sqlite3 shop.db < scripts/seed-sqlite.sql
DROP VIEW IF EXISTS order_totals;
DROP TABLE IF EXISTS order_items;
DROP TABLE IF EXISTS orders;
DROP TABLE IF EXISTS products;
DROP TABLE IF EXISTS customers;

BEGIN;

CREATE TABLE customers (
    id          INTEGER PRIMARY KEY,
    email       TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    country     TEXT,
    created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    prefs       TEXT DEFAULT '{}'
);

CREATE TABLE products (
    id      INTEGER PRIMARY KEY,
    sku     TEXT NOT NULL UNIQUE,
    title   TEXT NOT NULL,
    price   NUMERIC NOT NULL CHECK (price >= 0),
    tags    TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE orders (
    id           INTEGER PRIMARY KEY,
    customer_id  INTEGER NOT NULL REFERENCES customers(id),
    status       TEXT NOT NULL DEFAULT 'pending'
                 CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled')),
    placed_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    note         TEXT
);
CREATE INDEX orders_customer_idx ON orders(customer_id);
CREATE INDEX orders_placed_idx ON orders(placed_at DESC);

CREATE TABLE order_items (
    order_id    INTEGER REFERENCES orders(id) ON DELETE CASCADE,
    product_id  INTEGER REFERENCES products(id),
    quantity    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (order_id, product_id)
);

WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 2000)
INSERT INTO customers (email, name, country, prefs)
SELECT 'user' || n || '@example.com', 'Customer ' || n,
       CASE n % 6 WHEN 0 THEN 'US' WHEN 1 THEN 'GB' WHEN 2 THEN 'DE' WHEN 3 THEN 'FR' WHEN 4 THEN 'AU' END,
       json_object('newsletter', json(CASE WHEN n % 2 = 0 THEN 'true' ELSE 'false' END),
                   'theme', CASE WHEN n % 3 = 0 THEN 'dark' ELSE 'light' END)
FROM g;

WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 500)
INSERT INTO products (sku, title, price, tags)
SELECT printf('SKU-%05d', n), 'Product ' || n, (n * 7919 % 20000) / 100.0,
       json_array('tag' || (n % 5), 'tag' || (n % 7))
FROM g;

WITH RECURSIVE g(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM g WHERE n < 100000)
INSERT INTO orders (customer_id, status, placed_at, note)
SELECT 1 + (n % 2000),
       CASE n % 4 WHEN 0 THEN 'pending' WHEN 1 THEN 'paid' WHEN 2 THEN 'shipped' ELSE 'cancelled' END,
       datetime('now', '-' || n || ' minutes'),
       CASE WHEN n % 10 = 0 THEN 'gift wrap' END
FROM g;

INSERT INTO order_items (order_id, product_id, quantity)
SELECT id, 1 + (id % 500), 1 + (id % 3) FROM orders;

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.email, o.status, sum(p.price * i.quantity) AS total
FROM orders o JOIN customers c ON c.id = o.customer_id
JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id
GROUP BY o.id, c.email, o.status;

COMMIT;

ANALYZE;
