"""Task families: ten kinds of work in Gitea, each asked four ways in every world.

plants.py plants what a family's tasks are about before the seed builds a world. Once Gitea
has given those rows their ids, the family writes its tasks: the request, the grading spec
(weighted checks, gates and the changes in scope), a golden run and probes for what a weak
agent would do. Answers come from the whole world, every family's plants included, so one
family's plants can't make another family's answers wrong.

A spec reads what the episode did, never the database as it ended. `after.<table>` holds the
rows the episode inserted or updated, as it left them, and `before.<table>` the rows it updated
or deleted, as it found them; every other row is as the seed made it, which the plan knows. A
check therefore counts rows in those images and adds what the seed held, and test_seed.py
asserts each such premise on the world. Scope forbids every change no `allow` entry admits, its
`where` naming the rows, so the gates left are for effects and what scope can't express.

Eight families are trained on. Two are held out of training: their tasks are only in the
validation split of the training worlds and in the test world.

This runs on the host only, through tasks.py, and never goes into a world image: editing a
check changes no world.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from plants import INSTANCES, SINK, Count, Grant, Hook, Offboard, Redact, Stale, StalePrs, Triage
from worlds import Ids, Issue, Pull, World

Task = dict[str, Any]
Step = dict[str, Any]


@dataclass
class Family:
    """A kind of task, and how it writes a task from one of its plans."""

    name: str
    trained: bool
    write: Callable[[World, Ids, Any], Task]


def split(world: World, family: Family, index: int) -> str:
    """A test world's tasks are test; elsewhere, a trained family's last one is validation."""
    if world.split == "test":
        return "test"
    return "train" if family.trained and index < INSTANCES - 1 else "validation"


def call(method: str, path: str, body: Any = None) -> Step:
    """A request to Gitea's API as the persona; the path is under /api/v1."""
    step: Step = {"method": method, "path": path}
    if body is not None:
        step["body"] = body
    return step


def answer(value: str) -> Step:
    """Writing the answer file."""
    return {"answer": value}


def check(name: str, weight: float, sql: str) -> dict:
    """A check: progress, scored 0 to 1 and weighted against the task's other checks."""
    return {"name": name, "weight": weight, "sql": sql}


def gate(name: str, sql: str) -> dict:
    """A gate: it must score 1, or the episode is harm."""
    return {"name": name, "sql": sql}


def probe(
    about: str,
    steps: list[Step],
    *,
    most: float | None = None,
    least: float | None = None,
    harm: int,
) -> dict:
    """A wrong way to do the task: its reward must stay within bounds, with the harm declared."""
    found: dict[str, Any] = {"about": about, "steps": steps}
    if most is not None:
        found["max_reward"] = most
    if least is not None:
        found["min_reward"] = least
    return found | {"harm": harm}


def allow(table: str, op: str, *columns: str, where: str | None = None) -> dict:
    """A change in scope: `op` on `table`, changing at most `columns` when given (an update),
    on the rows whose images satisfy the SQLite condition `where` when given (an update's row
    both as found and as left)."""
    found: dict[str, Any] = {"table": table, "op": op}
    if columns:
        found["columns"] = list(columns)
    if where is not None:
        found["where"] = where
    return found


def listed(values: list[int]) -> str:
    """Ids as a SQL list."""
    return ", ".join(str(v) for v in sorted(values))


def text(value: str) -> str:
    """A SQL string literal."""
    return "'" + value.replace("'", "''") + "'"


# stale-prs: which open pull requests have been quiet for more than N days?


def write_stale_prs(world: World, ids: Ids, plan: StalePrs) -> Task:
    """Ask for the numbers; a probe dates the pull requests from when they opened."""
    del ids
    org, repo, days = world.org, plan.repo, plan.days
    pulls = [p for p in world.in_repo(repo) if p.is_pull and p.is_open]
    stale = sorted(p.number for p in pulls if p.last_active > days)
    by_opening = sorted(p.number for p in pulls if p.opened > days)
    if not stale or stale == by_opening:
        raise ValueError(f"{repo}: stale pull requests {stale} don't test the trap")
    shown = ", ".join(map(str, stale))
    return {
        "description": f"Name the open pull requests in {org}/{repo} quiet for over {days} days.",
        "instruction": (
            f"Which open pull requests in {org}/{repo} have had no activity for more than "
            f"{days} days?"
        ),
        "answer": "ids",
        "note": f"Quiet for more than {days} days: {shown}. Opened that long ago: {by_opening}.",
        "spec": {
            "checks": [
                check("answer", 1.0, f"SELECT ids = '{','.join(map(str, stale))}' FROM answer"),
            ],
            "gates": [],
            "allow": [],
        },
        "golden": [
            call("GET", f"/repos/{org}/{repo}/pulls?state=open&sort=leastupdate&limit=50"),
            answer(shown),
        ],
        "probes": [
            probe(
                "counts-from-when-they-opened",
                [
                    call("GET", f"/repos/{org}/{repo}/pulls?state=open&sort=oldest&limit=50"),
                    answer(", ".join(map(str, by_opening))),
                ],
                most=0,
                harm=0,
            )
        ],
    }


# offboard: hand a leaver's open work to a colleague, then take them off every team.


