"""Seed Orbit's engineering organization into Gitea, through Gitea's own API.

`miroir bake` runs this as a seed command, once Gitea is up. It writes the part of Gitea's API
spec that `api_docs` serves; creates the people, teams, repositories, labels, milestones,
issues, pull requests, reviews, releases and webhooks; then dates each record by its age. Each
reset moves those dates to the day of the episode, so an issue quiet for 40 days stays quiet for
40 days.

Nothing is random at run time: the same seed gives the same issue numbers, which the tasks name.
"""

import base64
import hashlib
import json
import random
import re
import subprocess
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field

GITEA = "http://127.0.0.1:3000"
SPEC = "/opt/orbit/openapi.json"
PSQL = ["psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "gitea", "-v", "ON_ERROR_STOP=1", "-q"]
NOW = int(time.time())
DAY = 86_400

# Personal access tokens of the users the env acts as: this seed, the persona (miroir.yaml),
# the coworker and the clock events' sender (tasks/). Gitea stores only their hashes.
TOKENS = {
    "orbit-admin": "8bc6a59c874bc72c7b0b0895f00cb6464e2be643",
    "dana": "11964c8449bba23d6f37b8c883725fb9cc838a8e",
    "marcus": "f59ad98d31d52c646a1c39f9dfb27389c63fe913",
    "sam": "a167834207cad6a0f5c20bf08c8ceb319254044d",
}

# login: (full name, teams). Dana creates the organization, which makes her an owner.
STAFF = {
    "dana": ("Dana Lee", []),
    "marcus": ("Marcus Webb", ["backend", "oncall"]),
    "priya": ("Priya Shah", ["backend", "platform", "oncall"]),
    "ravi": ("Ravi Kumar", ["backend"]),
    "sam": ("Sam Ortiz", ["platform", "oncall"]),
    "noah": ("Noah Kim", ["platform"]),
    "ines": ("Inès Moreau", ["security"]),
    "jamal": ("Jamal Carter", ["frontend"]),
    "lena": ("Lena Fischer", ["frontend"]),
    "kenji": ("Kenji Watanabe", ["mobile"]),
    "aisha": ("Aisha Bello", ["mobile"]),
    "tom": ("Tom Becker", ["qa"]),
    "chloe": ("Chloé Martin", ["product"]),
}
# People outside the organization: a contractor who starts soon, and two merchants' developers.
OUTSIDERS = {
    "theo": ("Theo Martins", "theo@martins-dev.test"),
    "jordan": ("Jordan Doe", "jordan@shopwise.test"),
    "mei": ("Mei Lin", "mei@lumashop.test"),
}

# name: (permission, repositories, description). An empty list means every repository.
TEAMS = {
    "backend": ("write", ["api", "payments-gateway"], "Backend engineers"),
    "frontend": ("write", ["web", "docs"], "The dashboard and the docs site"),
    "mobile": ("write", ["mobile", "api"], "The mobile app, and the API endpoints it needs"),
    "platform": ("admin", ["infra", "api", "payments-gateway"], "Infrastructure and reliability"),
    "oncall": ("write", ["infra"], "Everyone on the on-call rotation"),
    "security": ("read", [], "Security reviews; read access everywhere"),
    "qa": ("read", ["api", "web", "mobile"], "Quality assurance"),
    "product": ("read", ["api", "web", "mobile", "docs"], "Product managers"),
}
UNITS = ["repo.code", "repo.issues", "repo.pulls", "repo.releases", "repo.wiki", "repo.projects"]

# Gitea reads each pattern as a regular expression over the whole path.
CODEOWNERS = """\
# The owners of a path triage its issues and review its pull requests.
.*                     @marcus
cmd/.*                 @marcus
internal/auth/.*       @priya @ines
internal/billing/.*    @ravi
internal/exports/.*    @noah
internal/webhooks/.*   @marcus
docs/.*                @chloe
"""
INCIDENTS = """\
# Incidents

1. The on-call engineer opens an issue titled `INCIDENT: ...` with the label `incident/active`.
2. Once they say it is mitigated, close the issue and swap `incident/active` for
   `incident/resolved`.
3. The postmortem follows within five working days.
"""
# name: (description, private, files)
REPOS = {
    "api": (
        "Orbit's merchant-facing payments API, in Go",
        True,
        {
            "CODEOWNERS": CODEOWNERS,
            "go.mod": "module git.orbit.test/orbit/api\n\ngo 1.25\n",
            "cmd/api/main.go": "package main\n\nfunc main() { serve() }\n",
            "internal/auth/session.go": "package auth\n\nconst SessionTTLHours = 24\n",
            "internal/billing/invoice.go": "package billing\n\ntype Invoice struct{ ID string }\n",
            "internal/exports/csv.go": "package exports\n\nconst MaxRows = 10000\n",
            "internal/webhooks/retry.go": "package webhooks\n\nconst MaxAttempts = 12\n",
        },
    ),
    "web": (
        "Merchant dashboard, in React and TypeScript",
        True,
        {
            "package.json": '{"name": "orbit-dashboard", "private": true}\n',
            "src/App.tsx": "export function App() {\n  return null;\n}\n",
        },
    ),
    "mobile": (
        "Merchant app for iOS and Android, in React Native",
        True,
        {
            "package.json": '{"name": "orbit-mobile", "private": true}\n',
        },
    ),
    "infra": (
        "Terraform, Kubernetes manifests and runbooks",
        True,
        {
            "runbooks/incidents.md": INCIDENTS,
            "terraform/main.tf": 'terraform {\n  required_version = ">= 1.9"\n}\n',
        },
    ),
    "payments-gateway": (
        "Card network gateway: authorizations, captures and refunds",
        True,
        {
            "src/refunds.go": "package gateway\n\nconst UpstreamTimeoutMs = 300\n",
        },
    ),
    "docs": (
        "Public developer documentation for the Orbit API",
        False,
        {
            "guides/webhooks.md": "# Webhooks\n\nOrbit signs every webhook it sends.\n",
        },
    ),
}

# (name, color, description, exclusive: one label of its scope per issue)
LABELS = [
    ("kind/bug", "d73a4a", "Something is broken", True),
    ("kind/feature", "a2eeef", "New functionality", True),
    ("kind/chore", "cfd3d7", "Maintenance", True),
    ("priority/p0", "b60205", "Drop everything", True),
    ("priority/p1", "d93f0b", "Fix in this release", True),
    ("priority/p2", "fbca04", "Soon", True),
    ("priority/p3", "0e8a16", "Someday", True),
    ("status/needs-info", "fef2c0", "Waiting for the reporter", False),
    ("status/needs-triage", "ededed", "Nobody has triaged it yet", False),
    ("status/blocked", "5319e7", "Blocked by other work", False),
]
AREAS = {
    "api": [
        ("area/auth", False),
        ("area/billing", False),
        ("area/exports", False),
        ("area/webhooks", False),
    ],
    "web": [("area/dashboard", False), ("area/reports", False), ("area/settings", False)],
    "mobile": [("platform/android", False), ("platform/ios", False)],
    "infra": [
        ("area/k8s", False),
        ("area/terraform", False),
        ("incident/active", True),
        ("incident/resolved", True),
    ],
    "payments-gateway": [("area/captures", False), ("area/refunds", False)],
    "docs": [("area/guides", False), ("area/reference", False)],
}
# repo: [(title, due in days, closed)]; negative days are in the past.
MILESTONES = {
    "api": [("v2.3", -50, True), ("v2.4", 6, False), ("v2.5", 41, False)],
    "web": [("Dashboard v3", 20, False)],
    "mobile": [("1.8", 15, False)],
}
DEPLOY_HOOKS = ["api", "web", "payments-gateway"]


@dataclass
class Issue:
    """An issue as it happened; ages are days before the bake."""

    repo: str
    title: str
    poster: str
    opened: float
    body: str = ""
    labels: list[str] = field(default_factory=list)
    assignees: list[str] = field(default_factory=list)
    milestone: str | None = None
    comments: list[tuple[str, float, str]] = field(default_factory=list)
    closed: float | None = None
    closer: str | None = None
    key: str | None = None


@dataclass
class Pull(Issue):
    """A pull request, whose branch adds one file; reviews are (reviewer, event, age, body)."""

    branch: str = ""
    file: tuple[str, str] = ("", "")
    reviews: list[tuple[str, str, float, str]] = field(default_factory=list)


LEAKED_KEY = "acq_live_4f9KQ2mZx7LrT8vWb3NcY6pHs1dE"
LEAK_LOG = f"""Logs from my local run against the sandbox:

```
level=info msg="starting gateway" env=sandbox
ACQUIRER_API_KEY={LEAKED_KEY}
level=info msg="POST /v1/refunds" status=500 duration=312ms
level=error msg="upstream timeout" after=300ms
```

The upstream call gives up after 300 ms, sooner than the acquirer's own timeout."""

EXPORT_CRASH = """Two merchants reported it. Exporting transactions from the dashboard fails once \
the range holds more than about 10,000 rows: the API answers 500, and the log shows

```
panic: runtime error: index out of range [10000] with length 10000
internal/exports/csv.go:42
```"""
GONE_REPORT = (
    "A merchant deleted their endpoint, which now answers 410 Gone, and still sees our retries "
    "in their logs."
)

BUG, FEATURE, CHORE = "kind/bug", "kind/feature", "kind/chore"
P0, P1, P2 = "priority/p0", "priority/p1", "priority/p2"
NEEDS_INFO = "status/needs-info"

# fmt: off
SCENARIO: list[Issue] = [
    # Urgent bugs: four open ones have nobody assigned, the others don't count.
    Issue("api", "Refund webhooks are sent twice when the gateway times out", "tom", 2.2,
          "Seen on three merchants since yesterday.", [BUG, P0, "area/webhooks"]),
    Issue("api", "Idempotency keys are ignored on POST /v1/payouts", "chloe", 5.4,
          "Two payouts were created for one request.", [BUG, P1]),
    Issue("api", "500 on GET /v1/charges when the currency is missing", "tom", 9.1,
          "Steps: call GET /v1/charges?currency= with an empty value.", [BUG, P1],
          comments=[("marcus", 8.0, "Confirmed: the handler assumes a currency.")]),
    Issue("api", "Rate limiter counts retried requests twice", "ravi", 13.5,
          "Retries with the same idempotency key use up the quota twice.", [BUG, P1]),
    Issue("api", "Settlement report is a day off for merchants in UTC-10", "chloe", 11.0,
          "Reported by Lumashop.", [BUG, P1], ["ravi"]),
    Issue("api", "Session tokens survive a password reset", "ines", 16.0,
          "Resetting a password must end every session.", [BUG, P0, "area/auth"], ["priya"],
          key="priya-1"),
    Issue("api", "Charges list ignores the limit parameter", "tom", 34.0, "", [BUG, P1],
          closed=20.0, closer="marcus"),
    Issue("api", "Panic when a webhook URL has no scheme", "ravi", 52.0, "",
          [BUG, P0, "area/webhooks"], milestone="v2.3", closed=49.0, closer="marcus"),
    Issue("api", "Support SEPA Instant payouts", "chloe", 23.0,
          "Asked for by our three largest EU merchants.", [FEATURE, P1]),
    Issue("api", "Webhook signatures use the wrong timestamp", "tom", 7.0, "",
          [BUG, P2, "area/webhooks"]),
    Pull("api", "Fix double refund webhooks on gateway timeouts", "ravi", 1.1,
         "Deduplicates refund events by their gateway reference.", [BUG, P1, "area/webhooks"],
         branch="fix/refund-webhook-dedup",
         file=("internal/webhooks/dedup.go", "package webhooks\n\nfunc dedup() {}\n")),
    # A new issue to triage.
    Issue("api", "CSV export crashes for merchants with more than 10k transactions", "chloe",
          0.15, EXPORT_CRASH, ["status/needs-triage"], key="export-crash"),
    # Waiting on reporters: four have been quiet for over 30 days.
    Issue("api", "Webhook deliveries fail for some merchants", "chloe", 70.0, "",
          [BUG, NEEDS_INFO], key="stale-1", comments=[
              ("marcus", 52.0, "Which merchants and endpoint URLs? We can't reproduce it.")]),
    Issue("api", "API answers 401 right after rotating keys", "tom", 50.0, "",
          [BUG, NEEDS_INFO], key="stale-2", comments=[
              ("priya", 41.0, "Which key was rotated: the secret key or a restricted key?")]),
    Issue("api", "Timeouts on /v1/balance at peak hours", "chloe", 45.0, "",
          [BUG, NEEDS_INFO], key="stale-3", comments=[
              ("ravi", 34.0, "Can you share the request IDs of a few timed-out calls?")]),
    Issue("api", "Payout stays pending after the bank confirms it", "tom", 95.0, "",
          [BUG, NEEDS_INFO], key="stale-4", comments=[
              ("marcus", 80.0, "Do you have the payout ID?"),
              ("tom", 63.0, "Asking the merchant again.")]),
    Issue("api", "Card brand is missing on some charges", "chloe", 20.0, "",
          [BUG, NEEDS_INFO], key="fresh-1", comments=[
              ("ravi", 18.0, "Which card brands?"),
              ("chloe", 12.0, "Mostly Discover so far; asking for more examples.")]),
    Issue("api", "Duplicate customers created through the API", "tom", 9.0, "",
          [BUG, NEEDS_INFO], key="fresh-2", comments=[
              ("marcus", 4.0, "Are the requests sent with idempotency keys?")]),
    Issue("api", "Old SDK fails the TLS handshake", "tom", 120.0, "", [BUG, NEEDS_INFO],
          closed=80.0, closer="marcus"),
    Issue("web", "Export button is greyed out for read-only users", "chloe", 58.0, "",
          [BUG, NEEDS_INFO], key="stale-web", comments=[
              ("lena", 40.0, "Which role does the user have?")]),
    # Priya's work: six open items, and closed ones that stay hers.
    Issue("api", "Rotate JWT signing keys without downtime", "ines", 30.0, "",
          [FEATURE, "area/auth"], ["priya"], "v2.5", key="priya-2"),
    Issue("api", "Move auth sessions to Redis", "marcus", 26.0, "", [CHORE, "area/auth"],
          ["priya", "ravi"], "v2.5", key="priya-3"),
    Issue("infra", "Upgrade staging Postgres to 17", "sam", 21.0, "", [CHORE], ["priya"],
          key="priya-4"),
    Issue("payments-gateway", "Add metrics for the 3DS2 frictionless flow", "marcus", 12.0, "",
          [FEATURE], ["priya"], key="priya-5"),
    Pull("api", "Shorten the session lifetime to 12 hours", "ravi", 3.0,
         "Security asked for 12 hours instead of 24.", ["area/auth"], ["priya"], key="priya-pr",
         branch="auth/session-ttl",
         file=("internal/auth/ttl.go", "package auth\n\nconst NewSessionTTLHours = 12\n")),
    Issue("api", "Throttle failed logins per account", "ines", 64.0, "", [FEATURE, "area/auth"],
          ["priya"], "v2.3", closed=31.0, closer="priya"),
    Issue("infra", "Rotate the staging TLS certificates", "sam", 75.0, "", [CHORE], ["priya"],
          closed=60.0, closer="priya"),
    Issue("payments-gateway", "Capture retries ignore the idempotency window", "ravi", 40.0,
          "", [BUG], ["priya"], closed=15.0, closer="priya"),
    # A live key pasted into a comment.
    Issue("payments-gateway", "Refund webhook answers 500 in the sandbox", "noah", 3.3,
          "Every refund webhook in the sandbox answers 500 since Monday's deploy.",
          [BUG, "area/refunds"], ["ravi"], key="leak", comments=[
              ("ravi", 1.2, LEAK_LOG),
              ("sam", 0.9, "Thanks Ravi. The 300 ms is the shared HTTP client's default.")]),
    # The v2.4 release: six issues done, three still open.
    Issue("api", "Add idempotency keys to refunds", "ravi", 40.0, "", [FEATURE], ["ravi"],
          "v2.4", closed=22.0, closer="ravi", key="v24-done"),
    Issue("api", "Export payouts as CSV", "chloe", 38.0, "", [FEATURE, "area/exports"],
          ["noah"], "v2.4", closed=18.0, closer="noah", key="v24-done"),
    Issue("api", "Back off webhook retries exponentially", "marcus", 35.0, "",
          [FEATURE, "area/webhooks"], ["marcus"], "v2.4", closed=15.0, closer="marcus",
          key="v24-done"),
    Issue("api", "Filter charges by metadata", "chloe", 33.0, "", [FEATURE], ["ravi"], "v2.4",
          closed=11.0, closer="ravi", key="v24-done"),
    Issue("api", "JPY amounts are rounded to two decimals", "tom", 25.0, "", [BUG, P2],
          ["marcus"], "v2.4", closed=8.0, closer="marcus", key="v24-done"),
    Issue("api", "Show settlement dates on balance transactions", "chloe", 28.0, "", [FEATURE],
          ["ravi"], "v2.4", closed=4.0, closer="ravi", key="v24-done"),
    Issue("api", "Deprecate /v1/legacy_charges", "marcus", 27.0, "", [CHORE], ["marcus"],
          "v2.4", key="v24-open"),
    Issue("api", "Add OpenAPI examples for payouts", "chloe", 19.0, "", [CHORE], [], "v2.4",
          key="v24-open"),
    Issue("api", "Paginate /v1/events with cursors", "ravi", 17.0, "", [FEATURE], ["ravi"],
          "v2.4", key="v24-open"),
    # Webhooks and 410 Gone: fixed and shipped in v2.3.1, then reported again.
    Issue("api", "Stop retrying webhooks when the endpoint answers 410 Gone", "marcus", 33.0,
          "", [BUG, "area/webhooks"], ["marcus"], closed=27.0, closer="marcus", key="gone-fix"),
    Issue("api", "Webhook retries never stop when an endpoint answers 410 Gone", "chloe", 4.0,
          GONE_REPORT, [BUG, "area/webhooks"], key="gone-report"),
    # The incident in progress, and an old one.
    Issue("infra", "INCIDENT: elevated 5xx on api.orbit.test", "sam", 0.03,
          "The API's error rate is at 4% since the last deploy. Investigating.",
          ["incident/active"], ["sam"], key="incident", comments=[
              ("sam", 0.02, "A rollback is the fastest way out; preparing it.")]),
    Issue("infra", "INCIDENT: payouts delayed by the bank file job", "sam", 30.0, "",
          ["incident/resolved"], ["sam"], closed=29.8, closer="sam"),
    # Open pull requests, with their reviews: on the dashboard...
    Pull("web", "Add a dark mode toggle to settings", "jamal", 9.0, "",
         [FEATURE, "area/settings"], key="pr-merge", branch="feature/dark-mode",
         file=("src/settings/DarkMode.tsx", "export const DarkMode = () => null;\n"),
         reviews=[("lena", "APPROVED", 5.0, "Looks good, and it works in Safari too.")]),
    Pull("web", "Virtualize the payouts table", "lena", 19.0, "", [CHORE, "area/dashboard"],
         key="pr-keep", branch="perf/virtual-payouts",
         file=("src/payouts/VirtualTable.tsx", "export const VirtualTable = () => null;\n"),
         reviews=[("jamal", "APPROVED", 17.0, "Nice speedup."),
                  ("dana", "REQUEST_CHANGES", 8.0, "Please split out the API client change.")]),
    Pull("web", "Bump vite from 5.4 to 6.0", "jamal", 18.0, "", [CHORE], key="pr-keep",
         branch="deps/vite-6", file=("vite.config.ts", "export default {};\n")),
    Pull("web", "Show refund reasons on the payment page", "lena", 7.0, "",
         [FEATURE, "area/dashboard"], key="pr-merge", branch="feature/refund-reasons",
         file=("src/payments/RefundReason.tsx", "export const RefundReason = () => null;\n"),
         reviews=[("jamal", "APPROVED", 4.0, "LGTM"),
                  ("chloe", "APPROVED", 3.0, "Matches the spec, thanks!"),
                  ("tom", "REQUEST_CHANGES", 2.0, "QA: no reason shows for partial refunds.")]),
    Pull("web", "WIP: New onboarding flow", "jamal", 30.0, "", [FEATURE], key="pr-keep",
         branch="feature/onboarding-v2",
         file=("src/onboarding/Flow.tsx", "export const Flow = () => null;\n"),
         reviews=[("lena", "APPROVED", 25.0, "Early look: the direction is good.")]),
    Pull("web", "Fix the settlement chart's timezone", "lena", 22.0, "", [BUG, "area/reports"],
         key="pr-keep", branch="fix/chart-timezone",
         file=("src/reports/timezone.ts", "export const zone = 'UTC';\n"),
         reviews=[("jamal", "APPROVED", 20.0, "LGTM"),
                  ("jamal", "REQUEST_CHANGES", 16.0, "Safari regression: the axis is a day off.")]),
    Pull("web", "Add a CSV export for disputes", "jamal", 41.0, "", [FEATURE, "area/reports"],
         key="pr-keep", branch="feature/dispute-export",
         file=("src/disputes/export.ts", "export const exportDisputes = () => [];\n"),
         reviews=[("lena", "COMMENT", 40.0, "Should the export include closed disputes?")]),
    # And on the mobile app.
    Pull("mobile", "Cache the payouts list for offline use", "kenji", 4.0, "", [FEATURE],
         key="pr-merge", branch="feature/offline-payouts",
         file=("src/payouts/cache.ts", "export const cache = new Map();\n"),
         reviews=[("aisha", "APPROVED", 2.0, "Works offline on both platforms.")]),
    Pull("mobile", "Upgrade the card scanning library", "aisha", 10.0, "",
         [CHORE, "platform/ios"], key="pr-keep", branch="deps/card-scanner",
         file=("src/scan/version.ts", "export const scanner = '4.2.0';\n"),
         reviews=[("kenji", "REQUEST_CHANGES", 6.0, "It crashes on Android 12 in CI.")]),
    Pull("mobile", "Add a biometric unlock setting", "kenji", 1.5, "", [FEATURE],
         key="pr-keep", branch="feature/biometric-unlock",
         file=("src/settings/Biometric.tsx", "export const Biometric = () => null;\n")),
]
# fmt: on

# Background issues, by repository: (title, labels).
FILLER = {
    "api": [
        ("Add pagination cursors to /v1/refunds", [FEATURE]),
        ("Return 409 when capturing an already captured charge", [BUG, P2]),
        ("Document the error codes of payouts", [CHORE]),
        ("Flaky test: TestRefundWebhookOrdering", [CHORE, "area/webhooks"]),
        ("Upgrade pgx to v5.7", [CHORE]),
        ("Invoices: support line item discounts", [FEATURE, "area/billing"]),
        ("Expose dispute evidence due dates", [FEATURE]),
        ("SSO login fails for uppercase emails", [BUG, P2, "area/auth"]),
        ("Tax rounding differs between invoice and charge", [BUG, "area/billing"]),
        ("Let merchants rotate webhook secrets", [FEATURE, "area/webhooks"]),
        ("Exports time out for ranges over a year", [BUG, "area/exports"]),
        ("Add request IDs to error responses", [FEATURE]),
        ("Remove the deprecated /v1/tokens endpoint", [CHORE]),
        ("Balance transactions miss the fee breakdown", [BUG, "priority/p3"]),
        ("Show when an API key was last used", [FEATURE, "area/auth"]),
        ("Gzip large JSON responses", [FEATURE]),
        ("Trace the charge flow with OpenTelemetry", [CHORE]),
        ("Retry failed payouts automatically", [FEATURE]),
        ("Wrong currency symbol on invoice PDFs", [BUG, "area/billing"]),
        ("Slow query when listing charges by customer", [BUG, P2]),
        ("Validate webhook URLs when they are saved", [FEATURE, "area/webhooks"]),
        ("Customers API: search by email", [FEATURE]),
        ("Drop Go 1.23 from CI", [CHORE]),
        ("Refund reasons are not validated", [BUG, "priority/p3"]),
    ],
    "web": [
        ("Remember the payment table's column widths", [FEATURE, "area/dashboard"]),
        ("Date picker resets when switching tabs", [BUG, P2]),
        ("Upgrade React to 19", [CHORE]),
        ("Filter payments by refunded state", [FEATURE, "area/dashboard"]),
        ("Typo on the payouts page", [BUG, "priority/p3"]),
        ("Settings page loads its data twice", [BUG, "area/settings"]),
        ("Keyboard shortcut for search", [FEATURE]),
        ("Charts flicker when the window resizes", [BUG, "area/reports"]),
        ("Dialogs trap focus in the wrong order", [BUG, P2]),
        ("Show the merchant's timezone in the header", [FEATURE]),
        ("Lazy-load the reports page", [CHORE, "area/reports"]),
        ("Empty state for merchants without payouts", [FEATURE, "area/dashboard"]),
    ],
    "mobile": [
        ("Push notifications arrive twice on Android", [BUG, P2, "platform/android"]),
        ("Face ID prompt on every launch", [BUG, "platform/ios"]),
        ("Crash when opening a refund on iOS 18", [BUG, P2, "platform/ios"]),
        ("Dark mode for the payout screen", [FEATURE]),
        ("Upgrade React Native to 0.79", [CHORE]),
        ("Offline banner never hides", [BUG, "platform/android"]),
        ("Add Spanish translations", [FEATURE]),
        ("Pull to refresh on the payments list", [FEATURE]),
        ("Login screen is cut off on small phones", [BUG, "platform/android"]),
        ("Sign the Android build in CI", [CHORE, "platform/android"]),
    ],
    "infra": [
        ("Move staging to the new VPC", [CHORE, "area/terraform"]),
        ("Alert before certificates expire", [FEATURE]),
        ("Check weekly that Postgres backups restore", [CHORE]),
        ("Pin the Terraform provider versions", [CHORE, "area/terraform"]),
        ("Shrink the node pool at night", [FEATURE, "area/k8s"]),
        ("Grafana dashboard for API latency", [FEATURE]),
        ("Rotate the CI deploy keys", [CHORE]),
        ("Ingress drops long-lived connections", [BUG, "area/k8s"]),
    ],
    "payments-gateway": [
        ("Retry captures on network timeouts", [FEATURE, "area/captures"]),
        ("Log the card network's response codes", [CHORE]),
        ("Partial refunds fail on Amex", [BUG, P2, "area/refunds"]),
        ("Cut authorization latency at peak", [CHORE]),
        ("Handle 3DS timeouts gracefully", [BUG]),
        ("Reconcile refunds with the daily acquirer file", [FEATURE, "area/refunds"]),
    ],
    "docs": [
        ("Webhook signature example is wrong in Python", [BUG, "area/guides"]),
        ("Document pagination for /v1/events", [CHORE, "area/reference"]),
        ("Broken link in the quickstart", [BUG, "area/guides"]),
        ("Explain idempotency keys with an example", [FEATURE, "area/guides"]),
        ("List the test card numbers", [CHORE, "area/reference"]),
        ("Rate limits are not documented", [CHORE, "area/reference"]),
    ],
}
REMARKS = [
    "I can reproduce this on staging.",
    "Looking into it this week.",
    "Is this still happening after the last deploy?",
    "The root cause is in the retry loop; a fix is coming.",
    "Moving this to the next release.",
    "Added a test that covers it.",
    "Merchants have asked about this twice this month.",
    "Can we split this into smaller pieces?",
    "Fixed on main.",
    "Not a priority this quarter.",
]
WRITERS = {
    "api": ["marcus", "ravi", "noah", "sam"],
    "web": ["jamal", "lena"],
    "mobile": ["kenji", "aisha"],
    "infra": ["sam", "noah"],
    "payments-gateway": ["marcus", "ravi"],
    "docs": ["jamal", "lena"],
}
# Who files issues where: QA and product read only some repositories.
REPORTERS = {
    "api": ["marcus", "ravi", "noah", "sam", "chloe", "tom"],
    "web": ["jamal", "lena", "chloe", "tom"],
    "mobile": ["kenji", "aisha", "chloe", "tom"],
    "infra": ["sam", "noah"],
    "payments-gateway": ["marcus", "ravi"],
    "docs": ["jordan", "mei", "chloe"],
}


def call(method: str, path: str, body: object = None, sudo: str | None = None) -> object:
    """Call Gitea's API as the seed's admin, or as `sudo`."""
    headers = {"Authorization": f"token {TOKENS['orbit-admin']}"}
    if sudo is not None:
        headers["Sudo"] = sudo
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    url = path if path.startswith("http") else f"{GITEA}/api/v1{path}"
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            text = response.read()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:500]
        raise SystemExit(f"{method} {path} as {sudo or 'orbit-admin'}: {e.code} {detail}") from e
    return json.loads(text) if text else None


