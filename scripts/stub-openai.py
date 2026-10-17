#!/usr/bin/env python3
"""An OpenAI-shaped endpoint that answers Velora without a model.

This exists so the *whole* path can be exercised without a credential: the real
binary, the real OpenAI-shaped provider, real HTTP, real history — with a stub
standing in for the model. It is how the judge's tool attachment, the confidence
notation, and the path from "I have a camera on the other machine" to an event the
system hears every few seconds were all verified on the wire rather than only in
unit tests.

It captures every request body it is sent, so what Velora actually put on the wire
can be read afterwards.

Usage:

    python3 scripts/stub-openai.py --port 7801 --out /tmp/velora-stub

    VELORA_LOG=info \\
    VELORA_UI=127.0.0.1:7799 \\
    VELORA_STORAGE=sqlite VELORA_HISTORY=/tmp/velora-real/history.db \\
    VELORA_LLM_PROVIDER=openai \\
    VELORA_LLM_BASE_URL=http://127.0.0.1:7801 \\
    VELORA_LLM_API_KEY=stub-key VELORA_LLM_MODEL=stub-model \\
    cargo run -q -p velora-bin

    curl -s -X POST http://127.0.0.1:7799/api/say -d '{"text":"I really love red."}'
    curl -s http://127.0.0.1:7799/api/snapshot

What it does, by request:

- A request whose tools include `revise_belief` is a judgement. It records what
  the event said, and it claims a confidence of `0.9` — a *fraction*, not basis
  points, which is the notation a real model writes and the one that used to make
  the whole answer unreadable.
- A request whose tools include `plan_create` is the reasoning engine deciding
  that something the operator cares about has changed. With `--camera` it answers
  by making a plan, and then works through the plan one step per turn: write a
  procedure for looking at the camera, set up a source that runs it, tell the
  operator. Nobody asks again in between.
- With `--parallel`, a message asking for something slow is answered with a long
  shell step, and every other message is answered straight away. Sending the
  second while the first is still running is how "talk to me while you install
  something" is exercised: the answer has to arrive before the install finishes.
- With `--long`, the first turn works for far longer than one sitting used to
  allow — a different command every step, so nothing about it looks like a
  circle — and then concludes. That is how the reviews are exercised end to end:
  the turn keeps going past each one, and stops when the model decides to.
- Anything else gets one sentence, so the turn ends without running anything.

With `--camera`, this server also answers `GET /shot`, which is the camera: a few
bytes of text that change every time it is looked at, so a source watching it has
something new to report.

Nothing here is a model. It answers the same thing to every message, and a belief
it reports is the stub's, not the system's.
"""

import argparse
import http.server
import itertools
import json
import os

CAMERA_URL = None
DOOR_URL = None
LONG = False
PARALLEL = False
FRAMES = itertools.cycle(
    [
        "an empty hallway",
        "an empty hallway",
        "a person standing at the door",
        "an empty hallway",
    ]
)


def field(prompt, name):
    """The value of one `name: value` line in a prompt, as far as it fits."""
    for line in prompt.splitlines():
        if line.startswith(name):
            return line[len(name) :].strip()
    return ""


def reconsidered(prompt):
    """The belief a reconsideration is about, and what it currently says.

    The system asks itself whether something it holds still holds; answering
    means re-asserting it, correcting it, or withdrawing it.
    """
    key = field(prompt, "content:").strip()
    for line in prompt.splitlines():
        prefix = f"- {key} = "
        if line.startswith(prefix):
            return key, line[len(prefix) :].split(" (")[0]
    return key, ""


