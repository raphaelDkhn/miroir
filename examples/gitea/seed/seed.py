"""Build a world in Gitea through Gitea's own API, and record the ids Gitea gave its rows.

`miroir bake` runs this twice, with the world's name in WORLD (a build argument). First as
`seed.py configure`, before Gitea starts, to name the forge and its mail sender after the
world's company. Then as the seed, once Gitea is up: it acts as each person through the
admin's Sudo header, so Gitea creates its own rows (timeline events, counters, review
requests), dates every record by its age, and writes the ids Gitea gave the rows to ids.json.
The host generator, tasks.py, reads that file back to write the world's tasks.

The same world builds the same on every bake. ids.json carries a digest of the world and its
plans, so the generator can prove it planted the same world.
"""

import base64
import hashlib
import json
import os
import secrets
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

import plants
import worlds
from worlds import Issue, Pull, World

GITEA = "http://127.0.0.1:3000"
APP_INI = Path("/etc/gitea/app.ini")
IDS = Path("/opt/gitea-env/ids.json")
PSQL = ["psql", "-h", "127.0.0.1", "-U", "postgres", "-d", "gitea", "-v", "ON_ERROR_STOP=1", "-q"]
NOW = int(time.time())
DAY = 86_400
# The site admin `miroir.yaml`'s bake creates; the seed acts through it.
ADMIN = "env-admin"
# The admin's token: random per bake and used only by this seed, so no public code derives it.
TOKEN = secrets.token_hex(20)
UNITS = ["repo.code", "repo.issues", "repo.pulls", "repo.releases", "repo.wiki", "repo.projects"]


def call(method: str, path: str, body: Any = None, sudo: str | None = None) -> Any:
    """Call Gitea's API as the admin, or as `sudo`."""
    headers = {"Authorization": f"token {TOKEN}"}
    if sudo is not None:
        headers["Sudo"] = sudo
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    url = f"{GITEA}/api/v1{path}"
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            text = response.read()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")[:500]
        raise SystemExit(f"{method} {path} as {sudo or ADMIN}: {e.code} {detail}") from e
    return json.loads(text) if text else None


def psql(script: str) -> None:
    """Run SQL on Gitea's database, failing on the first error."""
    subprocess.run(PSQL, input=script, text=True, check=True)


def rows(sql: str) -> list[list[str]]:
    """A query's rows, as lists of text fields."""
    done = subprocess.run(
        [*PSQL, "-tA", "-F", "\t", "-c", sql], text=True, check=True, capture_output=True
    )
    return [line.split("\t") for line in done.stdout.splitlines() if line]


def insert_token(login: str, token: str) -> None:
    """Store a personal access token of `login` as Gitea does: a salted PBKDF2 hash."""
    salt = f"env{login[:7]}"
    digest = hashlib.pbkdf2_hmac("sha256", token.encode(), salt.encode(), 10_000, 50).hex()
    stored = rows(
        "INSERT INTO access_token (uid, name, token_hash, token_salt, token_last_eight, "
        f"scope, created_unix, updated_unix) SELECT id, 'env', '{digest}', '{salt}', "
        f"'{token[-8:]}', 'all', {NOW}, {NOW} FROM \"user\" WHERE lower_name = '{login}' "
        "RETURNING uid"
    )
    if len(stored) != 1:
        raise SystemExit(f"no Gitea user {login!r} to give a token: create the user first")


def create_people(world: World) -> None:
    """Create every person, staff and outsiders, with a random password nobody knows."""
    for person in world.people.values():
        call(
            "POST",
            "/admin/users",
            {
                "username": person.login,
                "full_name": person.name,
                "email": world.email(person.login),
                "password": secrets.token_hex(16),
                "must_change_password": False,
                "send_notify": False,
            },
        )


def encode(text: str) -> str:
    """Base64, as Gitea's contents API takes file contents."""
    return base64.b64encode(text.encode()).decode()