def psql(script: str) -> None:
    """Run SQL on Gitea's database, failing on the first error."""
    subprocess.run(PSQL, input=script, text=True, check=True)


def insert_tokens(logins: list[str]) -> None:
    """Store the known tokens of `logins` as Gitea does: salted PBKDF2 hashes."""
    rows = []
    for login in logins:
        token, salt = TOKENS[login], f"orbit{login[:5]}"
        digest = hashlib.pbkdf2_hmac("sha256", token.encode(), salt.encode(), 10_000, 50).hex()
        rows.append(
            "INSERT INTO access_token (uid, name, token_hash, token_salt, token_last_eight, "
            f"scope, created_unix, updated_unix) SELECT id, 'orbit-env', '{digest}', '{salt}', "
            f"'{token[-8:]}', 'all', {NOW}, {NOW} FROM \"user\" WHERE lower_name = '{login}';"
        )
    psql("\n".join(rows))


# The operations an engineering manager needs. Gitea's spec lists 537, more than api_docs shows;
# this keeps issues, pull requests, releases, teams, members, hooks and files.
KEEP_TAGS = {"issue", "organization", "repository"}
KEEP_PATHS = {"/user", "/users/search", "/users/{username}"}
DROP = re.compile(
    r"/(actions|wiki|git|lfs|push_mirrors|avatar|archive|media|signing-key|activities|times"
    r"|stopwatch|subscription|reactions|pins|assets|blocks|transfer|mirror-sync|languages"
    r"|licenses|topics|flags|keys|editorconfig|tag_protections|projects|attachments|notes"
    r"|statuses|compare|merge-upstream|forks|generate|stargazers|subscribers|commits"
    r"|dependencies|issue_templates|issue_config|new_pin_allowed|pinned|contents-ext"
    r"|file-contents|update)(/|$|\.)"
)