def judge_answer(prompt):
    """What the judge says an event shows, and whether it calls for action.

    The wake is the part that matters here: a report from a source is recorded
    like anything else, and *this one* is worth doing something about. A real
    model decides that from what the operator asked for; this decides it from one
    word in the report.
    """
    kind = field(prompt, "kind:")
    content = field(prompt, "content:")

    if kind == "belief.reconsider":
        # Looking at something it already holds. The answer here is that it still
        # holds, at a slightly lower confidence than the first time: it has been
        # looked at again and not re-derived from anything new.
        key, value = reconsidered(prompt)
        return (
            {
                "action": "assert",
                "key": key,
                "value": value,
                "confidence": 0.75,
                "rationale": "looked at it again; nothing has changed and it still holds",
            },
            None,
        )

    if kind == "observation.self":
        # Something a source the system set up reported: evidence for what that
        # source saw, and the only evidence there will ever be. Which source it
        # was is in the payload, which is what lets two of them be told apart.
        observer = content.split(" reported:", 1)[0].strip()
        key = "door.state" if observer.startswith("door") else "camera.saw"
        value = content.split("reported:", 1)[-1].strip()[:120]
        why = f"{observer or 'a source'} reported it"
    else:
        key = "operator.stated"
        value = content[:120]
        why = "the operator said so"

    wake = None
    if kind == "observation.self" and "person" in content:
        wake = {
            "task": "Somebody is in front of the camera; tell the operator.",
            "reason": "they asked to be told when the camera sees somebody",
        }
    elif kind == "operator.said" and any(
        asked in content.lower() for asked in ("watch", "keep an", "monitor")
    ):
        # What a real model did, and the tool description asked it not to: judge
        # the operator's own message as something to act on, on top of the rule
        # that already attends to it. That is the shape of three turns for one
        # message, and it is what the resolver now settles.
        wake = {
            "task": "Set up continuous watching and decide when a change is worth saying "
            "something about.",
            "reason": "the operator asked for it to be watched and reported",
        }

    return (
        {
            "action": "assert",
            "key": key,
            "value": value,
            "confidence": 0.9,
            "rationale": why,
        },
        wake,
    )


DOORS = itertools.cycle(
    [
        "the door is closed",
        "the door is closed",
        "the door is open",
    ]
)


def tool_call(name, arguments, number):
    return {
        "id": f"call_{number}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


PLAN_STEPS = [
    "Work out how to look at the camera on the other computer, and write it down as a "
    "procedure so it never has to be worked out again.",
    "Set up a source that runs that procedure regularly, so the system hears what is in "
    "front of the camera without being asked.",
    "Tell the operator it is done.",
]


def door_definition():
    """A second sense, written by the system after its first one told it something."""
    look = {
        "name": "door.state",
        "description": "Look at the door sensor and report whether the door is open.",
        "parameters": {
            "type": "object",
            "properties": {"url": {"type": "string", "description": "where the sensor is"}},
            "required": ["url"],
        },
        "script": 'curl -sS --max-time 5 "$VELORA_ARG_URL"',
        "effect": "read",
    }
    watch = {
        "name": "door.watch",
        "description": "Keep looking at the door sensor, so the system knows when the door opens.",
        "skill": "door.state",
        "arguments": {"url": DOOR_URL},
        "every": "3s",
    }
    return look, watch


def camera_definition():
    """The steps it takes to hear from a camera nobody compiled in."""
    look = {
        "name": "camera.snapshot",
        "description": "Look at the camera on the other computer and report what is in "
        "front of it. The camera answers at $VELORA_ARG_URL with a few words.",
        "parameters": {
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "where the camera is"}
            },
            "required": ["url"],
        },
        "script": 'curl -sS --max-time 5 "$VELORA_ARG_URL"',
        "effect": "read",
    }
    watch = {
        "name": "camera.watch",
        "description": "Keep looking at the camera so the system hears what is in front of it.",
        "skill": "camera.snapshot",
        "arguments": {"url": CAMERA_URL},
        "every": "3s",
    }
    return look, watch


