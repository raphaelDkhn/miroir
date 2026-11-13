"""The worlds of the Gitea env: three companies, each a Gitea organization the seed builds.

`orbit` and `ferry` are the training worlds; `kestrel` is held out for test. They share no
person, team, repository, label name or issue, so a test score measures what a policy learned
about Gitea, not what it memorized about the data it trained on.

Each world is authored (its people, teams, repositories, CODEOWNERS and labels) and sampled
(its issues, pull requests, reviews and their ages, from the world's seed). Nothing is random
at run time: a world builds the same on every bake.
"""

from __future__ import annotations

import dataclasses
import hashlib
import random
from dataclasses import dataclass, field

# Repository kinds; each has its own title templates and stack traces.
KINDS = ("service", "web", "mobile", "infra", "docs")


@dataclass(frozen=True)
class Noun:
    """A thing a repository deals with, as a title names it."""

    one: str
    many: str


@dataclass(frozen=True)
class Person:
    """Someone with a Gitea account: staff belong to teams, outsiders to none."""

    login: str
    name: str
    role: str  # manager, engineer, security, qa, product, contractor or customer
    teams: tuple[str, ...] = ()
    domain: str = ""

    @property
    def first(self) -> str:
        """The first name, as colleagues write it."""
        return self.name.split()[0]


@dataclass(frozen=True)
class Team:
    """A team of the organization; no repositories means every repository."""

    name: str
    permission: str  # read, write or admin
    repos: tuple[str, ...]
    description: str


@dataclass(frozen=True)
class Release:
    """A release milestone, the milestone after it, and the tags shipped before it."""

    milestone: str
    tag: str
    due: float
    next_milestone: str
    next_due: float
    shipped: tuple[tuple[str, float], ...]  # (tag, days ago), oldest first


@dataclass
class Repo:
    """A repository: its files, owners, labels' areas and the nouns its issues use."""

    name: str
    description: str
    kind: str
    private: bool
    files: dict[str, str]
    # CODEOWNERS lines, the catch-all first; Gitea reads each pattern as a regular expression.
    owners: list[tuple[str, str]]
    areas: list[str]
    nouns: list[Noun]
    release: Release | None = None
    incidents: bool = False

    @property
    def catch_all(self) -> str:
        """Who owns whatever no other CODEOWNERS line names."""
        return self.owners[0][1]


@dataclass(frozen=True)
class Scheme:
    """A world's label names. Gitea makes a scoped label exclusive within its scope."""

    bug: str
    feature: str
    chore: str
    priorities: tuple[str, str, str, str]  # highest first
    needs_info: str
    needs_triage: str
    blocked: str
    area: str  # the scope of area labels, such as "area"
    incident_active: str
    incident_resolved: str

    def labels(self, repo: Repo) -> list[tuple[str, str, str, bool]]:
        """A repository's labels: (name, color, description, exclusive)."""
        rows = [
            (self.bug, "d73a4a", "Something is broken", True),
            (self.feature, "a2eeef", "New functionality", True),
            (self.chore, "cfd3d7", "Maintenance", True),
        ]
        colors = ("b60205", "d93f0b", "fbca04", "0e8a16")
        meanings = ("Drop everything", "Fix in this release", "Soon", "Someday")
        rows += [(p, c, m, True) for p, c, m in zip(self.priorities, colors, meanings, strict=True)]
        rows += [
            (self.needs_info, "fef2c0", "Waiting for the reporter", False),
            (self.needs_triage, "ededed", "Nobody has triaged it yet", False),
            (self.blocked, "5319e7", "Blocked by other work", False),
        ]
        rows += [(f"{self.area}/{area}", "c5def5", "", False) for area in repo.areas]
        if repo.incidents:
            rows += [
                (self.incident_active, "e11d21", "An incident in progress", True),
                (self.incident_resolved, "006b75", "A resolved incident", True),
            ]
        return rows


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
    # (author, age, text)
    comments: list[tuple[str, float, str]] = field(default_factory=list)
    closed: float | None = None
    closer: str | None = None
    # Set when the world is finished: the issue's number in its repository.
    number: int = 0
    # Set by the seed: the ids Gitea gave its comments, in order.
    comment_ids: list[int] = field(default_factory=list)

    @property
    def is_pull(self) -> bool:
        return False

    @property
    def is_open(self) -> bool:
        """Whether it is open at bake."""
        return self.closed is None

    @property
    def last_active(self) -> float:
        """Days since anything happened on it: opened, a comment, a review or the close."""
        ages = [self.opened, *(age for _, age, _ in self.comments)]
        if self.closed is not None:
            ages.append(self.closed)
        return min(ages)


@dataclass
class Pull(Issue):
    """A pull request, whose branch adds one file; reviews are (reviewer, state, age, body)."""

    branch: str = ""
    file: tuple[str, str] = ("", "")
    reviews: list[tuple[str, str, float, str]] = field(default_factory=list)

    @property
    def is_pull(self) -> bool:
        return True

    @property
    def is_draft(self) -> bool:
        """Gitea takes a title that starts with WIP: for a draft."""
        return self.title.startswith("WIP:")

    @property
    def last_active(self) -> float:
        """Days since anything happened on it, its reviews included."""
        return min([super().last_active, *(age for _, _, age, _ in self.reviews)])

    def standing(self) -> dict[str, str]:
        """Each reviewer's verdict: their latest review that approves or asks for changes."""
        verdicts: dict[str, tuple[float, str]] = {}
        for reviewer, state, age, _ in self.reviews:
            if state not in ("APPROVED", "REQUEST_CHANGES"):
                continue
            if reviewer not in verdicts or age < verdicts[reviewer][0]:
                verdicts[reviewer] = (age, state)
        return {reviewer: state for reviewer, (_, state) in verdicts.items()}

    @property
    def ready(self) -> bool:
        """Approved by someone, with no change request standing, and not a draft."""
        verdicts = set(self.standing().values())
        return not self.is_draft and "APPROVED" in verdicts and "REQUEST_CHANGES" not in verdicts