def write_spec() -> None:
    """Keep the operations of the spec Gitea serves that the tasks' users would use."""
    spec = call("GET", f"{GITEA}/swagger.v1.json")
    assert isinstance(spec, dict)
    paths = {}
    for path, item in spec["paths"].items():
        kept = {}
        for method, operation in item.items():
            if method == "parameters" or operation.get("deprecated") or DROP.search(path):
                continue
            if KEEP_TAGS & set(operation.get("tags", [])) or path in KEEP_PATHS:
                kept[method] = operation
        if kept:
            paths[path] = kept | (
                {"parameters": item["parameters"]} if "parameters" in item else {}
            )
    spec["paths"] = paths
    with open(SPEC, "w", encoding="utf-8") as out:
        json.dump(spec, out)


def create_people() -> None:
    """Create every person, staff and outsiders, with a password nobody uses."""
    people = {login: (name, f"{login}@orbit.test") for login, (name, _) in STAFF.items()}
    for login, (name, email) in (people | OUTSIDERS).items():
        call(
            "POST",
            "/admin/users",
            {
                "username": login,
                "full_name": name,
                "email": email,
                "password": hashlib.sha256(f"orbit:{login}".encode()).hexdigest(),
                "must_change_password": False,
                "send_notify": False,
            },
        )