def did_this_turn(messages, name):
    """Whether this turn has already called a tool, so it should now conclude.

    A real model stops after saying something or finishing a step. A stub that
    keeps offering the same call runs to the step limit and does the same thing
    seven times, which looks like a system that cannot stop rather than a stub
    that will not. The turn begins at the last thing said to it: what came before
    that is a different turn.
    """
    last_ask = max(
        (index for index, message in enumerate(messages) if message.get("role") == "user"),
        default=-1,
    )
    for message in messages[last_ask:]:
        for call in message.get("tool_calls") or []:
            if call.get("function", {}).get("name") == name:
                return True
    return False





def finishing(request_messages):
    """Whether the system has told it there is one reply left."""
    system = next(
        (m.get("content") or "" for m in request_messages if m.get("role") == "system"), ""
    )
    return "last reply" in system


def answer(parsed, state):
    """The response to one request.

    What this stub is being asked is decided by *the task it was given*, which is
    the last thing said to it — not by the whole prompt. A real model reads the
    whole transcript; this cannot, and looking for a word anywhere in it would
    find the same word in a turn from ten minutes ago and do the same thing
    twice.

    `state` is this stub's memory across calls: whether the plan exists, how many
    of its steps have been worked through, and whether it has built a second
    sense yet. A real model would read all of that out of what it holds.
    """
    messages = parsed.get("messages", [])
    tools = [tool.get("function", {}).get("name") for tool in parsed.get("tools", [])]
    prompt = "\n".join(message.get("content") or "" for message in messages)
    task = task_given(messages)

    if "revise_belief" in tools:
        belief, wake = judge_answer(prompt)
        calls = [tool_call("revise_belief", belief, 1)]
        if wake:
            calls.append(tool_call("wake_reasoning", wake, 2))
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "The event states something about the operator, or about "
                "something a source saw.",
                "tool_calls": calls,
            },
            "tool_calls",
        )

    if PARALLEL and "operator_say" in tools and "waiting for an answer" in task:
        # One action per turn, then conclude — otherwise the stub says the same
        # thing on every call of the turn until the step limit stops it.
        if did_this_turn(messages, "operator_say"):
            return concluded(parsed, "Said.")

        if did_this_turn(messages, "local_shell_run"):
            return completed(
                parsed,
                {
                    "role": "assistant",
                    "content": None,
                    "reasoning_content": "That finished, so I will say so.",
                    "tool_calls": [
                        tool_call("operator_say", {"text": "ffmpeg is installed."}, 1)
                    ],
                },
                "tool_calls",
            )

        if "install" in what_prompted(messages).lower():
            # Something long. This turn waits for it; the system is free to
            # answer somebody else in the meantime.
            return completed(
                parsed,
                {
                    "role": "assistant",
                    "content": None,
                    "reasoning_content": "This will take a while, so I will run it and wait.",
                    "tool_calls": [
                        tool_call("local_shell_run", {"command": "sleep 8; echo installed"}, 1)
                    ],
                },
                "tool_calls",
            )

        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "Answering is a procedure.",
                "tool_calls": [tool_call("operator_say", {"text": "I am here."}, 1)],
            },
            "tool_calls",
        )

    if LONG and "operator_say" in tools and "waiting for an answer" in task:
        # Work for much longer than a turn used to be allowed. A different
        # command every step, so the circle check has nothing to say about it and
        # the only thing that can stop it is the model deciding to.
        if did_this_turn(messages, "operator_say"):
            return concluded(parsed, "Said.")

        worked = state["worked"]
        if worked >= 26:
            return completed(
                parsed,
                {
                    "role": "assistant",
                    "content": None,
                    "reasoning_content": "That is the whole job, so I will say so.",
                    "tool_calls": [
                        tool_call(
                            "operator_say",
                            {"text": f"Done, after {worked} steps in one turn."},
                            1,
                        )
                    ],
                },
                "tool_calls",
            )

        state["worked"] = worked + 1
        reviewed = "## Where this has got to" in task
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": (
                    "I have been asked whether this is still going somewhere. It is, so I "
                    "will carry on."
                    if reviewed
                    else "Still working."
                ),
                "tool_calls": [
                    tool_call(
                        "local_shell_run", {"command": f"echo grinding {worked + 1}"}, 1
                    )
                ],
            },
            "tool_calls",
        )

    if LONG and "plan_step_done" in tools and "mark it done" in task:
        if did_this_turn(messages, "plan_step_done"):
            return concluded(parsed, "That step is done.")
        index = state["worked"]
        state["worked"] += 1
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": f"Doing step {index + 1} of the plan.",
                "tool_calls": [
                    tool_call("local_shell_run", {"command": f"echo step {index + 1}"}, 1),
                    tool_call(
                        "plan_step_done", {"id": "burn", "note": f"step {index + 1} done"}, 2
                    ),
                ],
            },
            "tool_calls",
        )

    if "this thought is your own" in the_task(messages):
        # A quiet moment, and nobody is waiting. The usual answer is that nothing
        # needs doing, and saying nothing to the operator is part of it.
        return concluded(parsed, "Nothing needs doing.")

    if "plan_create" in tools and CAMERA_URL and not state["planned"] and "waiting for an answer" in task:
        # Something the operator wants is more than one thing to do, and it will
        # take several turns. Write it down.
        state["planned"] = True
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "This is more than one thing to do and I will be at it "
                "across several turns, so I will write it down as a plan and work through it.",
                "tool_calls": [
                    tool_call(
                        "plan_create",
                        {"id": "camera", "goal": "hear from the camera", "steps": PLAN_STEPS},
                        1,
                    )
                ],
            },
            "tool_calls",
        )

    if "plan_step_done" in tools and "mark it done" in task:
        if did_this_turn(messages, "plan_step_done"):
            return concluded(parsed, "That step is done.")
        return completed(parsed, *work_the_step(state))

    if "skill_define" in tools and "called for action" in task and DOOR_URL and not state["alerted"]:
        # Woken by a report from a source it built for itself, and the answer is
        # not only to say something: it is to build the sense it wishes it had.
        state["alerted"] = True
        look, watch = door_definition()
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "The camera told me somebody is at the door, and I cannot "
                "tell whether the door opens. I will write a procedure for the door sensor and "
                "set up a source for it, the way I did for the camera.",
                "tool_calls": [
                    tool_call("skill_define", look, 1),
                    tool_call("skill_observer_define", watch, 2),
                    tool_call(
                        "operator_say",
                        {
                            "text": "Somebody is at the door. I cannot tell whether the door "
                            "opens, so I have written a procedure for the door sensor and set "
                            "up a source that watches it; I will tell you when it opens."
                        },
                        3,
                    ),
                ],
            },
            "tool_calls",
        )

    if "operator_say" in tools:
        if did_this_turn(messages, "operator_say"):
            return concluded(parsed, "I have said what I did, so there is nothing left to do.")
        return completed(
            parsed,
            {
                "role": "assistant",
                "content": None,
                "reasoning_content": "Answering is a procedure.",
                "tool_calls": [
                    tool_call(
                        "operator_say",
                        {
                            "text": (
                                "Somebody is standing at the door — the camera I set up saw them."
                                if "called for action" in task
                                else "Noted; nothing needs doing about that."
                            )
                        },
                        1,
                    )
                ],
            },
            "tool_calls",
        )

    return concluded(parsed, "Noted.")