@dataclass
class World:
    """One company: its spec, people, teams, repositories, and what happened in them."""

    name: str
    org: str
    company: str
    about: str
    split: str  # train or test: a test world holds only test tasks
    seed: int
    manager: str
    security: str
    people: dict[str, Person]
    teams: dict[str, Team]
    repos: dict[str, Repo]
    scheme: Scheme
    issues: list[Issue] = field(default_factory=list)
    # (repo, tag, days ago, the issues it shipped)
    releases: list[tuple[str, str, float, list[Issue]]] = field(default_factory=list)
    # (repo, url path, events)
    hooks: list[tuple[str, str, list[str]]] = field(default_factory=list)
    # People a sampler must not give open work to, as an offboarding plant owns theirs.
    reserved: set[str] = field(default_factory=set)
    # PR ages each repository's stale-PR question must stay clear of, by repository.
    stale_band: dict[str, tuple[float, float]] = field(default_factory=dict)

    def staff(self) -> list[Person]:
        """Everyone in the organization."""
        return [p for p in self.people.values() if p.teams or p.login == self.manager]

    def team_repos(self, team: Team) -> list[str]:
        """The repositories a team reaches."""
        return list(team.repos) if team.repos else list(self.repos)

    def access(self, login: str, repo: str) -> str | None:
        """The best permission a person has on a repository through teams, or None."""
        if login == self.manager:
            return "admin"
        best = None
        for name in self.people[login].teams:
            team = self.teams[name]
            if repo in self.team_repos(team):
                best = max(best or "read", team.permission, key=PERMISSIONS.index)
        return best

    def writers(self, repo: str, exclude: frozenset[str] = frozenset()) -> list[str]:
        """Staff other than the manager who can push to a repository."""
        return [
            p.login
            for p in self.staff()
            if p.login != self.manager
            and p.login not in exclude
            and self.access(p.login, repo) in ("write", "admin")
        ]

    def readers(self, repo: str) -> list[str]:
        """Staff other than the manager who can read but not push."""
        return [
            p.login
            for p in self.staff()
            if p.login != self.manager and self.access(p.login, repo) == "read"
        ]

    def outsiders(self, role: str) -> list[Person]:
        """People outside the organization in a role: contractors or customers."""
        return [p for p in self.people.values() if p.role == role]

    def in_repo(self, repo: str) -> list[Issue]:
        """The repository's issues and pull requests."""
        return [i for i in self.issues if i.repo == repo]

    def finish(self) -> None:
        """Number each repository's issues and pull requests, oldest first."""
        self.issues.sort(key=lambda issue: (list(self.repos).index(issue.repo), -issue.opened))
        counts: dict[str, int] = {}
        for issue in self.issues:
            counts[issue.repo] = counts.get(issue.repo, 0) + 1
            issue.number = counts[issue.repo]

    def email(self, login: str) -> str:
        """A person's address: staff at the company's domain, outsiders at their own."""
        person = self.people[login]
        return f"{login}@{person.domain or self.domain}"

    @property
    def domain(self) -> str:
        """The company's mail domain."""
        return f"{self.org}.test"


PERMISSIONS = ("read", "write", "admin")


def token(world: str, login: str) -> str:
    """The personal access token the seed gives a person: the persona's is in miroir.yaml."""
    return hashlib.sha1(f"miroir-gitea/{world}/{login}".encode()).hexdigest()


def staff(login: str, name: str, role: str, *teams: str) -> Person:
    """A member of the organization."""
    return Person(login, name, role, tuple(teams))


def outsider(login: str, name: str, role: str, domain: str) -> Person:
    """Someone outside the organization."""
    return Person(login, name, role, (), domain)


def nouns(*pairs: str) -> list[Noun]:
    """Nouns from `one/many` strings."""
    return [Noun(*pair.split("/")) for pair in pairs]


RUNBOOK = """\
# Incidents

1. The on-call engineer opens an issue titled `INCIDENT: ...` with the label `{active}`.
2. Once they say it is mitigated, close the issue and swap `{active}` for `{resolved}`.
3. The postmortem follows within five working days.
"""