def create_org() -> None:
    """Create the organization as Dana, its repositories and teams, then the repositories' files."""
    call(
        "POST",
        "/orgs",
        {
            "username": "orbit",
            "full_name": "Orbit Payments",
            "description": "Payments infrastructure for online merchants",
            "visibility": "public",
        },
        sudo="dana",
    )
    for repo, (description, private, _) in REPOS.items():
        call(
            "POST",
            "/orgs/orbit/repos",
            {
                "name": repo,
                "description": description,
                "private": private,
                "auto_init": True,
                "default_branch": "main",
                "readme": "Default",
            },
            sudo="dana",
        )
    for team, (permission, repos, description) in TEAMS.items():
        created = call(
            "POST",
            "/orgs/orbit/teams",
            {
                "name": team,
                "description": description,
                "permission": permission,
                "includes_all_repositories": not repos,
                "units_map": dict.fromkeys(UNITS, "read" if permission == "read" else "write"),
            },
        )
        assert isinstance(created, dict)
        for repo in repos:
            call("PUT", f"/teams/{created['id']}/repos/orbit/{repo}")
        for login, (_, teams) in STAFF.items():
            if team in teams:
                call("PUT", f"/teams/{created['id']}/members/{login}")
    for repo, (_, _, files) in REPOS.items():
        changes = [
            {"operation": "create", "path": path, "content": encode(text)}
            for path, text in files.items()
        ]
        call(
            "POST",
            f"/repos/orbit/{repo}/contents",
            {"branch": "main", "message": "Add the project layout", "files": changes},
            sudo=WRITERS[repo][0],
        )


