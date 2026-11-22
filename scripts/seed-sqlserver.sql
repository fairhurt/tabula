-- Demo data for trying Tabula with SQL Server 2017 or later (or Azure SQL):
-- the small shop from seed.sql, in a database called `shop`.
--
--   sqlcmd -S localhost -U sa -i scripts/seed-sqlserver.sql
-- or open it in Tabula and run it: GO lines split it into batches.
IF DB_ID('shop') IS NOT NULL
BEGIN
    ALTER DATABASE shop SET SINGLE_USER WITH ROLLBACK IMMEDIATE;
    DROP DATABASE shop;
END
GO
CREATE DATABASE shop;
GO
USE shop;
GO
CREATE SCHEMA shop;
GO

CREATE TABLE shop.customers (
    id          int IDENTITY(1,1) CONSTRAINT customers_pkey PRIMARY KEY,
    email       nvarchar(255) NOT NULL CONSTRAINT customers_email_key UNIQUE,
    name        nvarchar(255) NOT NULL,
    country     char(2),
    created_at  datetime2(3) NOT NULL DEFAULT SYSDATETIME(),
    prefs       nvarchar(max) DEFAULT N'{}'
);
EXEC sp_addextendedproperty 'MS_Description', 'People who have an account', 'SCHEMA', 'shop', 'TABLE', 'customers';

CREATE TABLE shop.products (
    id      int IDENTITY(1,1) CONSTRAINT products_pkey PRIMARY KEY,
    sku     varchar(32) NOT NULL CONSTRAINT products_sku_key UNIQUE,
    title   nvarchar(255) NOT NULL,
    price   decimal(10,2) NOT NULL CONSTRAINT products_price_check CHECK (price >= 0),
    tags    nvarchar(max) NOT NULL DEFAULT N'[]',
    photo   varbinary(64)
);

CREATE TABLE shop.orders (
    id           bigint IDENTITY(1,1) CONSTRAINT orders_pkey PRIMARY KEY,
    customer_id  int NOT NULL CONSTRAINT orders_customer_id_fkey REFERENCES shop.customers (id),
    status       varchar(10) NOT NULL DEFAULT 'pending'
                 CONSTRAINT orders_status_check CHECK (status IN ('pending', 'paid', 'shipped', 'cancelled')),
    placed_at    datetime2(3) NOT NULL DEFAULT SYSDATETIME(),
    note         nvarchar(max),
    token        uniqueidentifier NOT NULL DEFAULT NEWID()
);
CREATE INDEX orders_customer_idx ON shop.orders (customer_id);
CREATE INDEX orders_placed_idx ON shop.orders (placed_at DESC) INCLUDE (status);

CREATE TABLE shop.order_items (
    order_id    bigint NOT NULL CONSTRAINT order_items_order_id_fkey REFERENCES shop.orders (id) ON DELETE CASCADE,
    product_id  int NOT NULL CONSTRAINT order_items_product_id_fkey REFERENCES shop.products (id),
    quantity    int NOT NULL DEFAULT 1,
    CONSTRAINT order_items_pkey PRIMARY KEY (order_id, product_id)
);
GO

WITH g(n) AS (SELECT TOP (100000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO shop.customers (email, name, country, prefs)
SELECT CONCAT('user', n, '@example.com'), CONCAT('Customer ', n),
       CHOOSE(n % 6 + 1, 'US', 'GB', 'DE', 'FR', 'AU', NULL),
       CONCAT('{"newsletter":', IIF(n % 2 = 0, 'true', 'false'), ',"theme":"', IIF(n % 3 = 0, 'dark', 'light'), '"}')
FROM g WHERE n <= 2000;

WITH g(n) AS (SELECT TOP (500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) FROM sys.all_objects)
INSERT INTO shop.products (sku, title, price, tags, photo)
SELECT CONCAT('SKU-', RIGHT(CONCAT('0000', n), 5)), CONCAT('Product ', n), (n * 7919 % 20000) / 100.0,
       CONCAT('["tag', n % 5, '","tag', n % 7, '"]'),
       IIF(n % 50 = 0, HASHBYTES('SHA2_256', CAST(n AS varchar(10))), NULL)
FROM g;

WITH g(n) AS (SELECT TOP (100000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) FROM sys.all_objects a CROSS JOIN sys.all_objects b)
INSERT INTO shop.orders (customer_id, status, placed_at, note)
SELECT 1 + (n % 2000), CHOOSE(n % 4 + 1, 'pending', 'paid', 'shipped', 'cancelled'),
       DATEADD(minute, -n, SYSDATETIME()), IIF(n % 10 = 0, 'gift wrap', NULL)
FROM g;

INSERT INTO shop.order_items (order_id, product_id, quantity)
SELECT id, 1 + (id % 500), 1 + (id % 3) FROM shop.orders;
GO

CREATE VIEW shop.order_totals AS
SELECT o.id AS order_id, c.email, o.status, SUM(p.price * i.quantity) AS total
FROM shop.orders o JOIN shop.customers c ON c.id = o.customer_id
JOIN shop.order_items i ON i.order_id = o.id JOIN shop.products p ON p.id = i.product_id
GROUP BY o.id, c.email, o.status;
GO

CREATE FUNCTION shop.customer_order_count(@customer int) RETURNS int AS
BEGIN
    RETURN (SELECT COUNT(*) FROM shop.orders WHERE customer_id = @customer);
END;
GO

CREATE SEQUENCE shop.invoice_numbers START WITH 1000;
GO

UPDATE STATISTICS shop.orders;
GO
