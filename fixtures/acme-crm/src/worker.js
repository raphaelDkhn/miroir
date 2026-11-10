import { Worker } from "bullmq";
import nodemailer from "nodemailer";
import { query } from "./db.js";

const smtp = new URL(process.env.SMTP_URL ?? "smtp://127.0.0.1:25");
const transport = nodemailer.createTransport({
  host: smtp.hostname, port: Number(smtp.port || 25), secure: false, ignoreTLS: true,
});
const FROM = "Acme CRM <noreply@acme.test>";

new Worker("email", (job) => transport.sendMail({ from: FROM, ...job.data }),
  { connection: { url: process.env.REDIS_URL }, concurrency: 1 });

async function digest() {
  await query("UPDATE invoices SET status = 'overdue' WHERE status = 'open' AND due_date < current_date");
  const managers = await query("SELECT name, email FROM users WHERE role = 'manager'");
  const [{ open }] = await query("SELECT count(*) AS open FROM deals WHERE status = 'open'");
  for (const manager of managers) {
    await transport.sendMail({ from: FROM, to: manager.email, subject: "Daily pipeline digest",
      text: `Hi ${manager.name}, there are ${open} open deals.` });
  }
}

if (process.env.SCHEDULER_ENABLED === "true") {
  setInterval(() => digest().catch((error) => console.error("digest failed", error)), 5000);
}
console.log("acme-crm worker started");
