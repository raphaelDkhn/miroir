"""What each task family plants in a world before the seed builds it in Gitea.

A family plants what its tasks are about: issues, pull requests, reviews and comments, with
ages that make each task's answer exact. Its plan records which of them a task names. The
world image runs this at bake, through seed.py; the host generator, tasks.py, runs it again to
get the same plans, and `digest` proves the two agree.
"""

from __future__ import annotations

import dataclasses
import hashlib
import itertools
import json
import random
import string
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

import worlds
from worlds import Issue, Pull, Titles, World

INSTANCES = 4
# Miroir's HTTP sink, on the world's loopback: the only place Gitea's webhooks may reach.
SINK = "http://127.0.0.1:9000"


def band_clear(world: World, repo: str, age: float) -> bool:
    """Whether an age of last activity keeps clear of the repository's stale-PR threshold."""
    lo, hi = world.stale_band.get(repo, (0.0, 0.0))
    return not lo <= age <= hi


def quiet_since(world: World, repo: str, rng: random.Random, lo: float, hi: float) -> float:
    """An age of last activity in [lo, hi] that keeps clear of the stale-PR threshold."""
    for _ in range(100):
        age = rng.uniform(lo, hi)
        if band_clear(world, repo, age):
            return age
    raise ValueError(f"{repo}: no age in [{lo}, {hi}] keeps clear of {world.stale_band[repo]}")


def add_issue(world: World, issue: Issue) -> Issue:
    """Add an issue or pull request to the world, refusing one the stale-PR question would
    read as ambiguous."""
    if issue.is_pull and issue.is_open and not band_clear(world, issue.repo, issue.last_active):
        raise ValueError(f"{issue.repo}: '{issue.title}' was last active at its PR threshold")
    world.issues.append(issue)
    return issue


def slug(title: str) -> str:
    """A branch-safe slug of a title."""
    kept = "".join(c if c.isalnum() else "-" for c in title.lower())
    return "-".join(part for part in kept.split("-") if part)[:40]


def pull(
    world: World,
    titles: Titles,
    repo: str,
    author: str,
    opened: float,
    **fields: Any,
) -> Pull:
    """A pull request whose branch adds a changelog fragment, the way the repositories do."""
    title = fields.pop("title", None) or titles.take(repo, "pull")
    branch = f"{len(world.in_repo(repo)) + 1}-{slug(title)}"
    entry = (f"changes/{branch}.md", f"{title}.\n")
    return Pull(repo, title, author, opened, branch=branch, file=entry, **fields)


def reporters(world: World, repo: str) -> list[str]:
    """Who files issues in a repository: its readers, and its customers if it is public."""
    found = world.readers(repo)
    if not world.repos[repo].private:
        found += [p.login for p in world.outsiders("customer")]
    return found or world.writers(repo)


def free_writers(world: World, repo: str) -> list[str]:
    """Writers of a repository who are no plant's leaver."""
    return world.writers(repo, frozenset(world.reserved)) or world.writers(repo)


def kind_label(world: World, kind: str) -> str:
    scheme = world.scheme
    return {"bug": scheme.bug, "feature": scheme.feature, "chore": scheme.chore}[kind]


# stale-prs: which open pull requests have been quiet for more than N days?


@dataclass
class StalePrs:
    repo: str
    days: int


