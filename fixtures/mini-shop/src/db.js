import mysql from "mysql2/promise";

export const pool = mysql.createPool({
  uri: process.env.DATABASE_URL,
  connectionLimit: 5,
  dateStrings: true,
  decimalNumbers: true,
  multipleStatements: true,
});

export async function query(sql, params) {
  const [rows] = await pool.query(sql, params);
  return rows;
}