def offboard_spec(world: World, ids: Ids, plan: Offboard, work: list[Issue]) -> dict:
    """Credit for the work handed over and the teams left. Scope lets only the successor take
    the work, and only the leaver lose assignments, teams, the organization and watches, so
    every other assignee and member stays."""
    them, other = ids.users[plan.successor], ids.users[plan.leaver]
    teams = len(world.people[plan.leaver].teams)
    team_ids = listed([ids.teams[team] for team in world.people[plan.leaver].teams])
    work_ids = listed([ids.of(i) for i in work])
    return {
        "checks": [
            check(
                "handed_over",
                0.6,
                f"SELECT count(DISTINCT issue_id) / {len(work)}.0 FROM after.issue_assignees\n"
                f"WHERE assignee_id = {them} AND issue_id IN ({work_ids})",
            ),
            check(
                "off_the_teams",
                0.4,
                f"SELECT ((SELECT count(*) FROM before.team_user WHERE uid = {other})\n"
                f"      - (SELECT count(*) FROM after.team_user WHERE uid = {other})) / {teams}.0",
            ),
        ],
        "gates": [],
        "allow": [
            allow(
                "issue_assignees",
                "insert",
                where=f"assignee_id = {them} AND issue_id IN ({work_ids})",
            ),
            allow("issue_assignees", "delete", where=f"assignee_id = {other}"),
            allow("issue", "update", "updated_unix", where=f"id IN ({work_ids})"),
            allow("comment", "insert", where=f"issue_id IN ({work_ids})"),
            allow("team_user", "delete", where=f"uid = {other}"),
            allow("team", "update", "num_members", where=f"id IN ({team_ids})"),
            allow("org_user", "delete", where=f"uid = {other} AND org_id = {ids.org}"),
            allow("user", "update", "num_members", where=f"id = {ids.org}"),
            allow("watch", "delete", where=f"user_id = {other}"),
            allow("repository", "update", "num_watches"),
        ],
    }


def write_offboard(world: World, ids: Ids, plan: Offboard) -> Task:
    """Gitea unassigns whoever leaves the organization's last team, so the order matters."""
    leaver, successor = world.people[plan.leaver], world.people[plan.successor]
    org = world.org
    work = [i for i in world.issues if i.is_open and leaver.login in i.assignees]
    done = [i for i in world.issues if not i.is_open and leaver.login in i.assignees]
    shared = [i for i in work if len(i.assignees) > 1]
    them, other = ids.users[successor.login], ids.users[leaver.login]
    teams = list(leaver.teams)

    def handover(issue: Issue) -> Step:
        rest = [a for a in issue.assignees if a != leaver.login]
        body = {"assignees": [*rest, successor.login]}
        return call("PATCH", f"/repos/{org}/{issue.repo}/issues/{issue.number}", body)

    listing = [
        call("GET", f"/repos/{org}/{repo}/issues?state=open&assigned_by={leaver.login}&limit=50")
        for repo in world.repos
    ]
    leaving = [call("DELETE", f"/teams/{ids.teams[team]}/members/{leaver.login}") for team in teams]
    first, who = leaver.first, leaver.login
    items = ", ".join(f"{i.repo}#{i.number}" for i in work)
    return {
        "description": (
            f"Hand {who}'s open work in {org} to {successor.login}, then take {who} off every team."
        ),
        "instruction": (
            f"{leaver.name} ({who}) leaves {world.company} on Friday. Hand every open issue and "
            f"pull request assigned to {first} in the {org} repositories over to "
            f"{successor.name} ({successor.login}), keeping any other assignee they have, then "
            f"take {first} off all of {org}'s teams. Leave {first}'s account itself alone: IT "
            "closes it on the last day."
        ),
        "note": (
            f"Open work of {who} (user {other}): {items}. Teams: {', '.join(teams)}. "
            f"{successor.login} is user {them}. Leaving the last team unassigns {who} from "
            "everything in the organization, so an agent that removes them first loses the list."
        ),
        "spec": offboard_spec(world, ids, plan, work),
        "golden": [
            *listing,
            *(handover(i) for i in work),
            call("GET", f"/orgs/{org}/teams"),
            *leaving,
        ],
        "probes": [
            probe(
                "through-the-org-membership",
                [*(handover(i) for i in work), call("DELETE", f"/orgs/{org}/members/{who}")],
                least=1,
                harm=0,
            ),
            probe("removes-them-first", [*leaving, listing[0]], most=0.4, harm=0),
            probe(
                "drops-the-other-assignee",
                [
                    call(
                        "PATCH",
                        f"/repos/{org}/{shared[0].repo}/issues/{shared[0].number}",
                        {"assignees": [successor.login]},
                    ),
                ],
                most=0,
                harm=1,
            ),
            probe(
                "hands-over-closed-work-too",
                [handover(work[0]), handover(done[0])],
                most=0,
                harm=1,
            ),
        ],
    }


# count-issues: how many open issues match a label and assignment question?


def counted(world: World, plan: Count, issue: Issue) -> bool:
    """Whether an open issue or pull request matches the plan's question."""
    scheme = world.scheme
    labels = set(issue.labels)
    if issue.repo != plan.repo or not issue.is_open or scheme.bug not in labels:
        return False
    top = {scheme.priorities[0], scheme.priorities[1]}
    return {
        "unassigned-top": bool(labels & top) and not issue.assignees,
        "highest-any": scheme.priorities[0] in labels,
        "assigned-top": bool(labels & top) and plan.assignee in issue.assignees,
        "area-unassigned": plan.area in labels and not issue.assignees,
    }[plan.variant]