def plant_stale_prs(world: World, rng: random.Random, titles: Titles) -> list[StalePrs]:
    """Quiet, fresh and closed pull requests, and one opened long ago but reviewed lately."""
    plans = []
    repos = rng.sample(list(world.repos), INSTANCES)
    for repo, days in zip(repos, rng.sample([7, 10, 14, 21], INSTANCES), strict=True):
        world.stale_band[repo] = (days - 2, days + 2)
        writers = world.writers(repo)
        for _ in range(rng.randint(2, 3)):
            last = rng.uniform(days + 3, days + 40)
            opened = last + rng.choice([0.0, rng.uniform(1, 12)])
            author = rng.choice(writers)
            remark = [] if opened == last else [(rng.choice(writers), last, "Any update here?")]
            add_issue(world, pull(world, titles, repo, author, opened, comments=remark))
        for _ in range(2):
            last = rng.uniform(0.3, days - 3)
            add_issue(world, pull(world, titles, repo, rng.choice(writers), last))
        author, reviewer = rng.sample(writers, 2)
        opened = rng.uniform(days + 5, days + 40)
        review = (reviewer, "COMMENT", rng.uniform(0.3, days - 3), "Left a few notes inline.")
        add_issue(world, pull(world, titles, repo, author, opened, reviews=[review]))
        opened = rng.uniform(days + 10, days + 60)
        closed = rng.uniform(days + 3, opened - 1)
        closer = rng.choice(writers)
        add_issue(world, pull(world, titles, repo, author, opened, closed=closed, closer=closer))
        plans.append(StalePrs(repo, days))
    return plans


# offboard: hand a leaver's open work to a colleague, then take them off every team.


@dataclass
class Offboard:
    leaver: str
    successor: str


def pick_leavers(world: World, rng: random.Random, candidates: list[str]) -> list[str]:
    """Leavers who leave every repository two writers who stay, for the other plants to use."""
    for _ in range(500):
        leavers = rng.sample(candidates, INSTANCES)
        if all(len(world.writers(r, frozenset(leavers))) >= 2 for r in world.repos):
            return leavers
    raise ValueError(f"{world.name}: no {INSTANCES} leavers leave two writers in every repository")


def handover_repos(
    world: World, rng: random.Random, leaver: str, writable: list[str]
) -> tuple[list[str], list[str]]:
    """Up to three of a leaver's repositories that another engineer can push to, and who can."""
    rng.shuffle(writable)
    engineers = [p.login for p in world.staff() if p.role == "engineer" and p.login != leaver]
    for size in (3, 2):
        for repos in itertools.combinations(writable, size):
            able = [
                e for e in engineers if all(world.access(e, r) in ("write", "admin") for r in repos)
            ]
            if able:
                return list(repos), able
    raise ValueError(f"{world.name}: no engineer can take over {leaver}'s work")


def plant_offboard(world: World, rng: random.Random, titles: Titles) -> list[Offboard]:
    """Leavers with open work across their repositories, some shared, and closed work."""

    def writable(login: str) -> list[str]:
        return [r for r in world.repos if world.access(login, r) in ("write", "admin")]

    candidates = [
        p.login
        for p in world.staff()
        if p.role == "engineer" and len(p.teams) >= 2 and len(writable(p.login)) >= 2
    ]
    leavers = pick_leavers(world, rng, candidates)
    world.reserved += leavers
    plans = []
    for leaver in leavers:
        repos, successors = handover_repos(world, rng, leaver, writable(leaver))
        successor = rng.choice([e for e in successors if e not in world.reserved] or successors)
        for k in range(rng.randint(4, 6)):
            repo = repos[k % len(repos)]
            if k == 0:
                author = rng.choice([w for w in free_writers(world, repo) if w != leaver])
                opened = quiet_since(world, repo, rng, 1, 25)
                add_issue(world, pull(world, titles, repo, author, opened, assignees=[leaver]))
                continue
            kind = rng.choice(["bug", "feature", "chore"])
            assignees = [leaver]
            if k == 1:
                partners = [w for w in world.writers(repo) if w not in (leaver, successor)]
                free = [w for w in partners if w not in world.reserved]
                assignees.append(rng.choice(free or partners))
            issue = Issue(
                repo,
                titles.take(repo, kind),
                rng.choice(world.writers(repo)),
                rng.uniform(3, 90),
                labels=[kind_label(world, kind)],
                assignees=assignees,
            )
            add_issue(world, issue)
        for _ in range(2):
            repo = rng.choice(repos)
            opened = rng.uniform(30, 150)
            add_issue(
                world,
                Issue(
                    repo,
                    titles.take(repo, "bug"),
                    rng.choice(world.writers(repo)),
                    opened,
                    labels=[world.scheme.bug],
                    assignees=[leaver],
                    closed=rng.uniform(2, opened - 1),
                    closer=leaver,
                ),
            )
        plans.append(Offboard(leaver, successor))
    return plans