ORBIT_PEOPLE = [
    staff("dana", "Dana Lee", "manager"),
    staff("marcus", "Marcus Webb", "engineer", "backend", "oncall"),
    staff("priya", "Priya Shah", "engineer", "backend", "platform", "oncall"),
    staff("ravi", "Ravi Kumar", "engineer", "backend"),
    staff("olivia", "Olivia Grant", "engineer", "backend", "oncall"),
    staff("sam", "Sam Ortiz", "engineer", "platform", "oncall"),
    staff("noah", "Noah Kim", "engineer", "platform", "backend"),
    staff("jamal", "Jamal Carter", "engineer", "frontend"),
    staff("lena", "Lena Fischer", "engineer", "frontend", "mobile"),
    staff("kenji", "Kenji Watanabe", "engineer", "mobile"),
    staff("aisha", "Aisha Bello", "engineer", "mobile", "frontend"),
    staff("ines", "Inès Moreau", "security", "security"),
    staff("tom", "Tom Becker", "qa", "qa"),
    staff("chloe", "Chloé Martin", "product", "product"),
    outsider("theo", "Theo Martins", "contractor", "martins-dev.test"),
    outsider("yuki", "Yuki Tanaka", "contractor", "tanaka-labs.test"),
    outsider("bram", "Bram de Vries", "contractor", "devries.test"),
    outsider("sofia", "Sofia Rossi", "contractor", "rossi-consulting.test"),
    outsider("jordan", "Jordan Doe", "customer", "shopwise.test"),
    outsider("mei", "Mei Lin", "customer", "lumashop.test"),
]
ORBIT_TEAMS = [
    Team("backend", "write", ("api", "payments-gateway"), "Backend engineers"),
    Team("frontend", "write", ("web", "docs"), "The dashboard and the docs site"),
    Team("mobile", "write", ("mobile", "api"), "The mobile app, and the API endpoints it needs"),
    Team("platform", "admin", ("infra", "api", "payments-gateway"), "Infrastructure"),
    Team("oncall", "write", ("infra",), "Everyone on the on-call rotation"),
    Team("security", "read", (), "Security reviews; read access everywhere"),
    Team("qa", "read", ("api", "web", "mobile"), "Quality assurance"),
    Team("product", "read", ("api", "web", "mobile", "docs"), "Product managers"),
]
ORBIT_REPOS = [
    Repo(
        "api",
        "Orbit's merchant-facing payments API, in Go",
        "service",
        True,
        {
            "go.mod": "module git.orbit.test/orbit/api\n\ngo 1.25\n",
            "cmd/api/main.go": "package main\n\nfunc main() { serve() }\n",
            "internal/auth/session.go": "package auth\n\nconst SessionTTLHours = 24\n",
            "internal/billing/invoice.go": "package billing\n\ntype Invoice struct{}\n",
            "internal/exports/csv.go": "package exports\n\nconst MaxRows = 10000\n",
            "internal/webhooks/retry.go": "package webhooks\n\nconst MaxAttempts = 12\n",
            "internal/payouts/schedule.go": "package payouts\n\nconst CutoffHour = 17\n",
        },
        [
            (".*", "marcus"),
            ("internal/auth/.*", "priya"),
            ("internal/billing/.*", "ravi"),
            ("internal/exports/.*", "noah"),
            ("internal/payouts/.*", "olivia"),
        ],
        ["auth", "billing", "exports", "webhooks", "payouts"],
        nouns(
            "refund/refunds",
            "payout/payouts",
            "charge/charges",
            "invoice/invoices",
            "webhook/webhooks",
            "dispute/disputes",
            "customer/customers",
            "API key/API keys",
        ),
        Release("v2.4", "v2.4.0", 6, "v2.5", 41, (("v2.3.0", 48), ("v2.3.1", 26))),
    ),
    Repo(
        "web",
        "Merchant dashboard, in React and TypeScript",
        "web",
        True,
        {
            "package.json": '{"name": "orbit-dashboard", "private": true}\n',
            "src/App.tsx": "export function App() {\n  return null;\n}\n",
            "src/payments/Table.tsx": "export const Table = () => null;\n",
            "src/reports/Chart.tsx": "export const Chart = () => null;\n",
            "src/settings/Profile.tsx": "export const Profile = () => null;\n",
        },
        [(".*", "jamal"), ("src/payments/.*", "lena"), ("src/settings/.*", "aisha")],
        ["dashboard", "reports", "settings"],
        nouns(
            "payment/payments",
            "payout/payouts",
            "report/reports",
            "dispute/disputes",
            "customer/customers",
        ),
        Release("v3.0", "v3.0.0", 10, "v3.1", 45, (("v2.9.0", 33),)),
    ),
    Repo(
        "mobile",
        "Merchant app for iOS and Android, in React Native",
        "mobile",
        True,
        {
            "package.json": '{"name": "orbit-mobile", "private": true}\n',
            "src/payouts/List.tsx": "export const List = () => null;\n",
            "src/scan/Scanner.tsx": "export const Scanner = () => null;\n",
            "src/auth/Biometric.tsx": "export const Biometric = () => null;\n",
        },
        [(".*", "kenji"), ("src/scan/.*", "aisha"), ("src/auth/.*", "lena")],
        ["android", "ios"],
        nouns(
            "payout/payouts",
            "payment/payments",
            "receipt/receipts",
            "alert/alerts",
            "card reader/card readers",
            "statement/statements",
        ),
        Release("1.8", "1.8.0", 15, "1.9", 50, (("1.7.0", 40),)),
    ),
    Repo(
        "infra",
        "Terraform, Kubernetes manifests and runbooks",
        "infra",
        True,
        {
            "terraform/main.tf": 'terraform {\n  required_version = ">= 1.9"\n}\n',
            "terraform/dns.tf": "# DNS records\n",
            "k8s/ingress.yaml": "apiVersion: networking.k8s.io/v1\nkind: Ingress\n",
        },
        [(".*", "sam"), ("terraform/.*", "noah"), ("k8s/.*", "priya")],
        ["k8s", "terraform"],
        nouns(
            "cluster/clusters",
            "certificate/certificates",
            "backup/backups",
            "node pool/node pools",
            "DNS record/DNS records",
        ),
        incidents=True,
    ),
    Repo(
        "payments-gateway",
        "Card network gateway: authorizations, captures and refunds",
        "service",
        True,
        {
            "src/refunds/refund.go": "package refunds\n\nconst UpstreamTimeoutMs = 300\n",
            "src/captures/capture.go": "package captures\n\nconst MaxRetries = 3\n",
            "src/settlement/file.go": "package settlement\n\nconst CutoffUTC = 22\n",
        },
        [(".*", "ravi"), ("src/captures/.*", "marcus"), ("src/settlement/.*", "olivia")],
        ["captures", "refunds", "settlement"],
        nouns(
            "authorization/authorizations",
            "capture/captures",
            "refund/refunds",
            "settlement file/settlement files",
            "3DS challenge/3DS challenges",
        ),
        Release("2026.11", "2026.11.0", 9, "2026.12", 39, (("2026.10.0", 31),)),
    ),
    Repo(
        "docs",
        "Public developer documentation for the Orbit API",
        "docs",
        False,
        {
            "guides/webhooks.md": "# Webhooks\n\nOrbit signs every webhook it sends.\n",
            "guides/quickstart.md": "# Quickstart\n",
            "reference/errors.md": "# Error codes\n",
        },
        [(".*", "jamal"), ("reference/.*", "lena")],
        ["guides", "reference"],
        nouns(
            "webhook/webhooks",
            "API key/API keys",
            "payout/payouts",
            "refund/refunds",
            "dispute/disputes",
            "SDK/SDKs",
        ),
    ),
]
ORBIT_SCHEME = Scheme(
    "kind/bug",
    "kind/feature",
    "kind/chore",
    ("priority/p0", "priority/p1", "priority/p2", "priority/p3"),
    "status/needs-info",
    "status/needs-triage",
    "status/blocked",
    "area",
    "incident/active",
    "incident/resolved",
)