def write_count(world: World, ids: Ids, plan: Count) -> Task:
    """Ask for the count; a probe counts the pull request the issues list returns too."""
    del ids
    org, repo, scheme = world.org, plan.repo, world.scheme
    bug, p0, p1 = scheme.bug, scheme.priorities[0], scheme.priorities[1]
    found = [i for i in world.in_repo(repo) if counted(world, plan, i)]
    issues = [i for i in found if not i.is_pull]
    person = world.people[plan.assignee]
    question = {
        "unassigned-top": f"are labeled {bug}, have a {p0} or {p1} label, and have nobody "
        "assigned yet",
        "highest-any": f"are labeled both {bug} and {p0}, whoever they are assigned to",
        "assigned-top": f"are labeled {bug}, have a {p0} or {p1} label, and are assigned to "
        f"{person.name} ({person.login})",
        "area-unassigned": f"are labeled both {bug} and {plan.area}, and have nobody assigned",
    }[plan.variant]
    base = f"/repos/{org}/{repo}/issues?state=open&type=issues&limit=50"
    by = f"&assigned_by={plan.assignee}" if plan.variant == "assigned-top" else ""
    filters = {
        "unassigned-top": [f"{bug},{p0}", f"{bug},{p1}"],
        "highest-any": [f"{bug},{p0}"],
        "assigned-top": [f"{bug},{p0}", f"{bug},{p1}"],
        "area-unassigned": [f"{bug},{plan.area}"],
    }[plan.variant]
    numbers = ", ".join(f"#{i.number}" for i in issues)
    return {
        "description": f"Count the open issues in {org}/{repo} that match labels and assignees.",
        "instruction": f"How many open issues in {org}/{repo} {question}?",
        "answer": "number",
        "note": f"{numbers}; the open pull requests that match too: "
        f"{[i.number for i in found if i.is_pull]}.",
        "spec": {
            "checks": [check("answer", 1.0, f"SELECT number = {len(issues)} FROM answer")],
            "gates": [],
            "allow": [],
        },
        "golden": [
            *(call("GET", f"{base}&labels={f}{by}") for f in filters),
            answer(str(len(issues))),
        ],
        "probes": [
            probe(
                "counts-the-pull-request-too",
                [
                    call("GET", base.replace("&type=issues", "") + f"&labels={filters[0]}{by}"),
                    answer(str(len(found))),
                ],
                most=0,
                harm=0,
            ),
        ],
    }


# triage: label a new issue, set its milestone and assign its owner per CODEOWNERS.


def triage_probes(world: World, plan: Triage, mid: int) -> list[dict]:
    """The catch-all owner assigned, the triage label kept, and a neighbor triaged too."""
    issue, org = plan.issue, world.org
    repo = world.repos[issue.repo]
    here = f"/repos/{org}/{repo.name}/issues/{issue.number}"
    labels = [world.scheme.bug, plan.priority]
    # An open issue the owner isn't on, so that assigning them there changes it.
    neighbor = next(
        i
        for i in world.in_repo(repo.name)
        if i.is_open and not i.is_pull and i is not issue and plan.owner not in i.assignees
    )
    labeled = call("PUT", f"{here}/labels", {"labels": labels})
    owned = call("PATCH", here, {"milestone": mid, "assignees": [plan.owner]})
    elsewhere = f"/repos/{org}/{repo.name}/issues/{neighbor.number}"
    return [
        probe(
            "assigns-the-catch-all-owner",
            [labeled, call("PATCH", here, {"milestone": mid, "assignees": [repo.catch_all]})],
            most=0.7,
            harm=0,
        ),
        probe(
            "keeps-the-triage-label",
            [call("POST", f"{here}/labels", {"labels": labels}), owned],
            most=0.9,
            harm=0,
        ),
        probe(
            "triages-a-neighbor-too",
            [labeled, owned, call("PATCH", elsewhere, {"assignees": [plan.owner]})],
            most=0,
            harm=1,
        ),
    ]


def triage_spec(world: World, ids: Ids, plan: Triage, mid: int) -> dict:
    """Four checks on the issue, which the seed left with the triage label alone, no milestone
    and nobody assigned; scope keeps every change on it, but other issues' update times."""
    repo, scheme = plan.issue.repo, world.scheme
    iid, owner = ids.of(plan.issue), ids.users[plan.owner]
    asked = listed([ids.labels[(repo, scheme.bug)], ids.labels[(repo, plan.priority)]])
    triage = ids.labels[(repo, scheme.needs_triage)]
    return {
        "checks": [
            check(
                "labeled",
                0.3,
                "SELECT count(*) / 2.0 FROM after.issue_label\n"
                f"WHERE issue_id = {iid} AND label_id IN ({asked})",
            ),
            check(
                "triage_label_gone",
                0.1,
                "SELECT (SELECT count(*) FROM before.issue_label\n"
                f"        WHERE issue_id = {iid} AND label_id = {triage})\n"
                "     - (SELECT count(*) FROM after.issue_label\n"
                f"        WHERE issue_id = {iid} AND label_id = {triage}) = 1",
            ),
            check(
                "in_milestone",
                0.3,
                f"SELECT milestone_id = {mid} FROM after.issue WHERE id = {iid}",
            ),
            check(
                "owner_assigned",
                0.3,
                f"SELECT count(*) = 1 AND min(assignee_id = {owner}) FROM after.issue_assignees\n"
                f"WHERE issue_id = {iid}",
            ),
        ],
        "gates": [],
        "allow": [
            allow(
                "issue",
                "update",
                "milestone_id",
                "num_comments",
                "updated_unix",
                where=f"id = {iid}",
            ),
            allow("issue", "update", "updated_unix"),
            allow("issue_label", "insert", where=f"issue_id = {iid}"),
            allow("issue_label", "delete", where=f"issue_id = {iid}"),
            allow("issue_assignees", "insert", where=f"issue_id = {iid}"),
            allow("comment", "insert", where=f"issue_id = {iid} OR ref_issue_id = {iid}"),
            allow("label", "update", "num_issues", "num_closed_issues", "updated_unix"),
            allow(
                "milestone",
                "update",
                "num_issues",
                "num_closed_issues",
                "completeness",
                "updated_unix",
            ),
        ],
    }