# count-issues: how many open issues match a label and assignment question?

COUNTS = ("unassigned-top", "highest-any", "assigned-top", "area-unassigned")


@dataclass
class Count:
    repo: str
    variant: str
    assignee: str
    area: str


def count_rows(world: World, plan: Count, rng: random.Random, other: str) -> list[tuple]:
    """(labels, assignees, closed) rows: two to five matches, then the near misses, each one
    field off from a match: assigned, closed, another kind or priority or area."""
    scheme, writers = world.scheme, free_writers(world, plan.repo)
    bug, feature = scheme.bug, scheme.feature
    p0, p1, p2, _ = scheme.priorities
    areas = [f"{scheme.area}/{a}" for a in world.repos[plan.repo].areas]
    areas.remove(plan.area)
    rows = []
    for _ in range(rng.randint(2, 5)):
        rows.append((*count_match(world, plan, rng, writers), False))
    rows += {
        "unassigned-top": [
            ([bug, p1], [other], False),
            ([bug, p0], [], True),
            ([feature, p1], [], False),
        ],
        "highest-any": [
            ([bug, p1], [], False),
            ([bug, p0], [other], True),
            ([feature, p0], [], False),
        ],
        "assigned-top": [
            ([bug, p1], [other], False),
            ([bug, p0], [plan.assignee], True),
            ([bug, p2], [plan.assignee], False),
        ],
        "area-unassigned": [
            ([bug, plan.area], [other], False),
            ([feature, plan.area], [], False),
            ([bug, rng.choice(areas)], [], False),
        ],
    }[plan.variant]
    return rows


def count_match(
    world: World, plan: Count, rng: random.Random, writers: list[str]
) -> tuple[list[str], list[str]]:
    """The labels and assignees of an issue the plan's question counts."""
    scheme = world.scheme
    top = rng.choice(scheme.priorities[:2])
    if plan.variant == "unassigned-top":
        return [scheme.bug, top], []
    if plan.variant == "highest-any":
        return [scheme.bug, scheme.priorities[0]], rng.choice([[], [rng.choice(writers)]])
    if plan.variant == "assigned-top":
        return [scheme.bug, top], [plan.assignee]
    return [scheme.bug, plan.area], []


def plant_count(world: World, rng: random.Random, titles: Titles) -> list[Count]:
    """Matching issues and their near misses, and a matching pull request."""
    code = [r for r, repo in world.repos.items() if repo.kind in ("service", "web", "mobile")]
    plans = []
    for repo, variant in zip(rng.sample(code, INSTANCES), COUNTS, strict=True):
        writers = free_writers(world, repo)
        assignee, other = rng.sample(writers, 2)
        area = f"{world.scheme.area}/{rng.choice(world.repos[repo].areas)}"
        plan = Count(repo, variant, assignee, area)
        for labels, assignees, closed in count_rows(world, plan, rng, other):
            opened = rng.uniform(2, 60)
            issue = Issue(
                repo,
                titles.take(repo, "bug"),
                rng.choice(reporters(world, repo)),
                opened,
                labels=labels,
                assignees=assignees,
                closed=rng.uniform(0.5, opened - 0.5) if closed else None,
                closer=other if closed else None,
            )
            add_issue(world, issue)
        labels, assignees = count_match(world, plan, rng, writers)
        opened = quiet_since(world, repo, rng, 1, 30)
        add_issue(
            world,
            pull(
                world, titles, repo, rng.choice(writers), opened, labels=labels, assignees=assignees
            ),
        )
        plans.append(plan)
    return plans


# triage: label a new issue, set its milestone and assign its owner per CODEOWNERS.