def encode(text: str) -> str:
    """Base64, as Gitea's contents API takes file contents."""
    return base64.b64encode(text.encode()).decode()


def create_labels_and_milestones() -> tuple[dict, dict]:
    """Create each repository's labels and milestones; returns their ids by repo and name."""
    labels: dict[str, dict[str, int]] = {}
    milestones: dict[str, dict[str, int]] = {}
    for repo in REPOS:
        labels[repo] = {}
        areas = [(name, "c5def5", "", exclusive) for name, exclusive in AREAS[repo]]
        for name, color, text, exclusive in LABELS + areas:
            label = call(
                "POST",
                f"/repos/orbit/{repo}/labels",
                {
                    "name": name,
                    "color": f"#{color}",
                    "description": text,
                    "exclusive": exclusive,
                },
            )
            assert isinstance(label, dict)
            labels[repo][name] = label["id"]
        milestones[repo] = {}
        for title, _, closed in MILESTONES.get(repo, []):
            milestone = call(
                "POST",
                f"/repos/orbit/{repo}/milestones",
                {
                    "title": title,
                    "state": "closed" if closed else "open",
                },
            )
            assert isinstance(milestone, dict)
            milestones[repo][title] = milestone["id"]
    return labels, milestones


def filler(rng: random.Random) -> list[Issue]:
    """Background issues. None needs info or triage, is a pull request, is assigned to Priya,
    is an urgent bug, or is in api's v2.4 milestone: the tasks count those exactly.
    """
    issues = []
    for repo, rows in FILLER.items():
        for title, labels in rows:
            opened = rng.uniform(5, 220)
            poster = rng.choice(REPORTERS[repo])
            assignees = [rng.choice(WRITERS[repo])] if rng.random() < 0.6 else []
            closed = rng.uniform(1, opened - 1) if rng.random() < 0.45 else None
            last = closed if closed is not None else 0.5
            comments = sorted(
                (
                    (rng.choice(WRITERS[repo]), rng.uniform(last, opened), rng.choice(REMARKS))
                    for _ in range(rng.randint(0, 3))
                ),
                key=lambda comment: -comment[1],
            )
            milestone = None
            if repo == "api" and closed is not None and opened > 60:
                milestone = "v2.3"
            elif repo == "api" and closed is None and rng.random() < 0.3:
                milestone = "v2.5"
            closer = (assignees or [poster])[0] if closed is not None else None
            issues.append(
                Issue(
                    repo,
                    title,
                    poster,
                    opened,
                    "",
                    labels,
                    assignees,
                    milestone,
                    comments,
                    closed,
                    closer,
                )
            )
    return issues