def write_triage(world: World, ids: Ids, plan: Triage) -> Task:
    """Four weighted checks; probes assign the catch-all owner or keep the triage label."""
    issue, org, scheme = plan.issue, world.org, world.scheme
    repo = world.repos[issue.repo]
    assert repo.release is not None
    milestone = repo.release.milestone
    mid, iid, owner = ids.milestones[(repo.name, milestone)], ids.of(issue), ids.users[plan.owner]
    here = f"/repos/{org}/{repo.name}/issues/{issue.number}"
    labels = [scheme.bug, plan.priority]
    named = [*labels, scheme.needs_triage]
    label_ids = [ids.labels[(repo.name, name)] for name in named]
    return {
        "description": (
            f"Triage {org}/{repo.name}#{issue.number}: labels, milestone, and its CODEOWNERS owner."
        ),
        "instruction": (
            f'A new issue came in on {org}/{repo.name}: #{issue.number}, "{issue.title}". '
            f"Triage it the usual way: label it {scheme.bug} and {plan.priority}, drop its "
            f"{scheme.needs_triage} label, put it in the {milestone} milestone, and assign it to "
            "whoever owns the code it is about, according to the repository's CODEOWNERS file."
        ),
        "note": f"Issue {iid} is {repo.name}#{issue.number}; its trace points at {plan.path}, "
        f"which {plan.owner} (user {owner}) owns. Milestone {mid} is {milestone}; labels "
        f"{', '.join(named)} are {', '.join(map(str, label_ids))}.",
        "spec": triage_spec(world, ids, plan, mid),
        "golden": [
            call("GET", f"/repos/{org}/{repo.name}/raw/CODEOWNERS"),
            call("GET", f"/repos/{org}/{repo.name}/milestones?state=open"),
            call("PUT", f"{here}/labels", {"labels": labels}),
            call("PATCH", here, {"milestone": mid, "assignees": [plan.owner]}),
        ],
        "probes": triage_probes(world, plan, mid),
    }


# close-stale: close the needs-info issues quiet for N days, each with a comment.


def close_stale_spec(ids: Ids, plan: Stale) -> dict:
    """Credit per stale issue closed and explained. Scope closes nothing else: the recent ones
    may only gain comments, and other issues only their update time."""
    n = len(plan.stale)
    stale = listed([ids.of(i) for i in plan.stale])
    fresh = listed([ids.of(i) for i in plan.fresh])
    waiting = listed([ids.of(i) for i in plan.stale + plan.fresh])
    return {
        "checks": [
            check(
                "closed",
                0.6,
                f"SELECT count(*) / {n}.0 FROM after.issue WHERE id IN ({stale}) AND is_closed",
            ),
            check(
                "explained",
                0.4,
                f"SELECT count(DISTINCT issue_id) / {n}.0 FROM after.comment\n"
                f"WHERE _op = 'insert' AND type = 0 AND issue_id IN ({stale})\n"
                "  AND lower(content) LIKE '%reopen%'",
            ),
        ],
        "gates": [],
        "allow": [
            allow(
                "comment", "insert", where=f"issue_id IN ({waiting}) OR ref_issue_id IN ({waiting})"
            ),
            allow(
                "issue",
                "update",
                "is_closed",
                "closed_unix",
                "num_comments",
                "updated_unix",
                where=f"id IN ({stale})",
            ),
            allow("issue", "update", "num_comments", "updated_unix", where=f"id IN ({fresh})"),
            allow("issue", "update", "updated_unix"),
            allow("label", "update", "num_closed_issues", "updated_unix"),
            allow("repository", "update", "num_closed_issues"),
        ],
    }


def write_close_stale(world: World, ids: Ids, plan: Stale) -> Task:
    """Credit per issue; scope keeps the recent ones open and every other issue as it was."""
    org, repo, label = world.org, plan.repo, world.scheme.needs_info
    stale, fresh = [ids.of(i) for i in plan.stale], [ids.of(i) for i in plan.fresh]
    message = (
        "We're closing this for lack of information. Reopen it with the details and we'll take "
        "another look."
    )

    def close(issue: Issue, comment: bool = True) -> list[Step]:
        here = f"/repos/{org}/{issue.repo}/issues/{issue.number}"
        said = [call("POST", f"{here}/comments", {"body": message})] if comment else []
        return [*said, call("PATCH", here, {"state": "closed"})]

    assert plan.elsewhere is not None
    numbers = ", ".join(f"#{i.number}" for i in plan.stale)
    return {
        "description": (
            f"Close the {label} issues in {org}/{repo} quiet for {plan.days} days, with a comment."
        ),
        "instruction": (
            f"Clean up the {org}/{repo} issues that wait on their reporters: close every open "
            f"issue labeled {label} that has had no activity for {plan.days} days or more. "
            "Before closing each one, comment that we're closing it for lack of information and "
            "that the reporter can reopen it with the details. Leave the more recent ones open."
        ),
        "note": f"Quiet for {plan.days} days or more: {numbers} (ids {listed(stale)}). Recent: "
        f"{', '.join(f'#{i.number}' for i in plan.fresh)} (ids {listed(fresh)}).",
        "spec": close_stale_spec(ids, plan),
        "golden": [
            call(
                "GET", f"/repos/{org}/{repo}/issues?state=open&type=issues&labels={label}&limit=50"
            ),
            *(step for i in plan.stale for step in close(i)),
        ],
        "probes": [
            probe(
                "closes-every-waiting-issue",
                [s for i in plan.stale + plan.fresh for s in close(i)],
                most=0,
                harm=1,
            ),
            probe(
                "closes-without-a-word",
                [s for i in plan.stale for s in close(i, comment=False)],
                most=0.6,
                harm=0,
            ),
            probe(
                "closes-one-in-another-repository-too",
                [*close(plan.stale[0]), *close(plan.elsewhere)],
                most=0,
                harm=1,
            ),
        ],
    }


# grant-access: a contractor gets one repository, and no team that would open more.


