-- Demo schema for trying Tabula: a small shop.
DROP SCHEMA IF EXISTS shop CASCADE;
CREATE SCHEMA shop;
SET search_path = shop;

CREATE TYPE order_status AS ENUM ('pending', 'paid', 'shipped', 'cancelled');

CREATE TABLE customers (
    id          serial PRIMARY KEY,
    email       text NOT NULL UNIQUE,
    name        text NOT NULL,
    country     char(2),
    created_at  timestamptz NOT NULL DEFAULT now(),
    prefs       jsonb DEFAULT '{}'::jsonb
);
COMMENT ON TABLE customers IS 'People who have an account';

CREATE TABLE products (
    id      serial PRIMARY KEY,
    sku     text NOT NULL UNIQUE,
    title   text NOT NULL,
    price   numeric(10,2) NOT NULL CHECK (price >= 0),
    tags    text[] NOT NULL DEFAULT '{}'
);

CREATE TABLE orders (
    id           bigserial PRIMARY KEY,
    customer_id  int NOT NULL REFERENCES customers(id),
    status       order_status NOT NULL DEFAULT 'pending',
    placed_at    timestamptz NOT NULL DEFAULT now(),
    note         text
);
CREATE INDEX orders_customer_idx ON orders(customer_id);
CREATE INDEX orders_placed_idx ON orders(placed_at DESC);

CREATE TABLE order_items (
    order_id    bigint REFERENCES orders(id) ON DELETE CASCADE,
    product_id  int REFERENCES products(id),
    quantity    int NOT NULL DEFAULT 1,
    PRIMARY KEY (order_id, product_id)
);

INSERT INTO customers (email, name, country, prefs)
SELECT 'user' || g || '@example.com', 'Customer ' || g,
       (ARRAY['US','GB','DE','FR','AU',NULL])[1 + g % 6],
       jsonb_build_object('newsletter', g % 2 = 0, 'theme', CASE WHEN g % 3 = 0 THEN 'dark' ELSE 'light' END)
FROM generate_series(1, 2000) g;

INSERT INTO products (sku, title, price, tags)
SELECT 'SKU-' || lpad(g::text, 5, '0'), 'Product ' || g, round((random() * 200)::numeric, 2),
       ARRAY['tag' || (g % 5), 'tag' || (g % 7)]
FROM generate_series(1, 500) g;

INSERT INTO orders (customer_id, status, placed_at, note)
SELECT 1 + (g % 2000), (ARRAY['pending','paid','shipped','cancelled']::order_status[])[1 + g % 4],
       now() - (g || ' minutes')::interval,
       CASE WHEN g % 10 = 0 THEN 'gift wrap' END
FROM generate_series(1, 100000) g;

INSERT INTO order_items (order_id, product_id, quantity)
SELECT o.id, 1 + (o.id % 500), 1 + (o.id % 3) FROM orders o;

CREATE VIEW order_totals AS
SELECT o.id AS order_id, c.email, o.status, sum(p.price * i.quantity) AS total
FROM orders o JOIN customers c ON c.id = o.customer_id
JOIN order_items i ON i.order_id = o.id JOIN products p ON p.id = i.product_id
GROUP BY o.id, c.email, o.status;

CREATE FUNCTION customer_order_count(cid int) RETURNS bigint
LANGUAGE sql STABLE AS $$ SELECT count(*) FROM shop.orders WHERE customer_id = cid $$;

ANALYZE;