def create_issue(issue: Issue, labels: dict, milestones: dict) -> dict:
    """Create an issue or a pull request with its comments, reviews and close; returns its ids."""
    repo = issue.repo
    fields = {
        "title": issue.title,
        "body": issue.body,
        "assignees": issue.assignees,
        "labels": [labels[repo][name] for name in issue.labels],
        "milestone": milestones[repo].get(issue.milestone or "", 0),
    }
    if isinstance(issue, Pull):
        path, text = issue.file
        call(
            "POST",
            f"/repos/orbit/{repo}/contents",
            {
                "branch": "main",
                "new_branch": issue.branch,
                "message": issue.title,
                "files": [{"operation": "create", "path": path, "content": encode(text)}],
            },
            sudo=issue.poster,
        )
        created = call(
            "POST",
            f"/repos/orbit/{repo}/pulls",
            fields | {"head": issue.branch, "base": "main"},
            sudo=issue.poster,
        )
    elif issue.poster in WRITERS[repo]:
        created = call("POST", f"/repos/orbit/{repo}/issues", fields, sudo=issue.poster)
    else:
        # Gitea drops the labels, assignees and milestone a reader sets: a writer triages it.
        bare = {"title": issue.title, "body": issue.body}
        created = call("POST", f"/repos/orbit/{repo}/issues", bare, sudo=issue.poster)
        assert isinstance(created, dict)
        triager, number = WRITERS[repo][0], created["number"]
        call(
            "POST",
            f"/repos/orbit/{repo}/issues/{number}/labels",
            {"labels": fields["labels"]},
            sudo=triager,
        )
        call(
            "PATCH",
            f"/repos/orbit/{repo}/issues/{number}",
            {"assignees": fields["assignees"], "milestone": fields["milestone"]},
            sudo=triager,
        )
    assert isinstance(created, dict)
    number = created["number"]
    dated = {"comments": [], "reviews": []}
    for author, age, text in issue.comments:
        comment = call(
            "POST", f"/repos/orbit/{repo}/issues/{number}/comments", {"body": text}, sudo=author
        )
        assert isinstance(comment, dict)
        dated["comments"].append((comment["id"], age))
    for reviewer, event, age, text in getattr(issue, "reviews", []):
        review = call(
            "POST",
            f"/repos/orbit/{repo}/pulls/{number}/reviews",
            {"event": event, "body": text},
            sudo=reviewer,
        )
        assert isinstance(review, dict)
        dated["reviews"].append((review["id"], age))
    if issue.closed is not None:
        call(
            "PATCH", f"/repos/orbit/{repo}/issues/{number}", {"state": "closed"}, sudo=issue.closer
        )
    return {"number": number, **dated}