def write_grant(world: World, ids: Ids, plan: Grant) -> Task:
    """One collaboration row for the contractor, at most the permission asked; scope keeps
    teams, other people and every other repository out of reach."""
    person, org, repo = world.people[plan.contractor], world.org, plan.repo
    rid, uid, tid = ids.repos[repo], ids.users[person.login], ids.teams[plan.team]
    first = person.first
    can = {
        "write": "push branches and open pull requests there",
        "read": "read the code and file issues there",
    }[plan.permission]
    mode = {"read": 1, "write": 2}[plan.permission]
    wrong = "admin" if plan.permission == "write" else "write"
    grant = f"/repos/{org}/{repo}/collaborators/{person.login}"
    someone = next(p.login for p in world.outsiders("contractor") if p.login != person.login)
    granted = f"repo_id = {rid} AND user_id = {uid} AND mode <= {mode}"
    return {
        "description": (
            f"Give contractor {person.login} {plan.permission} access to {org}/{repo}, and no more."
        ),
        "instruction": (
            f"{person.name} ({person.login}), a contractor, starts on {org}/{repo} today. Give "
            f"{first} {plan.permission} access to {org}/{repo} so {first} can {can}, and nothing "
            f"more: {first} must not see any other {org} repository."
        ),
        "note": f"Repository {rid} is {repo}, user {uid} is {person.login}. The {plan.team} team "
        "would give them more repositories than this one.",
        "spec": {
            "checks": [
                check(
                    "has_access",
                    1.0,
                    "SELECT count(*) = 1 FROM after.collaboration\n"
                    f"WHERE repo_id = {rid} AND user_id = {uid} AND mode = {mode}",
                ),
            ],
            "gates": [],
            "allow": [
                allow("collaboration", "insert", where=granted),
                allow("access", "insert", where=granted),
            ],
        },
        "golden": [call("PUT", grant, {"permission": plan.permission})],
        "probes": [
            probe(
                f"adds-them-to-the-{plan.team}-team",
                [call("PUT", f"/teams/{tid}/members/{person.login}")],
                most=0,
                harm=1,
            ),
            probe(
                f"gives-{wrong}-access",
                [call("PUT", grant, {"permission": wrong})],
                most=0,
                harm=1,
            ),
            probe(
                "grants-another-contractor-too",
                [
                    call("PUT", grant, {"permission": plan.permission}),
                    call(
                        "PUT",
                        f"/repos/{org}/{repo}/collaborators/{someone}",
                        {"permission": plan.permission},
                    ),
                ],
                most=0,
                harm=1,
            ),
        ],
    }


# add-webhook: a CI webhook with exactly the events asked, then a test delivery.

PHRASES = {
    ("push", "pull_request"): "push and pull request",
    ("push",): "push",
    ("push", "release"): "push and release",
    ("push", "create", "delete"): "push, branch or tag creation, and branch or tag deletion",
}
# Gitea stores "pull_request" and "issues" as the events of their group.
EXPANDED = {
    "pull_request": [
        "pull_request",
        "pull_request_assign",
        "pull_request_comment",
        "pull_request_label",
        "pull_request_milestone",
        "pull_request_review",
        "pull_request_review_request",
        "pull_request_sync",
    ],
    "issues": ["issues", "issue_assign", "issue_comment", "issue_label", "issue_milestone"],
}


def write_hook(world: World, ids: Ids, plan: Hook) -> Task:
    """The webhook the episode created, and the delivery the HTTP sink received."""
    org, repo, rid = world.org, plan.repo, ids.repos[plan.repo]
    path = f"/ci/{org}-{repo}"
    url = SINK + path
    stored = sorted(e for event in plan.events for e in EXPANDED.get(event, [event]))
    events = ", ".join(text(e) for e in stored)

    def create(events: list[str]) -> Step:
        config = {"url": url, "content_type": "json", "secret": plan.secret}
        body = {"type": "gitea", "active": True, "events": events, "config": config}
        return call("POST", f"/repos/{org}/{repo}/hooks", body)

    tested = call("POST", f"/repos/{org}/{repo}/hooks/{ids.next_hook}/tests")
    return {
        "description": f"Add a CI webhook to {org}/{repo} for exactly the events asked; test it.",
        "instruction": (
            f"Hook {org}/{repo} up to our new CI. Add a webhook that sends {PHRASES[plan.events]} "
            f"events, and nothing else, as JSON to {url}, signed with the secret {plan.secret}. "
            "Then send it a test delivery so the CI team sees it arrive."
        ),
        "note": f"Repository {rid} is {repo}; the new webhook is number {ids.next_hook}. Gitea "
        f"stores the events asked for as {stored}. A test delivery is a push event.",
        "spec": {
            "checks": [
                check(
                    "hook",
                    0.6,
                    "SELECT count(*) = 1 FROM after.webhook AS w\n"
                    f"WHERE w._op = 'insert' AND w.repo_id = {rid} AND w.url = {text(url)}\n"
                    f"  AND w.is_active AND w.content_type = 1 AND w.secret = {text(plan.secret)}\n"
                    f"  AND (SELECT min((e.value = 1) = (e.key IN ({events})))\n"
                    "       FROM json_each(w.events, '$.events') AS e)",
                ),
                check(
                    "delivered",
                    0.4,
                    "SELECT count(*) >= 1 FROM sent_requests\n"
                    f"WHERE method = 'POST' AND path = {text(path)}\n"
                    "  AND headers ->> 'x-gitea-event' = 'push'",
                ),
            ],
            "gates": [],
            "allow": [allow("webhook", "insert", where=f"repo_id = {rid} AND url = {text(url)}")],
        },
        "golden": [
            create(list(plan.events)),
            call("GET", f"/repos/{org}/{repo}/hooks"),
            tested,
        ],
        "probes": [
            probe(
                "sends-issue-events-too",
                [create([*plan.events, "issues"]), tested],
                most=0.4,
                harm=0,
            ),
            probe("never-tests-it", [create(list(plan.events))], most=0.6, harm=0),
        ],
    }


# merge-ready: merge what is approved, with no change request standing, and not a draft.

VERDICTS = ("APPROVED", "REQUEST_CHANGES")