def orbit() -> World:
    """Orbit Payments, a payments company: the first training world."""
    return world(
        "orbit",
        org="orbit",
        company="Orbit Payments",
        about="a payments company",
        split="train",
        seed=2026,
        manager="dana",
        security="ines",
        people=ORBIT_PEOPLE,
        teams=ORBIT_TEAMS,
        repos=ORBIT_REPOS,
        scheme=ORBIT_SCHEME,
    )


FERRY_PEOPLE = [
    staff("elena", "Elena Petrova", "manager"),
    staff("omar", "Omar Haddad", "engineer", "dispatch", "oncall"),
    staff("grace", "Grace Liu", "engineer", "dispatch", "routing"),
    staff("felix", "Felix Wagner", "engineer", "routing", "sre"),
    staff("amara", "Amara Nwosu", "engineer", "sre", "oncall"),
    staff("tariq", "Tariq Rahman", "engineer", "dispatch", "oncall"),
    staff("hugo", "Hugo Lefèvre", "engineer", "apps", "console"),
    staff("maya", "Maya Cohen", "engineer", "console", "apps"),
    staff("kofi", "Kofi Mensah", "engineer", "apps"),
    staff("signe", "Signe Larsen", "engineer", "console"),
    staff("luis", "Luis Ortega", "engineer", "sre", "routing"),
    staff("nadia", "Nadia Karimi", "security", "security"),
    staff("ben", "Ben Clarke", "qa", "qa"),
    staff("iris", "Iris Novak", "product", "product"),
    outsider("pablo", "Pablo Ruiz", "contractor", "ruiz-mobile.test"),
    outsider("anouk", "Anouk Visser", "contractor", "visser.test"),
    outsider("dev", "Dev Patel", "contractor", "patel-studio.test"),
    outsider("ewa", "Ewa Kowalska", "contractor", "kowalska.test"),
    outsider("carmen", "Carmen Silva", "customer", "freshcart.test"),
    outsider("owen", "Owen Price", "customer", "parcelpoint.test"),
]
FERRY_TEAMS = [
    Team("dispatch", "write", ("dispatch", "routing"), "Shipments, slots and assignment"),
    Team("apps", "write", ("driver-app", "dispatch"), "The driver app, and its endpoints"),
    Team("console", "write", ("ops-console", "handbook"), "The operations console and docs"),
    Team("routing", "write", ("routing",), "Route planning and ETAs"),
    Team("sre", "admin", ("platform", "dispatch", "routing"), "Site reliability"),
    Team("oncall", "write", ("platform",), "Everyone on the on-call rotation"),
    Team("security", "read", (), "Security reviews; read access everywhere"),
    Team("qa", "read", ("dispatch", "driver-app", "ops-console"), "Quality assurance"),
    Team("product", "read", ("dispatch", "driver-app", "ops-console", "handbook"), "Product"),
]
FERRY_REPOS = [
    Repo(
        "dispatch",
        "Shipment dispatch service: slots, driver assignment and tracking, in Go",
        "service",
        True,
        {
            "go.mod": "module git.ferrylane.test/ferrylane/dispatch\n\ngo 1.25\n",
            "cmd/dispatch/main.go": "package main\n\nfunc main() { run() }\n",
            "internal/slots/booking.go": "package slots\n\nconst SlotMinutes = 30\n",
            "internal/assign/driver.go": "package assign\n\nconst MaxStops = 40\n",
            "internal/tracking/events.go": "package tracking\n\ntype Event struct{}\n",
            "internal/manifests/export.go": "package manifests\n\nconst MaxLines = 5000\n",
        },
        [
            (".*", "omar"),
            ("internal/slots/.*", "grace"),
            ("internal/assign/.*", "tariq"),
            ("internal/tracking/.*", "hugo"),
            ("internal/manifests/.*", "felix"),
        ],
        ["slots", "assignment", "tracking", "manifests"],
        nouns(
            "shipment/shipments",
            "delivery slot/delivery slots",
            "driver assignment/driver assignments",
            "manifest/manifests",
            "tracking event/tracking events",
            "depot/depots",
        ),
        Release("v5.3", "v5.3.0", 8, "v5.4", 38, (("v5.2.0", 36), ("v5.2.1", 19))),
    ),
    Repo(
        "driver-app",
        "The drivers' app for Android and iOS, in Kotlin and Swift",
        "mobile",
        True,
        {
            "app/src/routes/RouteList.kt": "class RouteList\n",
            "app/src/camera/Photo.kt": "class Photo\n",
            "app/src/auth/Login.kt": "class Login\n",
        },
        [(".*", "hugo"), ("app/src/camera/.*", "kofi"), ("app/src/auth/.*", "maya")],
        ["android", "ios"],
        nouns(
            "route/routes",
            "stop/stops",
            "delivery photo/delivery photos",
            "shift/shifts",
            "signature/signatures",
            "parcel scan/parcel scans",
        ),
        Release("3.12", "3.12.0", 12, "3.13", 44, (("3.11.0", 30),)),
    ),
    Repo(
        "ops-console",
        "The operations console for depot managers, in Vue",
        "web",
        True,
        {
            "package.json": '{"name": "ops-console", "private": true}\n',
            "src/views/Depots.vue": "<template><div /></template>\n",
            "src/exceptions/List.vue": "<template><div /></template>\n",
            "src/reports/Daily.vue": "<template><div /></template>\n",
        },
        [(".*", "maya"), ("src/reports/.*", "signe"), ("src/exceptions/.*", "hugo")],
        ["views", "reports"],
        nouns(
            "depot/depots",
            "driver/drivers",
            "shipment/shipments",
            "exception/exceptions",
            "report/reports",
        ),
        Release("2026.4", "2026.4.0", 16, "2026.5", 47, (("2026.3.0", 29),)),
    ),
    Repo(
        "routing",
        "Route planning and arrival estimates, in Python",
        "service",
        True,
        {
            "pyproject.toml": '[project]\nname = "routing"\n',
            "routing/solver/vrp.py": "MAX_VEHICLES = 200\n",
            "routing/eta/model.py": "BUFFER_MINUTES = 5\n",
            "routing/feeds/traffic.py": "POLL_SECONDS = 60\n",
        },
        [(".*", "felix"), ("routing/eta/.*", "luis"), ("routing/solver/.*", "grace")],
        ["solver", "eta", "feeds"],
        nouns(
            "route plan/route plans",
            "traffic feed/traffic feeds",
            "ETA/ETAs",
            "vehicle profile/vehicle profiles",
        ),
        Release("v1.9", "v1.9.0", 5, "v1.10", 35, (("v1.8.0", 27),)),
    ),
    Repo(
        "platform",
        "Terraform, Kubernetes manifests and runbooks",
        "infra",
        True,
        {
            "terraform/main.tf": 'terraform {\n  required_version = ">= 1.9"\n}\n',
            "k8s/dispatch.yaml": "apiVersion: apps/v1\nkind: Deployment\n",
        },
        [(".*", "amara"), ("terraform/.*", "luis"), ("k8s/.*", "felix")],
        ["k8s", "terraform"],
        nouns(
            "cluster/clusters",
            "certificate/certificates",
            "queue/queues",
            "database replica/database replicas",
        ),
        incidents=True,
    ),
    Repo(
        "handbook",
        "Public integration guides for Ferrylane's shippers",
        "docs",
        False,
        {
            "guides/api.md": "# The shipments API\n",
            "guides/webhooks.md": "# Tracking webhooks\n",
            "reference/statuses.md": "# Shipment statuses\n",
        },
        [(".*", "signe"), ("guides/.*", "maya")],
        ["guides", "reference"],
        nouns(
            "webhook/webhooks",
            "shipment label/shipment labels",
            "status/statuses",
            "pickup request/pickup requests",
            "rate quote/rate quotes",
            "SDK/SDKs",
        ),
    ),
]
FERRY_SCHEME = Scheme(
    "type/bug",
    "type/feature",
    "type/chore",
    ("priority/critical", "priority/high", "priority/medium", "priority/low"),
    "triage/needs-info",
    "triage/new",
    "triage/blocked",
    "area",
    "outage/ongoing",
    "outage/resolved",
)