def task_given(messages):
    """The whole context this turn was opened with."""
    for message in reversed(messages):
        if message.get("role") == "user":
            return message.get("content") or ""
    return ""


def what_prompted(messages):
    """The event this turn was opened about, as the context shows it.

    The task is the same sentence every time — "the operator has said something"
    — so what was actually said is in the section below it, and that is what a
    stub has to read to tell one message from another.
    """
    context = task_given(messages)
    _, _, after = context.partition("## What prompted this")
    for line in after.splitlines():
        if line.strip() and not line.startswith("## "):
            return line.strip()
    return ""


def the_task(messages):
    """What this turn was asked, and nothing else.

    Not the whole context: a real model reads all of it, and a stub that looks
    for a word anywhere in it finds the word in something said three turns ago
    and does the wrong thing. The task is the first section of the context.
    """
    context = task_given(messages)
    _, _, after = context.partition("## The task")
    for line in after.splitlines():
        if line.strip():
            return line.strip()
    return ""


def work_the_step(state):
    """Do the plan step that is in hand, and mark it done."""
    look, watch = camera_definition()
    which = state["worked"]
    state["worked"] += 1

    if which == 0:
        calls = [
            tool_call("skill_define", look, 1),
            tool_call("plan_step_done", {"id": "camera", "note": "wrote skill.camera.snapshot"}, 2),
        ]
    elif which == 1:
        calls = [
            tool_call("skill_observer_define", watch, 1),
            tool_call("plan_step_done", {"id": "camera", "note": "camera.watch is running"}, 2),
        ]
    else:
        calls = [
            tool_call(
                "operator_say",
                {
                    "text": "The camera is set up: I wrote a procedure for looking at it, and a "
                    "source that looks every few seconds. I will record what it sees."
                },
                1,
            ),
            tool_call("plan_step_done", {"id": "camera", "note": "told the operator"}, 2),
        ]

    message = {
        "role": "assistant",
        "content": None,
        "reasoning_content": f"Working through step {which + 1} of the plan.",
        "tool_calls": calls,
    }
    return message, "tool_calls"


