import asyncio
from types import SimpleNamespace

from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client
from openenv.cli._validation import validate_running_environment
from openenv.core import MCPToolClient
from openenv.core.env_server.mcp_types import CallToolAction
from openenv.validation.graders.runtime.basic import (
    ObservationSchemaGrader,
    RewardWellFormedGrader,
    StateContractGrader,
)
from openenv.validation.manifest import RewardDeclaration
from openenv.validation.runtime.collector import collect_runtime_evidence
from openenv.validation.runtime.contracts import RuntimePlan, RuntimeReset
from openenv.validation.types import CheckStatus

GOLDEN = [
    ("PATCH", "/deals/1042?notify=false", {"owner_id": 7}),
    ("POST", "/deals/1042/notes", {"text": "Reassigned from Priya, who leaves on Friday."}),
    ("PATCH", "/deals/1043?notify=false", {"owner_id": 7}),
    ("POST", "/deals/1043/notes", {"text": "Reassigned from Priya, who leaves on Friday."}),
    ("PATCH", "/deals/1057?notify=false", {"owner_id": 7}),
    ("POST", "/deals/1057/notes", {"text": "Reassigned from Priya, who leaves on Friday."}),
]


def test_openenv_client_episode(env_url: str) -> None:

    async def episode() -> None:
        async with MCPToolClient(base_url=env_url, message_timeout_s=120) as env:
            start = await env.reset(task_id="reassign-stale-deals", episode_id="ep-client")
            assert start.observation.instruction.startswith("Priya Shah leaves the team")
            assert start.reward is None and start.done is False
            tools = await env.list_tools()
            assert [tool.name for tool in tools] == ["api_request", "api_docs", "submit"]
            deals = await env.call_tool(
                "api_request", method="GET", path="/deals?owner_id=12&status=open"
            )
            assert deals["error"] is None
            assert {deal["id"] for deal in deals["result"]["body"]} == {
                1042,
                1043,
                1050,
                1057,
                1061,
            }
            for method, path, body in GOLDEN:
                step = await env.step(
                    CallToolAction(
                        tool_name="api_request",
                        arguments={"method": method, "path": path, "body": body},
                    )
                )
                assert step.observation.error is None and step.reward == 0.0 and step.done is False
            final = await env.step(CallToolAction(tool_name="submit", arguments={"answer": "done"}))
            assert (final.reward, final.done) == (1.0, True)
            assert final.metadata["success"] is True
            state = await env.state()
            assert (state.episode_id, state.step_count) == ("ep-client", 8)

    asyncio.run(episode())


def test_openenv_runtime_graders(env_url: str) -> None:
    plan = RuntimePlan(
        plan_schema_version="1",
        reset=RuntimeReset(
            episode_id="ep-graders", seed=0, options={"task_id": "count-overdue-invoices"}
        ),
        actions=[
            {"type": "list_tools"},
            {
                "type": "call_tool",
                "tool_name": "api_request",
                "arguments": {"method": "GET", "path": "/contacts?q=Northwind"},
            },
            {
                "type": "call_tool",
                "tool_name": "api_docs",
                "arguments": {"operation": "listInvoices"},
            },
            {"type": "call_tool", "tool_name": "submit", "arguments": {"answer": "3"}},
        ],
    )
    evidence = collect_runtime_evidence(env_url, plan, episode_timeout_s=180)
    assert evidence.failure_reason is None, evidence.failure_reason
    assert [row.operation for row in evidence.exchanges] == ["reset", "state"] + [
        "step",
        "state",
    ] * 4
    subject = SimpleNamespace(
        runtime_evidence=evidence,
        manifest=SimpleNamespace(reward=RewardDeclaration(range=(0.0, 1.0), floor_margin=1.0)),
    )
    for grader in (RewardWellFormedGrader(), ObservationSchemaGrader(), StateContractGrader()):
        result = grader.run(subject)
        assert result.status is CheckStatus.PASS, (grader.check_id, result.evidence)


def test_openenv_validates_the_running_env(env_url: str) -> None:
    report = validate_running_environment(env_url)
    failed = [c for c in report["criteria"] if c.get("required", True) and not c["passed"]]
    assert report["passed"], failed


def test_a_standard_mcp_client_connects_and_acts(env_url: str) -> None:
    async def episode() -> None:
        async with MCPToolClient(base_url=env_url, message_timeout_s=120) as env:
            await env.reset(task_id="count-overdue-invoices")
            async with (
                streamable_http_client(f"{env_url}/mcp") as (read, write, _),
                ClientSession(read, write) as session,
            ):
                await session.initialize()
                names = [tool.name for tool in (await session.list_tools()).tools]
                assert names == ["api_request", "api_docs", "submit"]
                me = await session.call_tool("api_request", {"method": "GET", "path": "/me"})
                assert not me.isError and "dana@acme.test" in me.content[0].text
                bad = await session.call_tool("no_such_tool", {})
                assert bad.isError
                done = await session.call_tool("submit", {"answer": "3"})
                assert not done.isError
            state = await env.state()
            assert state.step_count == 3

    asyncio.run(episode())