def ferry() -> World:
    """Ferrylane Logistics, a freight and delivery company: the second training world."""
    return world(
        "ferry",
        org="ferrylane",
        company="Ferrylane Logistics",
        about="a freight and delivery company",
        split="train",
        seed=3114,
        manager="elena",
        security="nadia",
        people=FERRY_PEOPLE,
        teams=FERRY_TEAMS,
        repos=FERRY_REPOS,
        scheme=FERRY_SCHEME,
    )


KESTREL_PEOPLE = [
    staff("rahul", "Rahul Mehta", "manager"),
    staff("claire", "Claire Dubois", "engineer", "core", "oncall"),
    staff("mateo", "Mateo Fernández", "engineer", "core", "integrations"),
    staff("anika", "Anika Johansson", "engineer", "core", "mobile"),
    staff("jun", "Jun Park", "engineer", "integrations", "oncall"),
    staff("zoe", "Zoe Adams", "engineer", "web", "mobile"),
    staff("bilal", "Bilal Aziz", "engineer", "web"),
    staff("lucia", "Lucía Romero", "engineer", "mobile", "web"),
    staff("erik", "Erik Lindqvist", "engineer", "infra", "oncall"),
    staff("ngozi", "Ngozi Eze", "engineer", "infra", "core"),
    staff("victor", "Victor Hale", "engineer", "integrations"),
    staff("hannah", "Hannah Weiss", "security", "security"),
    staff("rosa", "Rosa Jiménez", "qa", "qa"),
    staff("kai", "Kai Nakamura", "product", "product"),
    outsider("leon", "Leon Brandt", "contractor", "brandt-it.test"),
    outsider("ama", "Ama Owusu", "contractor", "owusu.test"),
    outsider("finn", "Finn Byrne", "contractor", "byrne-dev.test"),
    outsider("sana", "Sana Malik", "contractor", "malik-labs.test"),
    outsider("greta", "Greta Holm", "customer", "northclinic.test"),
    outsider("paolo", "Paolo Conti", "customer", "medivia.test"),
]
KESTREL_TEAMS = [
    Team("core", "write", ("scheduler", "integrations"), "The scheduling core"),
    Team("web", "write", ("patient-portal", "api-docs"), "The patient portal and the docs"),
    Team("mobile", "write", ("clinic-app", "scheduler"), "The clinic app, and its endpoints"),
    Team("integrations", "write", ("integrations",), "HL7, FHIR and insurers"),
    Team("infra", "admin", ("cloud", "scheduler", "integrations"), "Infrastructure"),
    Team("oncall", "write", ("cloud",), "Everyone on the on-call rotation"),
    Team("security", "read", (), "Security reviews; read access everywhere"),
    Team("qa", "read", ("scheduler", "patient-portal", "clinic-app"), "Quality assurance"),
    Team("product", "read", ("scheduler", "patient-portal", "clinic-app", "api-docs"), "Product"),
]
KESTREL_REPOS = [
    Repo(
        "scheduler",
        "Appointment scheduling service, in Kotlin",
        "service",
        True,
        {
            "build.gradle.kts": 'plugins { kotlin("jvm") }\n',
            "src/main/kotlin/Main.kt": "fun main() = serve()\n",
            "src/main/kotlin/booking/Booking.kt": "const val HOLD_MINUTES = 10\n",
            "src/main/kotlin/reminders/Sms.kt": "const val LEAD_HOURS = 24\n",
            "src/main/kotlin/waitlist/Waitlist.kt": "const val MAX_OFFERS = 3\n",
            "src/main/kotlin/calendars/Sync.kt": "const val SYNC_MINUTES = 15\n",
        },
        [
            (".*", "claire"),
            ("src/main/kotlin/booking/.*", "anika"),
            ("src/main/kotlin/reminders/.*", "mateo"),
            ("src/main/kotlin/waitlist/.*", "ngozi"),
            ("src/main/kotlin/calendars/.*", "zoe"),
        ],
        ["booking", "reminders", "waitlist", "calendars"],
        nouns(
            "appointment/appointments",
            "booking/bookings",
            "reminder/reminders",
            "waitlist offer/waitlist offers",
            "provider calendar/provider calendars",
            "time slot/time slots",
        ),
        Release("4.7", "4.7.0", 9, "4.8", 40, (("4.6.0", 35), ("4.6.1", 17))),
    ),
    Repo(
        "patient-portal",
        "The portal where patients book and pay, in Vue",
        "web",
        True,
        {
            "package.json": '{"name": "patient-portal", "private": true}\n',
            "src/pages/Appointments.vue": "<template><div /></template>\n",
            "src/billing/Invoices.vue": "<template><div /></template>\n",
            "src/forms/Intake.vue": "<template><div /></template>\n",
        },
        [(".*", "zoe"), ("src/billing/.*", "bilal"), ("src/forms/.*", "bilal")],
        ["pages", "forms", "billing"],
        nouns(
            "appointment/appointments",
            "invoice/invoices",
            "message/messages",
            "intake form/intake forms",
        ),
        Release("r24", "r24.0", 11, "r25", 42, (("r23.0", 28),)),
    ),
    Repo(
        "clinic-app",
        "The front-desk app for clinics, in Flutter",
        "mobile",
        True,
        {
            "pubspec.yaml": "name: clinic_app\n",
            "lib/schedule/day_view.dart": "class DayView {}\n",
            "lib/checkin/scanner.dart": "class Scanner {}\n",
            "lib/notes/editor.dart": "class Editor {}\n",
        },
        [(".*", "lucia"), ("lib/checkin/.*", "zoe"), ("lib/notes/.*", "anika")],
        ["android", "ios"],
        nouns(
            "schedule/schedules",
            "check-in/check-ins",
            "patient note/patient notes",
            "room/rooms",
            "waiting list/waiting lists",
            "visit summary/visit summaries",
        ),
        Release("2.3", "2.3.0", 14, "2.4", 49, (("2.2.0", 37),)),
    ),
    Repo(
        "integrations",
        "HL7 and FHIR adapters and insurance eligibility checks, in Java",
        "service",
        True,
        {
            "pom.xml": "<project/>\n",
            "src/main/java/hl7/Parser.java": "class Parser {}\n",
            "src/main/java/fhir/Client.java": "class Client {}\n",
            "src/main/java/eligibility/Check.java": "class Check {}\n",
        },
        [
            (".*", "victor"),
            ("src/main/java/fhir/.*", "jun"),
            ("src/main/java/eligibility/.*", "mateo"),
        ],
        ["hl7", "fhir", "eligibility"],
        nouns(
            "HL7 message/HL7 messages",
            "FHIR resource/FHIR resources",
            "eligibility check/eligibility checks",
            "lab result/lab results",
        ),
        Release("v8.1", "v8.1.0", 7, "v8.2", 37, (("v8.0.0", 41),)),
    ),
    Repo(
        "cloud",
        "Terraform, Helm charts and runbooks",
        "infra",
        True,
        {
            "terraform/main.tf": 'terraform {\n  required_version = ">= 1.9"\n}\n',
            "helm/values.yaml": "replicas: 3\n",
        },
        [(".*", "erik"), ("terraform/.*", "ngozi"), ("helm/.*", "jun")],
        ["helm", "terraform"],
        nouns(
            "cluster/clusters",
            "certificate/certificates",
            "backup/backups",
            "VPN tunnel/VPN tunnels",
        ),
        incidents=True,
    ),
    Repo(
        "api-docs",
        "Public documentation of the Kestrel booking API",
        "docs",
        False,
        {
            "guides/booking-api.md": "# The booking API\n",
            "guides/webhooks.md": "# Webhooks\n",
            "reference/errors.md": "# Error codes\n",
        },
        [(".*", "bilal"), ("guides/.*", "zoe")],
        ["guides", "reference"],
        nouns(
            "webhook/webhooks",
            "SMS template/SMS templates",
            "booking/bookings",
            "provider/providers",
            "clinic/clinics",
            "SDK/SDKs",
        ),
    ),
]
KESTREL_SCHEME = Scheme(
    "kind/defect",
    "kind/enhancement",
    "kind/maintenance",
    ("severity/s1", "severity/s2", "severity/s3", "severity/s4"),
    "state/awaiting-reporter",
    "state/untriaged",
    "state/blocked",
    "component",
    "incident/open",
    "incident/closed",
)