REPORTS = {
    "service": "{who} reported it since yesterday. The service answers 500, and the log "
    "shows\n\n```\nERROR unhandled exception\n    at {path}:{line}\n```",
    "web": "Seen by two accounts in production. The browser console shows\n\n```\nTypeError: "
    "Cannot read properties of undefined (reading 'id')\n    at {path}:{line}\n```",
    "mobile": "Reported from the field. The crash log shows\n\n```\nFatal exception: "
    "IllegalStateException\n    at {path}:{line}\n```",
    "infra": "Seen in the last two applies:\n\n```\nError: Provider produced inconsistent "
    "result\n  on {path} line {line}\n```",
    "docs": "A reader reports a mistake in {path}, around line {line}.",
}


@dataclass
class Triage:
    issue: Issue
    owner: str
    path: str
    priority: str


def plant_triage(world: World, rng: random.Random, titles: Titles) -> list[Triage]:
    """A new report whose stack trace points at a path an owner other than the catch-all owns."""
    plans = []
    repos = [r for r, repo in world.repos.items() if repo.release]
    for repo in rng.sample(repos, INSTANCES):
        found = world.repos[repo]
        pattern, owner = rng.choice(found.owners[1:])
        path = rng.choice([p for p in found.files if p.startswith(pattern.removesuffix(".*"))])
        who = rng.choice(["Two customers have", "Three customers have", "A customer has"])
        body = REPORTS[found.kind].format(who=who, path=path, line=rng.randint(30, 240))
        issue = Issue(
            repo,
            titles.take(repo, "bug"),
            rng.choice(reporters(world, repo)),
            rng.uniform(0.05, 0.6),
            body,
            labels=[world.scheme.needs_triage],
        )
        plans.append(
            Triage(add_issue(world, issue), owner, path, rng.choice(world.scheme.priorities[:3]))
        )
    return plans


# close-stale: close the needs-info issues quiet for N days, each with a comment.

QUESTIONS = [
    "Which accounts does this affect? We can't reproduce it.",
    "Can you share the request IDs of a few failed calls?",
    "Which version are you on?",
    "Do you have the exact steps to reproduce it?",
    "Can you attach the full log?",
    "Does it still happen after clearing the cache?",
]


@dataclass
class Stale:
    repo: str
    days: int
    stale: list[Issue]
    fresh: list[Issue]
    elsewhere: Issue | None = None


def waiting(
    world: World,
    rng: random.Random,
    titles: Titles,
    repo: str,
    *,
    asked: float,
    opened: float,
    closed: float | None = None,
) -> Issue:
    """An issue labeled as waiting on its reporter since a writer asked them something."""
    scheme, writers = world.scheme, world.writers(repo)
    comments = [(rng.choice(writers), asked, rng.choice(QUESTIONS))]
    issue = Issue(
        repo,
        titles.take(repo, "bug"),
        rng.choice(reporters(world, repo)),
        opened,
        labels=[scheme.bug, scheme.needs_info],
        comments=comments,
        closed=closed,
        closer=rng.choice(writers) if closed else None,
    )
    return add_issue(world, issue)


def plant_close_stale(world: World, rng: random.Random, titles: Titles) -> list[Stale]:
    """Issues waiting on their reporters: long quiet ones, recent ones and a closed one."""
    plans = []
    repos = rng.sample(list(world.repos), INSTANCES)
    for repo, days in zip(repos, rng.sample([21, 30, 45, 60], INSTANCES), strict=True):
        stale, fresh = [], []
        for _ in range(rng.randint(3, 4)):
            asked = rng.uniform(days + 3, days + 60)
            opened = asked + rng.uniform(1, 30)
            stale.append(waiting(world, rng, titles, repo, asked=asked, opened=opened))
        for _ in range(2):
            asked = rng.uniform(1, days - 4)
            opened = asked + rng.uniform(1, 20)
            fresh.append(waiting(world, rng, titles, repo, asked=asked, opened=opened))
        asked = rng.uniform(days + 20, days + 90)
        opened, closed = asked + rng.uniform(1, 20), rng.uniform(2, asked - 1)
        waiting(world, rng, titles, repo, asked=asked, opened=opened, closed=closed)
        plans.append(Stale(repo, days, stale, fresh))
    for i, plan in enumerate(plans):
        plan.elsewhere = plans[(i + 1) % len(plans)].stale[0]
    return plans