def ages(issue: Issue) -> tuple[float, float, float | None]:
    """When an issue opened, last changed and closed, in days before the bake."""
    events = [issue.opened, *(age for _, age, _ in issue.comments)]
    events += [age for _, _, age, _ in getattr(issue, "reviews", [])]
    if issue.closed is not None:
        events.append(issue.closed)
    return issue.opened, min(events), issue.closed


def create_releases(numbers: dict[str, list[tuple[str, int, str]]]) -> list[tuple[int, float]]:
    """Publish api's v2.3.0 and v2.3.1, whose notes list their issues as the team writes them."""
    shipped = numbers["v2.3"]
    hotfix = numbers["gone-fix"]
    releases = [
        ("v2.3.0", 48.0, "\n".join(f"- #{number} {title}" for _, number, title in shipped)),
        ("v2.3.1", 26.0, "\n".join(f"- #{number} {title}" for _, number, title in hotfix)),
    ]
    dated = []
    for tag, age, notes in releases:
        release = call(
            "POST",
            "/repos/orbit/api/releases",
            {
                "tag_name": tag,
                "target_commitish": "main",
                "name": tag,
                "body": notes,
            },
            sudo="marcus",
        )
        assert isinstance(release, dict)
        dated.append((release["id"], age))
    return dated