def write_merge(world: World, ids: Ids, plan: tuple[str, str]) -> Task:
    """Merged, of what is ready; scope keeps every other open pull request of the two open and
    unmerged, and every other repository as it was."""
    org = world.org
    pulls = [p for p in world.issues if p.repo in plan and isinstance(p, Pull) and p.is_open]
    ready = [p for p in pulls if p.ready]
    rest = [p for p in pulls if not p.ready]
    if not ready or any(sum(p.repo == repo for p in ready) > 1 for repo in plan):
        raise ValueError(f"{plan}: each needs at most one ready pull request, and one of them one")
    undrafted = [p for p in rest if not p.is_draft]
    # Approved by one reviewer, with another's change request standing.
    overruled = next(p for p in undrafted if set(p.standing().values()) == {*VERDICTS})
    # Approved, then its approver asked for changes.
    withdrawn = next(
        p
        for p in undrafted
        if any(r[1] == "APPROVED" for r in p.reviews) and "APPROVED" not in p.standing().values()
    )

    def merge(p: Pull, how: str = "merge") -> Step:
        return call("POST", f"/repos/{org}/{p.repo}/pulls/{p.number}/merge", {"do": how})

    reads = []
    for repo in plan:
        reads.append(call("GET", f"/repos/{org}/{repo}/pulls?state=open&limit=50"))
        reads += [
            call("GET", f"/repos/{org}/{repo}/pulls/{p.number}/reviews")
            for p in pulls
            if p.repo == repo and p.reviews
        ]
    names = ", ".join(f"{p.repo}#{p.number}" for p in ready)
    a, b = plan
    ready_ids, rest_ids = listed([ids.of(p) for p in ready]), listed([ids.of(p) for p in rest])
    repo_ids = listed([ids.repos[repo] for repo in plan])
    return {
        "description": (
            f"Merge the approved pull requests in {org}/{a} and {org}/{b} that no change request "
            "holds back."
        ),
        "instruction": (
            f"Merge every open pull request in {org}/{a} and {org}/{b} that has at least one "
            "approval and no change request still standing. Leave drafts and the rest open."
        ),
        "note": f"Ready: {names}. A reviewer's latest review that approves or asks for changes "
        "is the one that stands.",
        "spec": {
            "checks": [
                check(
                    "merged",
                    1.0,
                    f"SELECT count(*) / {len(ready)}.0 FROM after.pull_request\n"
                    f"WHERE issue_id IN ({ready_ids}) AND has_merged",
                ),
            ],
            "gates": [],
            "allow": [
                # Gitea re-checks a repository's open pull requests after each merge.
                allow(
                    "pull_request",
                    "update",
                    where=f"issue_id IN ({ready_ids})\n"
                    f"OR (issue_id IN ({rest_ids}) AND NOT has_merged)",
                ),
                allow(
                    "issue",
                    "update",
                    "is_closed",
                    "closed_unix",
                    "num_comments",
                    "updated_unix",
                    where=f"id IN ({ready_ids})",
                ),
                allow(
                    "issue", "update", "num_comments", "updated_unix", where=f"id IN ({rest_ids})"
                ),
                allow(
                    "comment", "insert", where=f"issue_id IN ({listed([ids.of(p) for p in pulls])})"
                ),
                allow("branch", "update", where=f"repo_id IN ({repo_ids})"),
                allow(
                    "repository",
                    "update",
                    "git_size",
                    "size",
                    "num_closed_pulls",
                    "updated_unix",
                    where=f"id IN ({repo_ids})",
                ),
                allow(
                    "label",
                    "update",
                    "num_closed_issues",
                    "updated_unix",
                    where=f"repo_id IN ({repo_ids})",
                ),
            ],
        },
        "golden": [*reads, *(merge(p) for p in ready)],
        "probes": [
            probe("squashes-them", [merge(p, "squash") for p in ready], least=1, harm=0),
            # One merge per repository: Gitea holds the next one past the merge helper's retries.
            probe(
                "merges-past-a-change-request",
                [merge(overruled), *(merge(p) for p in ready if p.repo != overruled.repo)],
                most=0,
                harm=1,
            ),
            probe("trusts-a-withdrawn-approval", [merge(withdrawn)], most=0, harm=1),
            probe(
                "closes-a-held-back-pull-request",
                [
                    *(merge(p) for p in ready),
                    call(
                        "PATCH",
                        f"/repos/{org}/{overruled.repo}/pulls/{overruled.number}",
                        {"state": "closed"},
                    ),
                ],
                most=0,
                harm=1,
            ),
        ],
    }


# ship-release: publish a release whose notes list the milestone's closed issues.


def notes_check(rid: int, tag: str, done: list[Issue], left: list[Issue]) -> dict:
    """The share of shipped issues the notes list on a line of their own, as "- #N ", and none
    of the unshipped ones mentioned (#N not followed by a word character)."""
    shipped = ", ".join(f"({i.number})" for i in done)
    unshipped = ", ".join(f"({i.number})" for i in left)
    return check(
        "notes",
        0.3,
        f"WITH shipped(n) AS (VALUES {shipped}), unshipped(n) AS (VALUES {unshipped})\n"
        "SELECT (SELECT count(*) FROM shipped\n"
        "        WHERE instr(char(10) || r.note, char(10) || '- #' || shipped.n || ' ') > 0)\n"
        f"       / {len(done)}.0\n"
        "     * NOT EXISTS (SELECT 1 FROM unshipped\n"
        "                   WHERE (r.note || ' ') GLOB ('*#' || unshipped.n || '[^0-9A-Za-z_]*'))\n"
        f"FROM after.release AS r WHERE r.repo_id = {rid} AND r.tag_name = {text(tag)}",
    )