# grant-access: a contractor gets one repository, and no team that would open more.


@dataclass
class Grant:
    contractor: str
    repo: str
    permission: str
    team: str


def plant_grant(world: World, rng: random.Random, titles: Titles) -> list[Grant]:
    """Nothing to plant: a contractor per task, and a team that reaches the repository and more."""
    del titles

    def wide_teams(repo: str) -> list[str]:
        return [
            t.name
            for t in world.teams.values()
            if repo in world.team_repos(t) and t.permission != "read" and len(t.repos) > 1
        ]

    repos = [r for r, repo in world.repos.items() if repo.private and wide_teams(r)]
    permissions = ["write", "write", "write", "read"]
    rng.shuffle(permissions)
    contractors = [p.login for p in world.outsiders("contractor")]
    return [
        Grant(c, r, p, rng.choice(wide_teams(r)))
        for c, r, p in zip(contractors, rng.sample(repos, INSTANCES), permissions, strict=True)
    ]


# add-webhook: a CI webhook with exactly the events asked, then a test delivery.

EVENT_SETS = [
    ("push", "pull_request"),
    ("push",),
    ("push", "release"),
    ("push", "create", "delete"),
]


@dataclass
class Hook:
    repo: str
    events: tuple[str, ...]
    secret: str


def plant_hooks(world: World, rng: random.Random, titles: Titles) -> list[Hook]:
    """Nothing to plant: a repository, the events and a secret per task."""
    del titles
    sets = list(EVENT_SETS)
    rng.shuffle(sets)
    alphabet = string.ascii_lowercase + string.digits
    return [
        Hook(repo, events, f"{repo}-ci-{''.join(rng.choices(alphabet, k=8))}")
        for repo, events in zip(rng.sample(list(world.repos), INSTANCES), sets, strict=True)
    ]


# merge-ready: merge what is approved, with no change request standing, and not a draft.

APPROVALS = ["Looks good.", "LGTM", "Tested it locally; works for me.", "Nice, thanks!"]
OBJECTIONS = [
    "QA: this breaks the empty state.",
    "This regresses the dark theme; please fix before merging.",
    "Please split out the client change.",
    "Missing a test for the error path.",
]


# The pull request ready to merge in each of the four repositories, if any: every pair of them
# that a task names has one or two. Gitea re-checks every open pull request of a repository in
# the background once one merges, and refuses a merge that comes before the check ends ("Please
# try again later"), so a repository with two ready would make the reward depend on timing.
READY = (None, "approved", "re-approved", None)


