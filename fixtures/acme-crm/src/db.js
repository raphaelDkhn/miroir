import pg from "pg";

pg.types.setTypeParser(pg.types.builtins.NUMERIC, Number);
pg.types.setTypeParser(pg.types.builtins.INT8, Number);
pg.types.setTypeParser(pg.types.builtins.DATE, (value) => value);

export const pool = new pg.Pool({ connectionString: process.env.DATABASE_URL, max: 10 });

export async function query(text, params) {
  return (await pool.query(text, params)).rows;
}

export async function transaction(work) {
  const client = await pool.connect();
  try {
    await client.query("BEGIN");
    const result = await work(client);
    await client.query("COMMIT");
    return result;
  } catch (error) {
    await client.query("ROLLBACK");
    throw error;
  } finally {
    client.release();
  }
}