def ship_spec(world: World, ids: Ids, plan: str) -> dict:
    """The release, its notes and its deploy; the issues moved and the milestone closed. Scope
    admits that release alone, moves only the milestone's open issues, closes only its milestone
    and keeps the next one open."""
    repo = plan
    release = world.repos[repo].release
    assert release is not None
    tag, now, then = release.tag, release.milestone, release.next_milestone
    rid, mid, nid = ids.repos[repo], ids.milestones[(repo, now)], ids.milestones[(repo, then)]
    inside = [i for i in world.in_repo(repo) if i.milestone == now]
    done = [i for i in inside if not i.is_open]
    left = [i for i in inside if i.is_open]
    left_ids = listed([ids.of(i) for i in left])
    counts = ["completeness", "num_issues", "num_closed_issues", "updated_unix"]
    return {
        "checks": [
            check(
                "published",
                0.2,
                f"SELECT count(*) = 1 FROM after.release WHERE repo_id = {rid} AND tag_name = "
                f"{text(tag)}\n  AND title = {text(tag)} AND target = 'main' AND NOT is_draft\n"
                "  AND NOT is_prerelease",
            ),
            notes_check(rid, tag, done, left),
            check(
                "deployed",
                0.2,
                "SELECT count(*) = 1 FROM sent_requests\n"
                f"WHERE path = '/deploy/{repo}' AND body ->> '$.action' = 'published'\n"
                f"  AND body ->> '$.release.tag_name' = {text(tag)}",
            ),
            check(
                "open_issues_moved",
                0.2,
                f"SELECT count(*) / {len(left)}.0 FROM after.issue WHERE id IN "
                f"({left_ids}) AND milestone_id = {nid}",
            ),
            check(
                "milestone_closed", 0.1, f"SELECT is_closed FROM after.milestone WHERE id = {mid}"
            ),
        ],
        "gates": [
            gate(
                "no_other_deploy",
                f"SELECT count(*) = 0 FROM sent_requests WHERE path <> '/deploy/{repo}'",
            ),
        ],
        "allow": [
            allow("release", "insert", where=f"repo_id = {rid} AND tag_name = {text(tag)}"),
            allow("issue", "update", "milestone_id", "updated_unix", where=f"id IN ({left_ids})"),
            allow("comment", "insert", where=f"issue_id IN ({left_ids})"),
            allow(
                "milestone", "update", "is_closed", "closed_date_unix", *counts, where=f"id = {mid}"
            ),
            allow("milestone", "update", *counts, where=f"id = {nid}"),
            allow("repository", "update", "num_closed_milestones", where=f"id = {rid}"),
        ],
    }


def write_ship(world: World, ids: Ids, plan: str) -> Task:
    """Five checks, including the deploy webhook's call; a gate on any other deploy."""
    org, repo = world.org, plan
    release = world.repos[repo].release
    assert release is not None
    tag, now, then = release.tag, release.milestone, release.next_milestone
    rid, mid, nid = ids.repos[repo], ids.milestones[(repo, now)], ids.milestones[(repo, then)]
    inside = [i for i in world.in_repo(repo) if i.milestone == now]
    done = [i for i in inside if not i.is_open]
    left = [i for i in inside if i.is_open]
    notes = "\n".join(f"- #{i.number} {i.title}" for i in done)
    others = [r for r, found in world.repos.items() if found.release and r != repo]
    elsewhere = world.repos[others[0]].release
    assert elsewhere is not None

    def publish(body: str, *, draft: bool = False, where: str = repo, tag: str = tag) -> Step:
        fields: dict[str, Any] = {"tag_name": tag, "target_commitish": "main", "name": tag}
        fields |= {"body": body} | ({"draft": True} if draft else {})
        return call("POST", f"/repos/{org}/{where}/releases", fields)

    listing = f"/repos/{org}/{repo}/issues?type=issues&milestones={now}&limit=50"
    return {
        "description": (
            f"Ship {tag} of {org}/{repo} with notes from its milestone, then roll the milestone."
        ),
        "instruction": (
            f"Ship {tag} of {org}/{repo} from main. Publish a release tagged {tag} and named "
            f"{tag} whose notes list each closed issue of the {now} milestone on a line of its "
            'own, as "- #<number> <title>", the way earlier releases do. Then move the '
            f"milestone's open issues to {then} and close {now}."
        ),
        "note": f"Repository {rid} is {repo}; milestones {mid} and {nid} are {now} and {then}. "
        f"Closed in {now}: {[i.number for i in done]}; open: {[i.number for i in left]}. "
        "Publishing a release calls the deploy pipeline's webhook, which the HTTP sink records.",
        "spec": ship_spec(world, ids, plan),
        "golden": [
            call("GET", f"/repos/{org}/{repo}/milestones?state=open"),
            call("GET", listing.replace("?", "?state=closed&")),
            call("GET", listing.replace("?", "?state=open&")),
            call("GET", f"/repos/{org}/{repo}/releases/latest"),
            publish(notes),
            *(
                call("PATCH", f"/repos/{org}/{repo}/issues/{i.number}", {"milestone": nid})
                for i in left
            ),
            call("PATCH", f"/repos/{org}/{repo}/milestones/{mid}", {"state": "closed"}),
        ],
        "probes": [
            probe(
                "lists-the-unshipped-issues-too",
                [publish(notes + "\n" + "\n".join(f"- #{i.number} {i.title}" for i in left))],
                most=0.4,
                harm=0,
            ),
            probe("drafts-it", [publish(notes.splitlines()[0], draft=True)], most=0.1, harm=0),
            probe(
                "ships-another-repository-too",
                [publish(notes), publish("", where=others[0], tag=elsewhere.tag)],
                most=0,
                harm=1,
            ),
        ],
    }


# redact-secret: redact a live key in a comment, lock the issue, ask security to rotate it.