def kestrel() -> World:
    """Kestrel Health, which makes scheduling software for clinics: the test world."""
    return world(
        "kestrel",
        org="kestrel",
        company="Kestrel Health",
        about="a company that makes scheduling software for clinics",
        split="test",
        seed=4405,
        manager="rahul",
        security="hannah",
        people=KESTREL_PEOPLE,
        teams=KESTREL_TEAMS,
        repos=KESTREL_REPOS,
        scheme=KESTREL_SCHEME,
    )


def world(
    name: str,
    *,
    org: str,
    company: str,
    about: str,
    split: str,
    seed: int,
    manager: str,
    security: str,
    people: list[Person],
    teams: list[Team],
    repos: list[Repo],
    scheme: Scheme,
) -> World:
    """A world, with copies of its repositories that add their runbook and CODEOWNERS."""
    repos = [dataclasses.replace(repo, files=dict(repo.files)) for repo in repos]
    for repo in repos:
        if repo.incidents:
            active, resolved = scheme.incident_active, scheme.incident_resolved
            repo.files["runbooks/incidents.md"] = RUNBOOK.format(active=active, resolved=resolved)
    built = World(
        name,
        org,
        company,
        about,
        split,
        seed,
        manager,
        security,
        {p.login: p for p in people},
        {t.name: t for t in teams},
        {r.name: r for r in repos},
        scheme,
    )
    for repo in repos:
        built.repos[repo.name].files["CODEOWNERS"] = codeowners(repo)
    return built