def plant_merge(world: World, rng: random.Random, titles: Titles) -> list[tuple[str, str]]:
    """Pull requests in every review state, in four repositories that pair up two by two."""
    code = [
        r
        for r, repo in world.repos.items()
        if repo.kind in ("service", "web", "mobile") and len(world.writers(r)) >= 3
    ]
    repos = rng.sample(code, 4)
    for repo, ready in zip(repos, READY, strict=True):
        writers = world.writers(repo)
        cases = ["overruled", "withdrawn", "draft"]
        cases += rng.sample(["comment", "quiet"], rng.randint(1, 2)) + ([ready] if ready else [])
        rng.shuffle(cases)
        for case in cases:
            author, first, second = rng.sample(writers, 3)
            objector = rng.choice([*world.readers(repo), second])
            last = quiet_since(world, repo, rng, 0.5, 20)
            earlier = last + rng.uniform(1, 6)
            opened = earlier + rng.uniform(0.5, 6)
            approve = (first, "APPROVED", earlier, rng.choice(APPROVALS))
            reviews = {
                "approved": [(first, "APPROVED", last, rng.choice(APPROVALS))],
                "re-approved": [
                    (first, "REQUEST_CHANGES", earlier, rng.choice(OBJECTIONS)),
                    (first, "APPROVED", last, "Thanks, all good now."),
                ],
                "overruled": [approve, (objector, "REQUEST_CHANGES", last, rng.choice(OBJECTIONS))],
                "withdrawn": [approve, (first, "REQUEST_CHANGES", last, rng.choice(OBJECTIONS))],
                "draft": [(first, "APPROVED", last, "Early look: the direction is good.")],
                "comment": [(first, "COMMENT", last, "Should this cover the error path too?")],
                "quiet": [],
            }[case]
            title = titles.take(repo, "pull")
            if case == "draft":
                title = f"WIP: {title}"
            if case == "quiet":
                opened = last
            add_issue(
                world, pull(world, titles, repo, author, opened, title=title, reviews=reviews)
            )
    return [(repos[0], repos[1]), (repos[2], repos[3]), (repos[0], repos[2]), (repos[1], repos[3])]


# releases: every repository with a release cadence has shipped before, with a deploy hook.


def plant_releases(world: World, rng: random.Random, titles: Titles) -> None:
    """Earlier releases, each with the issues its notes list, and the deploy pipeline's hooks."""
    for repo, found in world.repos.items():
        if found.release is None:
            continue
        writers = world.writers(repo)
        for tag, age in found.release.shipped:
            issues = []
            for _ in range(rng.randint(2, 4)):
                closed = age + rng.uniform(0.5, 12)
                opened = closed + rng.uniform(1, 40)
                kind = rng.choice(["bug", "feature"])
                closer = rng.choice(writers)
                issues.append(
                    add_issue(
                        world,
                        Issue(
                            repo,
                            titles.take(repo, kind),
                            closer,
                            opened,
                            labels=[kind_label(world, kind)],
                            assignees=[closer],
                            closed=closed,
                            closer=closer,
                        ),
                    )
                )
            world.releases.append((repo, tag, age, issues))
        world.hooks.append((repo, f"/deploy/{repo}", ["release"]))


# ship-release: publish a release whose notes list the milestone's closed issues.


def plant_ship(world: World, rng: random.Random, titles: Titles) -> list[str]:
    """A release milestone's closed issues, and its open ones to move to the next."""
    plans = []
    repos = [r for r, repo in world.repos.items() if repo.release]
    for repo in rng.sample(repos, INSTANCES):
        release = world.repos[repo].release
        assert release is not None
        latest = min(age for _, age in release.shipped)
        writers = world.writers(repo)
        for done in [True] * rng.randint(4, 6) + [False] * rng.randint(2, 3):
            kind = rng.choice(["bug", "feature", "chore"])
            who = rng.choice(writers if done else free_writers(world, repo))
            closed = rng.uniform(1, latest - 1) if done else None
            opened = (closed or 0) + rng.uniform(2, 40)
            add_issue(
                world,
                Issue(
                    repo,
                    titles.take(repo, kind),
                    who,
                    opened,
                    labels=[kind_label(world, kind)],
                    assignees=[who],
                    milestone=release.milestone,
                    closed=closed,
                    closer=who if done else None,
                ),
            )
        plans.append(repo)
    return plans


# redact-secret: redact a live key in a comment, lock the issue, ask security to rotate it.

SECRETS = [
    ("ACQUIRER_API_KEY", "acquirer API key", "acq_live_", 26),
    ("STRIPE_SECRET_KEY", "Stripe secret key", "sk_live_", 24),
    ("SENDGRID_API_KEY", "SendGrid API key", "SG.", 40),
    ("SLACK_BOT_TOKEN", "Slack bot token", "xoxb-", 32),
    ("AWS_SECRET_ACCESS_KEY", "AWS secret access key", "", 40),
]
LOG = """Logs from my local run against staging:

```
level=info msg="starting {repo}" env=staging
{name}={value}
level=info msg="POST /v1/{path}" status=500 duration={ms}ms
level=error msg="upstream timeout" after=300ms
```

The upstream call gives up after 300 ms, sooner than the provider's own timeout."""