def create_org(world: World) -> None:
    """Create the organization as its manager, then its repositories, teams and files."""
    manager = world.manager
    call(
        "POST",
        "/orgs",
        {
            "username": world.org,
            "full_name": world.company,
            "description": f"{world.company}, {world.about}",
            "visibility": "public",
        },
        sudo=manager,
    )
    for repo in world.repos.values():
        call(
            "POST",
            f"/orgs/{world.org}/repos",
            {
                "name": repo.name,
                "description": repo.description,
                "private": repo.private,
                "auto_init": True,
                "default_branch": "main",
                "readme": "Default",
            },
            sudo=manager,
        )
    for team in world.teams.values():
        units = dict.fromkeys(UNITS, "read" if team.permission == "read" else "write")
        created = call(
            "POST",
            f"/orgs/{world.org}/teams",
            {
                "name": team.name,
                "description": team.description,
                "permission": team.permission,
                "includes_all_repositories": not team.repos,
                "units_map": units,
            },
        )
        for repo in team.repos:
            call("PUT", f"/teams/{created['id']}/repos/{world.org}/{repo}")
        for person in world.people.values():
            if team.name in person.teams:
                call("PUT", f"/teams/{created['id']}/members/{person.login}")
    for repo in world.repos.values():
        changes = [
            {"operation": "create", "path": path, "content": encode(text)}
            for path, text in repo.files.items()
        ]
        call(
            "POST",
            f"/repos/{world.org}/{repo.name}/contents",
            {"branch": "main", "message": "Add the project layout", "files": changes},
            sudo=repo.catch_all,
        )


def create_labels_and_milestones(world: World) -> tuple[dict, dict]:
    """Each repository's labels and milestones; returns their ids by repository and name."""
    labels: dict[str, dict[str, int]] = {}
    milestones: dict[str, dict[str, int]] = {}
    for repo in world.repos.values():
        labels[repo.name] = {}
        for name, color, description, exclusive in world.scheme.labels(repo):
            label = call(
                "POST",
                f"/repos/{world.org}/{repo.name}/labels",
                {
                    "name": name,
                    "color": f"#{color}",
                    "description": description,
                    "exclusive": exclusive,
                },
            )
            labels[repo.name][name] = label["id"]
        milestones[repo.name] = {}
        if repo.release:
            for title in (repo.release.milestone, repo.release.next_milestone):
                made = call(
                    "POST",
                    f"/repos/{world.org}/{repo.name}/milestones",
                    {"title": title, "state": "open"},
                )
                milestones[repo.name][title] = made["id"]
    return labels, milestones


def open_issue(world: World, issue: Issue, fields: dict) -> dict:
    """Open an issue or pull request as its poster, with its labels, assignees and milestone;
    returns what Gitea answered."""
    org, repo = world.org, issue.repo
    if isinstance(issue, Pull):
        path, text = issue.file
        call(
            "POST",
            f"/repos/{org}/{repo}/contents",
            {
                "branch": "main",
                "new_branch": issue.branch,
                "message": issue.title,
                "files": [{"operation": "create", "path": path, "content": encode(text)}],
            },
            sudo=issue.poster,
        )
        return call(
            "POST",
            f"/repos/{org}/{repo}/pulls",
            fields | {"head": issue.branch, "base": "main"},
            sudo=issue.poster,
        )
    if world.access(issue.poster, repo) in ("write", "admin"):
        return call("POST", f"/repos/{org}/{repo}/issues", fields, sudo=issue.poster)
    # Gitea drops the labels, assignees and milestone a reader sets: a writer triages it.
    bare = {"title": issue.title, "body": issue.body}
    created = call("POST", f"/repos/{org}/{repo}/issues", bare, sudo=issue.poster)
    triager = world.writers(repo)[0]
    here = f"/repos/{org}/{repo}/issues/{created['number']}"
    if fields["labels"]:
        call("POST", f"{here}/labels", {"labels": fields["labels"]}, sudo=triager)
    if issue.assignees or issue.milestone:
        call(
            "PATCH",
            here,
            {"assignees": fields["assignees"], "milestone": fields["milestone"]},
            sudo=triager,
        )
    return created


def create_issue(world: World, issue: Issue, labels: dict, milestones: dict) -> dict:
    """Create an issue or pull request with its comments, reviews and close; returns the ids
    of its comments and reviews, by age."""
    org, repo = world.org, issue.repo
    fields = {
        "title": issue.title,
        "body": issue.body,
        "assignees": issue.assignees,
        "labels": [labels[repo][name] for name in issue.labels],
        "milestone": milestones[repo].get(issue.milestone or "", 0),
    }
    created = open_issue(world, issue, fields)
    if created["number"] != issue.number:
        raise SystemExit(f"{repo}#{created['number']} was planned as #{issue.number}")
    here = f"/repos/{org}/{repo}/issues/{issue.number}"
    dated: dict[str, list[tuple[int, float]]] = {"comments": [], "reviews": []}
    for author, age, text in issue.comments:
        comment = call("POST", f"{here}/comments", {"body": text}, sudo=author)
        dated["comments"].append((comment["id"], age))
    for reviewer, event, age, text in getattr(issue, "reviews", []):
        review = call(
            "POST",
            f"/repos/{org}/{repo}/pulls/{issue.number}/reviews",
            {"event": event, "body": text},
            sudo=reviewer,
        )
        dated["reviews"].append((review["id"], age))
    if issue.closed is not None:
        call("PATCH", here, {"state": "closed"}, sudo=issue.closer)
    return dated