def completed(parsed, message, finish):
    return {
        "id": "stub",
        "object": "chat.completion",
        "created": 1,
        "model": parsed.get("model", "stub"),
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


def concluded(parsed, text):
    """A turn that has nothing left to do, which is how a real one ends."""
    return completed(
        parsed,
        {
            "role": "assistant",
            "content": text,
            "reasoning_content": "There is nothing left to do in this turn.",
        },
        "stop",
    )


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    out = "/tmp/velora-stub"
    counter = itertools.count(1)
    state = {"planned": False, "worked": 0, "alerted": False}

    def log_message(self, *args):
        """Silence: the captures are the log."""

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/shot":
            payload = next(FRAMES).encode()
        elif path == "/door":
            payload = next(DOORS).encode()
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("content-type", "text/plain")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length).decode()

        number = next(self.counter)
        with open(os.path.join(self.out, f"wire-{number:03d}.json"), "w") as captured:
            captured.write(body)

        payload = json.dumps(answer(json.loads(body), self.state)).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


def main():
    global CAMERA_URL, DOOR_URL, PARALLEL, LONG

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=7801)
    parser.add_argument(
        "--out",
        default="/tmp/velora-stub",
        help="where to write one captured request body per call",
    )
    parser.add_argument(
        "--parallel",
        action="store_true",
        help="run long steps for slow requests and answer everything else at once",
    )
    parser.add_argument(
        "--long",
        action="store_true",
        help="work for far longer than one sitting, a different step every time",
    )
    parser.add_argument(
        "--camera",
        action="store_true",
        help="answer the reasoning engine by defining a procedure and a source that "
        "looks at GET /shot on this same server",
    )
    arguments = parser.parse_args()

    os.makedirs(arguments.out, exist_ok=True)
    Handler.out = arguments.out
    PARALLEL = arguments.parallel
    LONG = arguments.long
    if arguments.camera:
        CAMERA_URL = f"http://127.0.0.1:{arguments.port}/shot"
        DOOR_URL = f"http://127.0.0.1:{arguments.port}/door"

    server = http.server.ThreadingHTTPServer(("127.0.0.1", arguments.port), Handler)
    print(f"stub openai listening on http://127.0.0.1:{arguments.port}", flush=True)
    print(f"capturing request bodies in {arguments.out}", flush=True)
    if CAMERA_URL:
        print(f"camera scenario on: GET {CAMERA_URL}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