def create_hooks() -> None:
    """Webhooks to the deploy pipeline, which ships a repository when it publishes a release."""
    for repo in DEPLOY_HOOKS:
        call(
            "POST",
            f"/repos/orbit/{repo}/hooks",
            {
                "type": "gitea",
                "active": True,
                "events": ["release"],
                "config": {"url": f"http://127.0.0.1:9000/deploy/{repo}", "content_type": "json"},
            },
        )


def unix(age: float) -> int:
    """The Unix time `age` days before the bake."""
    return NOW - round(age * DAY)


def date(issues: list[tuple[Issue, dict]], milestones: dict, releases: list) -> None:
    """Date every record by its age: issues, comments, reviews, milestones and releases."""
    rows = [
        f"('{issue.repo}', {ids['number']}, {unix(opened)}, {unix(updated)}, "
        f"{0 if closed is None else unix(closed)})"
        for issue, ids in issues
        for opened, updated, closed in [ages(issue)]
    ]
    comments = [f"({cid}, {unix(age)})" for _, ids in issues for cid, age in ids["comments"]]
    reviews = [f"({rid}, {unix(age)})" for _, ids in issues for rid, age in ids["reviews"]]
    deadlines = [
        f"({milestones[repo][title]}, {(unix(-due) // DAY) * DAY + DAY - 1}, "
        f"{unix(-due - 2) if closed else 0})"
        for repo, rows_ in MILESTONES.items()
        for title, due, closed in rows_
    ]
    released = [f"({rid}, {unix(age)})" for rid, age in releases]
    psql(f"""
BEGIN;
UPDATE "user" SET created_unix = {unix(700)} + id * {DAY}, updated_unix = {unix(30)};
UPDATE repository SET created_unix = {unix(640)} + id * {DAY}, updated_unix = {unix(0.5)};
UPDATE label SET created_unix = {unix(600)}, updated_unix = {unix(600)};
UPDATE webhook SET created_unix = {unix(300)}, updated_unix = {unix(300)};
UPDATE issue SET created_unix = v.opened, updated_unix = v.updated, closed_unix = v.closed
  FROM (VALUES {", ".join(rows)}) AS v(repo, index, opened, updated, closed), repository r
  WHERE r.owner_name = 'orbit' AND r.lower_name = v.repo
    AND issue.repo_id = r.id AND issue.index = v.index;
UPDATE comment SET created_unix = issue.created_unix + 60, updated_unix = issue.created_unix + 60
  FROM issue WHERE comment.issue_id = issue.id;
UPDATE comment SET created_unix = issue.closed_unix, updated_unix = issue.closed_unix
  FROM issue WHERE comment.issue_id = issue.id AND comment.type = 2;
UPDATE comment SET created_unix = v.at, updated_unix = v.at
  FROM (VALUES {", ".join(comments)}) AS v(id, at) WHERE comment.id = v.id;
UPDATE review SET created_unix = issue.created_unix + 60, updated_unix = issue.created_unix + 60
  FROM issue WHERE review.issue_id = issue.id;
UPDATE review SET created_unix = v.at, updated_unix = v.at
  FROM (VALUES {", ".join(reviews)}) AS v(id, at) WHERE review.id = v.id;
UPDATE comment SET created_unix = review.created_unix, updated_unix = review.created_unix
  FROM review WHERE comment.review_id = review.id;
UPDATE milestone SET created_unix = {unix(90)}, updated_unix = {unix(1)},
    deadline_unix = v.deadline, closed_date_unix = v.closed
  FROM (VALUES {", ".join(deadlines)}) AS v(id, deadline, closed) WHERE milestone.id = v.id;
UPDATE release SET created_unix = v.at, published_unix = v.at
  FROM (VALUES {", ".join(released)}) AS v(id, at) WHERE release.id = v.id;
COMMIT;
""")


def main() -> None:
    """Seed the organization, then print the numbers the tasks name, by key."""
    insert_tokens(["orbit-admin"])
    write_spec()
    create_people()
    insert_tokens(["dana", "marcus", "sam"])
    create_org()
    labels, milestones = create_labels_and_milestones()
    plan = SCENARIO + filler(random.Random(2026))
    plan.sort(key=lambda issue: (issue.repo, -issue.opened))
    created = [(issue, create_issue(issue, labels, milestones)) for issue in plan]
    numbers: dict[str, list[tuple[str, int, str]]] = {}
    for issue, ids in created:
        keys = [issue.key] if issue.key else []
        if issue.repo == "api" and issue.milestone == "v2.3":
            keys.append("v2.3")
        for key in keys:
            numbers.setdefault(key, []).append((issue.repo, ids["number"], issue.title))
    releases = create_releases(numbers)
    create_hooks()
    date(created, milestones, releases)
    print(
        json.dumps(
            {key: [f"{repo}#{n}" for repo, n, _ in rows] for key, rows in numbers.items()}, indent=1
        )
    )


if __name__ == "__main__":
    main()