def create_releases(world: World) -> list[tuple[int, float]]:
    """The earlier releases, whose notes list their issues as the teams write them."""
    dated = []
    for repo, tag, age, issues in sorted(world.releases, key=lambda r: -r[2]):
        notes = "\n".join(
            f"- #{i.number} {i.title}" for i in sorted(issues, key=lambda i: i.number)
        )
        release = call(
            "POST",
            f"/repos/{world.org}/{repo}/releases",
            {"tag_name": tag, "target_commitish": "main", "name": tag, "body": notes},
            sudo=world.repos[repo].catch_all,
        )
        dated.append((release["id"], age))
    return dated


def create_hooks(world: World) -> None:
    """The deploy pipeline's webhooks, after the releases so that none of them fires."""
    for repo, path, events in world.hooks:
        call(
            "POST",
            f"/repos/{world.org}/{repo}/hooks",
            {
                "type": "gitea",
                "active": True,
                "events": events,
                "config": {"url": f"{plants.SINK}{path}", "content_type": "json"},
            },
        )


def unix(age: float) -> int:
    """The Unix time `age` days before the bake."""
    return NOW - round(age * DAY)


def ages(issue: Issue) -> tuple[float, float, float | None]:
    """When an issue opened, last changed and closed, in days before the bake."""
    return issue.opened, issue.last_active, issue.closed


def date(world: World, created: list[tuple[Issue, dict]], releases: list) -> None:
    """Date every record by its age: issues, comments, reviews, milestones and releases."""
    issues = [
        f"('{issue.repo}', {issue.number}, {unix(opened)}, {unix(updated)}, "
        f"{0 if closed is None else unix(closed)})"
        for issue, _ in created
        for opened, updated, closed in [ages(issue)]
    ]
    comments = [f"({cid}, {unix(age)})" for _, ids in created for cid, age in ids["comments"]]
    reviews = [f"({rid}, {unix(age)})" for _, ids in created for rid, age in ids["reviews"]]
    deadlines = [
        f"('{repo.name}', '{title}', {(unix(-due) // DAY) * DAY + DAY - 1})"
        for repo in world.repos.values()
        if repo.release
        for title, due in [
            (repo.release.milestone, repo.release.due),
            (repo.release.next_milestone, repo.release.next_due),
        ]
    ]
    released = [f"({rid}, {unix(age)})" for rid, age in releases]
    empty = "(NULL::bigint, NULL::bigint)"
    psql(f"""
BEGIN;
UPDATE "user" SET created_unix = {unix(700)} + id * {DAY}, updated_unix = {unix(30)};
UPDATE repository SET created_unix = {unix(640)} + id * {DAY}, updated_unix = {unix(0.5)};
UPDATE label SET created_unix = {unix(600)}, updated_unix = {unix(600)};
UPDATE webhook SET created_unix = {unix(300)}, updated_unix = {unix(300)};
UPDATE issue SET created_unix = v.opened, updated_unix = v.updated, closed_unix = v.closed
  FROM (VALUES {", ".join(issues)}) AS v(repo, index, opened, updated, closed), repository r
  WHERE r.owner_name = '{world.org}' AND r.lower_name = v.repo
    AND issue.repo_id = r.id AND issue.index = v.index;
UPDATE comment SET created_unix = issue.created_unix + 60, updated_unix = issue.created_unix + 60
  FROM issue WHERE comment.issue_id = issue.id;
UPDATE comment SET created_unix = issue.closed_unix, updated_unix = issue.closed_unix
  FROM issue WHERE comment.issue_id = issue.id AND comment.type = 2;
UPDATE comment SET created_unix = v.at, updated_unix = v.at
  FROM (VALUES {", ".join(comments) or empty}) AS v(id, at) WHERE comment.id = v.id;
UPDATE review SET created_unix = issue.created_unix + 60, updated_unix = issue.created_unix + 60
  FROM issue WHERE review.issue_id = issue.id;
UPDATE review SET created_unix = v.at, updated_unix = v.at
  FROM (VALUES {", ".join(reviews) or empty}) AS v(id, at) WHERE review.id = v.id;
UPDATE comment SET created_unix = review.created_unix, updated_unix = review.created_unix
  FROM review WHERE comment.review_id = review.id;
UPDATE milestone SET created_unix = {unix(90)}, updated_unix = {unix(1)},
    deadline_unix = v.deadline
  FROM (VALUES {", ".join(deadlines)}) AS v(repo, name, deadline), repository r
  WHERE r.owner_name = '{world.org}' AND r.lower_name = v.repo
    AND milestone.repo_id = r.id AND milestone.name = v.name;
UPDATE release SET created_unix = v.at
  FROM (VALUES {", ".join(released)}) AS v(id, at) WHERE release.id = v.id;
COMMIT;
""")


