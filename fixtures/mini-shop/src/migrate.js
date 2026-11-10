import { pool } from "./db.js";

await pool.query(`
CREATE TABLE staff_keys (
  key_value VARCHAR(64) PRIMARY KEY,
  name VARCHAR(100) NOT NULL
);
CREATE TABLE customers (
  id INT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  email VARCHAR(200) NOT NULL UNIQUE,
  created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE products (
  id INT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(100) NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  stock INT NOT NULL DEFAULT 0,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP
);
CREATE TABLE orders (
  id INT AUTO_INCREMENT PRIMARY KEY,
  customer_id INT NOT NULL,
  status VARCHAR(20) NOT NULL DEFAULT 'pending',
  total DECIMAL(10, 2) NOT NULL DEFAULT 0,
  placed_at DATETIME NOT NULL,
  ship_by DATE,
  updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  FOREIGN KEY (customer_id) REFERENCES customers (id)
);
CREATE TABLE order_items (
  id INT AUTO_INCREMENT PRIMARY KEY,
  order_id INT NOT NULL,
  product_id INT NOT NULL,
  quantity INT NOT NULL,
  price DECIMAL(10, 2) NOT NULL,
  CONSTRAINT items_order FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE,
  FOREIGN KEY (product_id) REFERENCES products (id)
);
CREATE TABLE price_days (
  day DATE PRIMARY KEY,
  factor DECIMAL(4, 2) NOT NULL,
  valid_until DATETIME NOT NULL DEFAULT '9999-12-31 23:59:59',
  stamp_until TIMESTAMP NOT NULL DEFAULT '2038-01-19 03:14:07',
  retired_at DATETIME,
  stamped_unix INT UNSIGNED NOT NULL DEFAULT 0,
  CHECK (day < valid_until)
);
CREATE TRIGGER orders_keep_total BEFORE UPDATE ON orders
  FOR EACH ROW SET NEW.total = NEW.total;
`);
await pool.end();
console.log("migrated");
