import json
import queue
import threading
from collections.abc import Callable
from typing import Any, cast

import pytest
from conftest import FakeModel

import miroir
from miroir import CoworkerModel, MiroirError, Session
from miroir.review import call, probe_steps

TURN = {
    "coworker": "priya",
    "messages": [
        {"role": "system", "content": "You are Priya Shah."},
        {"role": "user", "content": "@priya which stage?"},
    ],
}


class ScriptedSocket:
    def __init__(self, replies: dict[str, Callable[[dict[str, Any]], dict[str, Any]]]) -> None:
        self.replies = replies
        self.sent: list[dict[str, Any]] = []
        self._reply: dict[str, Any] = {}

    def send(self, text: str) -> None:
        frame = json.loads(text)
        self.sent.append(frame)
        self._reply = self.replies[frame["type"]](frame)

    def recv(self) -> str:
        return json.dumps(self._reply)

    def close(self) -> None:
        pass


def session(socket: object) -> Session:
    return Session("http://env:1", cast(Any, socket))


def answering(turns: list[dict[str, Any]], status: int = 201) -> ScriptedSocket:
    owed = list(turns)

    def pull(_: dict[str, Any]) -> dict[str, Any]:
        return {"type": "coworkers", "data": {"turns": owed}}

    def reply(frame: dict[str, Any]) -> dict[str, Any]:
        owed.clear()
        return {"type": "coworker_reply", "data": {"status": status}}

    return ScriptedSocket({"coworkers": pull, "coworker_reply": reply})


def test_a_reset_says_it_answers_coworkers_only_when_it_does():
    observation = {"type": "observation", "data": {"observation": {"metadata": {}}}}
    socket = ScriptedSocket({"reset": lambda frame: observation})
    session(socket).reset("count")
    session(socket).reset("ask-priya", coworkers=True)
    assert socket.sent[0]["data"]["coworkers"] is False
    assert socket.sent[1]["data"]["coworkers"] is True


def test_coworkers_answer_the_turns_they_owe_with_the_models_text():
    model = FakeModel("Qualified.")
    try:
        socket = answering([TURN])
        answered = miroir.answer_coworkers(session(socket), CoworkerModel(model.url, "small", "k"))
        assert answered == 1
        assert socket.sent[-1] == {
            "type": "coworker_reply",
            "data": {"coworker": "priya", "text": "Qualified."},
        }
        path, headers, body = model.requests[0]
        assert path == "/v1/chat/completions"
        assert headers["Authorization"] == "Bearer k"
        assert body == {"model": "small", "messages": TURN["messages"]}, "no temperature unless set"
        miroir.answer_coworkers(session(answering([TURN])), CoworkerModel(model.url, "m", None, 0))
        assert model.requests[1][2]["temperature"] == 0
        assert "Authorization" not in model.requests[1][1]
    finally:
        model.close()


def test_no_turn_owed_calls_no_model():
    model = FakeModel()
    try:
        assert miroir.answer_coworkers(session(answering([])), CoworkerModel(model.url, "m")) == 0
        assert model.requests == []
    finally:
        model.close()


@pytest.mark.parametrize(
    ("model", "says"),
    [
        (lambda: FakeModel(status=500), "HTTP Error 500"),
        (lambda: FakeModel(content="  "), "answered with no text"),
        (lambda: FakeModel(content=None), "answered with no text"),
        (lambda: FakeModel(delay=1.0), "timed out"),
    ],
)
def test_a_model_that_fails_is_a_coworker_failure(monkeypatch, model, says):
    monkeypatch.setattr(miroir, "COWORKER_TIMEOUT_S", 0.3)
    served = model()
    try:
        socket = answering([TURN])
        with pytest.raises(MiroirError, match=says) as failed:
            miroir.answer_coworkers(session(socket), CoworkerModel(served.url, "m"))
        assert failed.value.code == "COWORKER_FAILED"
        assert [frame["type"] for frame in socket.sent] == ["coworkers"], "no answer is sent"
    finally:
        served.close()


def test_a_coworker_model_never_shows_its_key():
    assert "secret" not in repr(CoworkerModel("http://m", "m", "secret"))


class SlowReader:
    def __init__(self) -> None:
        self.frames: queue.Queue[dict[str, Any]] = queue.Queue()
        self.second_read = threading.Event()
        self.first = threading.Lock()

    def send(self, text: str) -> None:
        self.frames.put(json.loads(text))

    def recv(self) -> str:
        first = self.first.acquire(blocking=False)
        if first:
            self.second_read.wait(0.5)
        frame = self.frames.get(timeout=2)
        if not first:
            self.second_read.set()
        return json.dumps({"type": frame["type"], "data": {"answers": frame["type"]}})


def test_two_threads_never_take_each_others_replies():
    held = session(SlowReader())
    got: dict[str, Any] = {}
    reader = threading.Thread(target=lambda: got.update(state=held.state()))
    reader.start()
    got["turns"] = held._exchange({"type": "coworkers"})
    reader.join()
    assert got == {"state": {"answers": "state"}, "turns": {"answers": "coworkers"}}


RECORD: dict[str, Any] = {
    "metadata": {"termination": "submit"},
    "actions": [
        {
            "tool": "api_request",
            "status": 201,
            "arguments": {
                "method": "POST",
                "path": "/deals/1100/notes",
                "body": {"text": "@priya"},
            },
            "result": {"status": 201},
        },
        {
            "tool": "coworker_reply",
            "via": "coworker",
            "app": None,
            "status": 201,
            "arguments": {"coworker": "priya", "text": "Qualified."},
            "result": {"status": 201},
        },
        {
            "tool": "api_request",
            "status": 200,
            "arguments": {"method": "PATCH", "path": "/deals/1100", "body": {"stage": "qualified"}},
            "result": {"status": 200},
        },
        {"tool": "submit", "status": None, "arguments": {}, "result": {"submitted": True}},
    ],
}


def test_a_coworkers_answer_replays_as_their_step_and_shows_as_theirs():
    steps = probe_steps(RECORD)
    assert [step["tool"] for step in steps] == [
        "api_request",
        "coworker_reply",
        "api_request",
        "submit",
    ]
    assert steps[1]["arguments"] == {"coworker": "priya", "text": "Qualified."}
    assert call(RECORD["actions"][1]) == "priya answered: Qualified. → 201"