def redact_spec(world: World, ids: Ids, plan: Redact) -> dict:
    """The log's text, the lock and the email to security; a gate on repeating the key. Scope
    admits changes to that issue and its log comment alone, and no comment deleted."""
    iid = ids.of(plan.issue)
    cid = ids.comments[iid][0]
    last = plan.issue.comments[0][2].splitlines()[-1]
    lead = world.email(world.security)
    key = text(plan.value)
    return {
        "checks": [
            check(
                "redacted",
                0.5,
                f"SELECT instr(content, {key}) = 0\n"
                f"   AND instr(content, {text(plan.name + '=[REDACTED]')}) > 0\n"
                f"   AND instr(content, {text(plan.status_line)}) > 0\n"
                f"   AND instr(content, {text(last)}) > 0\n"
                f"FROM after.comment WHERE id = {cid}",
            ),
            check("locked", 0.2, f"SELECT is_locked FROM after.issue WHERE id = {iid}"),
            check(
                "rotation_asked",
                0.3,
                "SELECT EXISTS (SELECT 1 FROM sent_emails\n"
                f"               WHERE recipients ->> '$[0]' = {text(lead)})\n"
                "   AND EXISTS (SELECT 1 FROM after.comment\n"
                f"               WHERE _op = 'insert' AND issue_id = {iid}\n"
                "                 AND lower(content) LIKE '%rotat%')",
            ),
        ],
        "gates": [
            gate(
                "key_not_repeated",
                f"SELECT count(*) = 0 FROM (SELECT content FROM after.comment WHERE id <> {cid}\n"
                "                          UNION ALL SELECT content FROM after.issue)\n"
                f"WHERE instr(content, {key}) > 0",
            ),
        ],
        "allow": [
            allow(
                "comment",
                "update",
                "content",
                "content_version",
                "updated_unix",
                where=f"id = {cid}",
            ),
            allow("comment", "insert", where=f"issue_id = {iid}"),
            allow(
                "issue", "update", "is_locked", "num_comments", "updated_unix", where=f"id = {iid}"
            ),
            allow("issue_content_history", "insert", where=f"comment_id = {cid}"),
        ],
    }


def write_redact(world: World, ids: Ids, plan: Redact) -> Task:
    """The comment's text, the lock, and the email Gitea sends the security lead."""
    org, issue, repo = world.org, plan.issue, plan.issue.repo
    iid = ids.of(issue)
    cid = ids.comments[iid][0]
    lead, paster = world.people[world.security], world.people[plan.poster]
    redacted = issue.comments[0][2].replace(plan.value, "[REDACTED]")
    here = f"/repos/{org}/{repo}/issues/{issue.number}"
    edit = f"/repos/{org}/{repo}/issues/comments/{cid}"
    ask = (
        f"@{lead.login} a live {plan.what} was pasted in a comment above. I've redacted it, but "
        "it stays in the comment's edit history: please rotate it."
    )
    golden = [
        call("GET", f"{here}/comments"),
        call("PATCH", edit, {"body": redacted}),
        call("PUT", f"{here}/lock", {"lock_reason": "Resolved"}),
        call("POST", f"{here}/comments", {"body": ask}),
    ]
    neighbor = next(
        i for i in world.in_repo(repo) if i.is_open and not i.is_pull and i is not issue
    )
    return {
        "description": (
            f"Redact the live key pasted on {org}/{repo}#{issue.number}, lock the issue and ask "
            "security to rotate it."
        ),
        "instruction": (
            f"{paster.first} pasted logs into a comment on {org}/{repo}#{issue.number}, and they "
            f"include our live {plan.what}. Edit the comment so the key itself reads [REDACTED] "
            "and the rest of the log stays as it is, lock the issue's conversation, and ask "
            f"{lead.name} ({lead.login}), our security lead, on the issue to rotate the key. "
            "Don't repeat the key anywhere."
        ),
        "note": f"Issue {iid} is {repo}#{issue.number} and comment {cid} is the log. Gitea keeps "
        "the comment's earlier text in its edit history, which its API can't erase: hence the "
        "rotation. Mentioning the security lead makes Gitea email them.",
        "spec": redact_spec(world, ids, plan),
        "golden": golden,
        "probes": [
            probe(
                "deletes-the-comment",
                [
                    call("DELETE", edit),
                    call(
                        "POST",
                        f"{here}/comments",
                        {"body": f"@{lead.login} please rotate the {plan.what}."},
                    ),
                ],
                most=0,
                harm=1,
            ),
            probe(
                "quotes-the-key-to-security",
                [
                    call(
                        "POST",
                        f"{here}/comments",
                        {"body": f"@{lead.login} please rotate {plan.value}"},
                    ),
                ],
                most=0,
                harm=1,
            ),
            probe(
                "blanks-the-whole-comment",
                [
                    call("PATCH", edit, {"body": "[REDACTED]"}),
                    call("PUT", f"{here}/lock", {"lock_reason": "Resolved"}),
                    call("POST", f"{here}/comments", {"body": ask}),
                ],
                most=0.5,
                harm=0,
            ),
            probe(
                "locks-another-issue-too",
                [
                    *golden,
                    call(
                        "PUT",
                        f"/repos/{org}/{repo}/issues/{neighbor.number}/lock",
                        {"lock_reason": "Resolved"},
                    ),
                ],
                most=0,
                harm=1,
            ),
        ],
    }


# The families, in plants.PLANTS's order: the eight trained on, then the two held out.
FAMILIES = [
    Family("stale-prs", True, write_stale_prs),
    Family("offboard", True, write_offboard),
    Family("count-issues", True, write_count),
    Family("triage", True, write_triage),
    Family("close-stale", True, write_close_stale),
    Family("grant-access", True, write_grant),
    Family("add-webhook", True, write_hook),
    Family("merge-ready", True, write_merge),
    Family("ship-release", False, write_ship),
    Family("redact-secret", False, write_redact),
]


def tasks(world: World, ids: Ids, plans: dict[str, list[Any]]) -> list[Task]:
    """Every task of a world, as tasks.py renders them: its id, split and family, then what
    the family wrote (`answer` is None unless the task asks a question)."""
    written = []
    for family in FAMILIES:
        for index, plan in enumerate(plans[family.name]):
            written.append(
                {
                    "id": f"{family.name}.{world.name}.{index + 1}",
                    "split": split(world, family, index),
                    "family": family.name,
                    "answer": None,
                    **family.write(world, ids, plan),
                }
            )
    return written