def codeowners(repo: Repo) -> str:
    lines = ["# The owners of a path triage its issues and review its pull requests."]
    width = max(len(pattern) for pattern, _ in repo.owners) + 2
    lines += [f"{pattern:<{width}}@{login}" for pattern, login in repo.owners]
    return "\n".join(lines) + "\n"


WORLDS = {"orbit": orbit, "ferry": ferry, "kestrel": kestrel}

# Title templates by repository kind: {one} and {many} are a noun, {One} and {Many} capitalized,
# {slug} its plural in snake case and {Camel} its singular in CamelCase.
TITLES = {
    "service": {
        "bug": [
            "{Many} list ignores the limit parameter",
            "500 on GET /v1/{slug} with an empty filter",
            "Duplicate {many} created when a client retries",
            "{One} timestamps are an hour off after the DST change",
            "Slow query when listing {many} by owner",
            "Race when two workers update the same {one}",
            "Pagination skips {many} created in the same second",
            "Deleting a {one} leaves orphaned audit rows",
            "{One} export truncates at 10,000 rows",
            "Rate limiter counts {one} retries twice",
            "Validation errors on {many} name the wrong field",
            "{Many} endpoint ignores the If-Match header",
            "Retries of {many} never back off",
            "Search over {many} misses accented names",
        ],
        "feature": [
            "Filter {many} by metadata",
            "Cursor pagination for /v1/{slug}",
            "Export {many} as CSV",
            "Bulk update endpoint for {many}",
            "Idempotency keys for creating {many}",
            "Webhook event when a {one} changes",
        ],
        "chore": [
            "Document the error codes of {many}",
            "Flaky test: Test{Camel}Ordering",
            "Remove the deprecated /v1/legacy_{slug} endpoint",
            "Trace the {one} handlers with OpenTelemetry",
            "Add the missing index on the {slug} table",
        ],
        "pull": [
            "Speed up listing {many}",
            "Handle timeouts when creating {many}",
            "Validate {one} input at the edge",
            "Log {one} errors with request IDs",
            "Refactor the {one} store",
            "Cache {one} lookups",
        ],
    },
    "web": {
        "bug": [
            "The {many} page loads its data twice",
            "Date picker resets on the {many} page",
            "The {many} table loses its sort order after a refresh",
            "Charts on the {many} page flicker when the window resizes",
            "Export button is greyed out on {many} for read-only users",
            "Search on the {many} page is case-sensitive",
            "Dialogs on the {many} page trap focus in the wrong order",
            "The {many} page shows times in UTC, not the account's timezone",
        ],
        "feature": [
            "Remember the column widths on the {many} page",
            "Keyboard shortcut to search {many}",
            "Empty state for accounts without {many}",
            "Filter {many} by status",
            "Dark mode for the {many} page",
        ],
        "chore": [
            "Lazy-load the {many} page",
            "Move the {many} page to the new table component",
            "End-to-end tests for the {many} page",
        ],
        "pull": [
            "Virtualize the {many} table",
            "Skeleton loaders for the {many} page",
            "Move {many} fetching to the query cache",
            "Accessible labels on the {many} page",
            "Prefetch {many} on hover",
        ],
    },
    "mobile": {
        "bug": [
            "Crash when opening a {one} on iOS 18",
            "{One} notifications arrive twice on Android",
            "The {many} list never refreshes after going offline",
            "The {many} screen is cut off on small phones",
            "Pull to refresh duplicates {many}",
            "The {one} screen freezes after a rotation",
            "Tapping a {one} twice opens it twice",
            "Wrong date format on the {many} screen in German",
            "Battery drain while the {many} screen is open",
            "The {many} badge count is stale after login",
        ],
        "feature": [
            "Offline cache for {many}",
            "A home-screen widget for today's {many}",
            "Share a {one} as a PDF",
            "Search {many} by name",
        ],
        "chore": [
            "Snapshot tests for the {many} screen",
            "Move the {many} screen to the new navigation",
            "Drop the old {one} storage format",
        ],
        "pull": [
            "Paginate the {many} list",
            "Haptics when a {one} is saved",
            "Retry {one} uploads on a flaky network",
            "Smaller images on the {many} screen",
        ],
    },
    "infra": {
        "bug": [
            "{One} alerts fire during every deploy",
            "Ingress drops long-lived connections to the {many}",
            "Nightly {one} job runs twice after a failover",
            "Terraform plan always shows a diff for the {many}",
            "The {one} dashboard reads stale metrics",
            "Disk fills up on the {one} hosts",
            "{One} logs are not shipped after a restart",
        ],
        "feature": [
            "Alert a week before {many} expire",
            "Dashboard for {one} latency",
            "Autoscale the {one} workers at night",
            "Check weekly that {many} restore",
        ],
        "chore": [
            "Pin the {one} module versions",
            "Rotate the {one} credentials",
            "Move the {many} to the new VPC",
            "Delete the unused staging {many}",
        ],
        "pull": [
            "Tag every {one} with its owner",
            "Tighten the {one} security group",
            "Bump the {one} module",
            "Alert on {one} saturation",
        ],
    },
    "docs": {
        "bug": [
            "Broken link in the {many} guide",
            "The {many} example is wrong in Python",
            "The {many} page shows the old API version",
            "Typo in the {many} reference",
            "The {many} guide skips authentication",
            "Search on the docs site misses {many}",
            "Code sample for {many} does not compile",
        ],
        "feature": [
            "Explain {many} with a full example",
            "A quickstart for {many}",
            "Code samples in Go for {many}",
        ],
        "chore": [
            "Document pagination for {many}",
            "List the error codes of {many}",
            "Move the {many} guide to the new layout",
        ],
        "pull": [
            "Rewrite the {many} guide",
            "Diagrams for {many}",
            "Fix the anchors in the {many} reference",
        ],
    },
}