@dataclass
class Redact:
    issue: Issue
    name: str
    what: str
    value: str
    status_line: str
    poster: str


def plant_redact(world: World, rng: random.Random, titles: Titles) -> list[Redact]:
    """An issue whose first comment pastes a log with a live key in it."""
    plans = []
    code = [r for r, repo in world.repos.items() if repo.kind != "docs"]
    alphabet = string.ascii_letters + string.digits
    for repo, (name, what, prefix, size) in zip(
        rng.sample(code, INSTANCES), rng.sample(SECRETS, INSTANCES), strict=True
    ):
        reporter, paster, helper = rng.sample(world.writers(repo), 3)
        value = prefix + "".join(rng.choices(alphabet, k=size))
        noun = rng.choice(world.repos[repo].nouns)
        ms = rng.randint(301, 900)
        log = LOG.format(repo=repo, name=name, value=value, path=noun.many.replace(" ", "_"), ms=ms)
        opened = rng.uniform(2, 6)
        pasted = rng.uniform(0.5, opened - 0.5)
        comments = [
            (paster, pasted, log),
            (helper, pasted * 0.7, f"Thanks {world.people[paster].first}, looking at the timeout."),
        ]
        issue = Issue(
            repo,
            titles.take(repo, "bug"),
            reporter,
            opened,
            "It fails every time on staging since the last deploy.",
            labels=[world.scheme.bug],
            comments=comments,
        )
        line = f'level=info msg="POST /v1/{noun.many.replace(" ", "_")}" status=500 duration={ms}ms'
        plans.append(Redact(add_issue(world, issue), name, what, value, line, paster))
    return plans


# The families' plants, in planting order, after the earlier releases: stale-prs sets the ages
# other pull requests keep clear of, and offboard reserves its leavers before anyone assigns.
PLANTS: dict[str, Callable[[World, random.Random, Titles], list[Any]]] = {
    "stale-prs": plant_stale_prs,
    "offboard": plant_offboard,
    "count-issues": plant_count,
    "triage": plant_triage,
    "close-stale": plant_close_stale,
    "grant-access": plant_grant,
    "add-webhook": plant_hooks,
    "merge-ready": plant_merge,
    "ship-release": plant_ship,
    "redact-secret": plant_redact,
}


def plant(world: World, rng: random.Random, titles: Titles) -> dict[str, list[Any]]:
    """Every family's plans for a world, by family, in planting order."""
    plant_releases(world, rng, titles)
    return {name: plant_family(world, rng, titles) for name, plant_family in PLANTS.items()}


def build(name: str) -> tuple[World, dict[str, list[Any]]]:
    """A world and its families' plans, before anything is in Gitea: the same on every call."""
    world = worlds.WORLDS[name]()
    rng = random.Random(world.seed)
    titles = Titles(world, rng)
    plans = plant(world, rng, titles)
    worlds.background(world, rng, titles)
    world.finish()
    return world, plans


def digest(world: World, plans: dict[str, list[Any]]) -> str:
    """The SHA-256 of a world and its plans, which the seed writes to ids.json. A generator that
    plants differently from the image, with other plant code or another Python, gets another."""
    text = json.dumps([world, plans], sort_keys=True, default=fields)
    return f"sha256:{hashlib.sha256(text.encode()).hexdigest()}"


def fields(value: object) -> dict[str, Any]:
    """A dataclass's fields, for `digest`. Anything else JSON can't hold, a set above all, has
    no stable order between processes, so it fails here rather than hashing differently."""
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: getattr(value, f.name) for f in dataclasses.fields(value)}
    raise TypeError(f"{type(value).__name__} has no stable JSON form: use a list or a dataclass")