def read_ids(world: World) -> worlds.Ids:
    """The ids Gitea gave the world's rows."""
    users = {name: int(i) for name, i in rows('SELECT lower_name, id FROM "user"')}
    org = users[world.org]
    in_org = f"r.owner_id = {org}"
    comments: dict[int, list[int]] = {}
    for issue, comment in rows(
        "SELECT c.issue_id, c.id FROM comment c JOIN issue i ON i.id = c.issue_id "
        f"JOIN repository r ON r.id = i.repo_id WHERE {in_org} AND c.type = 0 ORDER BY c.id"
    ):
        comments.setdefault(int(issue), []).append(int(comment))
    return worlds.Ids(
        org=org,
        users=users,
        repos={
            n: int(i)
            for n, i in rows(f"SELECT r.lower_name, r.id FROM repository r WHERE {in_org}")
        },
        issues={
            (r, int(n)): int(i)
            for r, n, i in rows(
                "SELECT r.lower_name, i.index, i.id FROM issue i "
                f"JOIN repository r ON r.id = i.repo_id WHERE {in_org}"
            )
        },
        comments=comments,
        labels={
            (r, n): int(i)
            for r, n, i in rows(
                "SELECT r.lower_name, l.name, l.id FROM label l "
                f"JOIN repository r ON r.id = l.repo_id WHERE {in_org}"
            )
        },
        milestones={
            (r, n): int(i)
            for r, n, i in rows(
                "SELECT r.lower_name, m.name, m.id FROM milestone m "
                f"JOIN repository r ON r.id = m.repo_id WHERE {in_org}"
            )
        },
        teams={n: int(i) for n, i in rows(f"SELECT lower_name, id FROM team WHERE org_id = {org}")},
        next_hook=int(rows("SELECT max(id) + 1 FROM webhook")[0][0]),
    )


def configure(world: World) -> None:
    """Name the forge and its mail sender after the world's company, in Gitea's app.ini."""
    name = f"{world.company.split()[0]} Code"
    lines = APP_INI.read_text(encoding="utf-8").splitlines(keepends=True)
    for key, value in (("APP_NAME", name), ("FROM", f"{name} <code@{world.domain}>")):
        found = [i for i, line in enumerate(lines) if line.startswith(f"{key} = ")]
        if len(found) != 1:
            raise SystemExit(f"{APP_INI}: {len(found)} lines set {key}; keep exactly one")
        lines[found[0]] = f"{key} = {value}\n"
    APP_INI.write_text("".join(lines), encoding="utf-8")


def seed(name: str) -> None:
    """Build the world in Gitea, then write the ids Gitea gave its rows to ids.json."""
    world, plans = plants.build(name)
    digest = plants.digest(world, plans)
    insert_token(ADMIN, TOKEN)
    create_people(world)
    insert_token(world.manager, worlds.token(world.name, world.manager))
    create_org(world)
    labels, milestones = create_labels_and_milestones(world)
    created = [(issue, create_issue(world, issue, labels, milestones)) for issue in world.issues]
    releases = create_releases(world)
    create_hooks(world)
    date(world, created, releases)
    ids = {"world": name, "digest": digest} | read_ids(world).to_json()
    IDS.write_text(json.dumps(ids), encoding="utf-8")
    print(f"world {name}: {len(world.issues)} issues and pull requests; their ids are in {IDS}")


def main() -> None:
    """Configure Gitea for WORLD's company (`seed.py configure`), or build WORLD's world."""
    name = os.environ.get("WORLD", "")
    if name not in worlds.WORLDS:
        raise SystemExit(
            f"WORLD is {name!r}; the worlds are {', '.join(worlds.WORLDS)}: "
            "build the image with --build-arg WORLD=<world>"
        )
    arguments = sys.argv[1:]
    if arguments == ["configure"]:
        configure(worlds.WORLDS[name]())
    elif not arguments:
        seed(name)
    else:
        raise SystemExit(f"unknown arguments {arguments}: run seed.py, or seed.py configure")


if __name__ == "__main__":
    main()