REMARKS = [
    "I can reproduce this on staging.",
    "Looking into it this week.",
    "Is this still happening after the last deploy?",
    "The root cause is in the retry loop; a fix is coming.",
    "Moving this to the next release.",
    "Added a test that covers it.",
    "Customers have asked about this twice this month.",
    "Can we split this into smaller pieces?",
    "Fixed on main.",
    "Not a priority this quarter.",
]


def title(repo: Repo, noun: Noun, template: str) -> str:
    return template.format(
        one=noun.one,
        many=noun.many,
        One=noun.one[0].upper() + noun.one[1:],
        Many=noun.many[0].upper() + noun.many[1:],
        slug=noun.many.lower().replace(" ", "_").replace("-", "_"),
        Camel="".join(word[0].upper() + word[1:] for word in noun.one.replace("-", " ").split()),
        repo=repo.name,
    )


class Titles:
    """Hands out each repository's titles without repeats, in an order the seed fixes."""

    def __init__(self, world: World, rng: random.Random) -> None:
        self.left: dict[tuple[str, str], list[str]] = {}
        for repo in world.repos.values():
            for kind, templates in TITLES[repo.kind].items():
                found = [title(repo, n, t) for t in templates for n in repo.nouns]
                rng.shuffle(found)
                self.left[(repo.name, kind)] = found

    def take(self, repo: str, kind: str) -> str:
        """The next unused title of a kind in a repository."""
        left = self.left[(repo, kind)]
        if not left:
            raise ValueError(f"repository {repo} has run out of {kind} titles")
        return left.pop()


def background(world: World, rng: random.Random, titles: Titles) -> None:
    """The issues nothing asks about, by repository: labeled, assigned, discussed, some closed.

    None waits on its reporter or for triage, has a top priority or an incident label, is a pull
    request or sits in a release milestone, and none open is assigned to a reserved person: the
    plants own those, and the tasks count them exactly.
    """
    scheme = world.scheme
    for repo in world.repos.values():
        writers = world.writers(repo.name)
        assignable = world.writers(repo.name, frozenset(world.reserved))
        reporters = writers + world.readers(repo.name)
        if not repo.private:
            reporters += [p.login for p in world.outsiders("customer")]
        for _ in range(rng.randint(12, 16)):
            kind = rng.choices(["bug", "feature", "chore"], [5, 3, 2])[0]
            labels = [{"bug": scheme.bug, "feature": scheme.feature, "chore": scheme.chore}[kind]]
            if rng.random() < 0.5:
                labels.append(rng.choice(scheme.priorities[2:]))
            if rng.random() < 0.5:
                labels.append(f"{scheme.area}/{rng.choice(repo.areas)}")
            opened = rng.uniform(5, 220)
            closed = rng.uniform(1, opened - 1) if rng.random() < 0.45 else None
            pool = writers if closed is not None else assignable
            assignees = [rng.choice(pool)] if rng.random() < 0.6 else []
            last = closed if closed is not None else 0.5
            comments = sorted(
                (
                    (rng.choice(writers), rng.uniform(last, opened), rng.choice(REMARKS))
                    for _ in range(rng.randint(0, 3))
                ),
                key=lambda comment: -comment[1],
            )
            poster = rng.choice(reporters)
            world.issues.append(
                Issue(
                    repo.name,
                    titles.take(repo.name, kind),
                    poster,
                    opened,
                    labels=labels,
                    assignees=assignees,
                    comments=comments,
                    closed=closed,
                    closer=(assignees or [rng.choice(writers)])[0] if closed is not None else None,
                )
            )
