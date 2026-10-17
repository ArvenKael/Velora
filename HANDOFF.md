# Velora — Engineering Handoff

This supersedes the earlier handoff, which described four crates and has since
drifted. Read this before making changes.

---

## 1. What this is

Velora is an agent that operates a computer. The intended architecture is
modular: an OS-agnostic core, separate from OS-specific runtimes, with an
eventual ambition of its own OS. Treat the OS as a distant side quest; it is not
a near-term design constraint.

One note on the README: "MITM" reads as man-in-the-middle, a security-attack
term. It should be reworded before this is described to anyone outside the
project. The README also has typos.

## 2. Where things stand

Nineteen crates, all building, all tested, no warnings:

| Crate | Responsibility |
|---|---|
| `velora-core` | Placeholder. Intended kernel; nothing depends on it yet. |
| `velora-log` | Logging facade over `tracing` + `tracing-subscriber`. |
| `velora-event` | Durable, ordered, append-only history, and the `Store` seam every backend implements. |
| `velora-storage` | The storage backends: a file, a SQLite database, memory, and the runtime choice between them. |
| `velora-operator` | What the operator said, and the channel the system says things back through. |
| `velora-web` | The screen: a live view of everything the system is doing, and a way to talk to it. |
| `velora-loop` | The always-on loop: ingress, durability, stage pipeline, feedback, history reading. |
| `velora-belief` | Evidence-backed beliefs, as a projection of history. |
| `velora-mind` | Deciding what an event means for belief, by rule or model. |
| `velora-context` | Assembling what the model is shown, from beliefs *and the events behind them*. |
| `velora-exec` | Devices, procedures, execution policy, and the computer it runs on. |
| `velora-config` | What the system is configured to be, folded from decisions it recorded. |
| `velora-llm` | The main thinking model; provider-agnostic, with a swappable engine. |
| `velora-reason` | Thinking, as durable turns where every thought and step is an event. |
| `velora-agent` | The resolver: belief changes become thoughts. |
| `velora-capability` | Procedures and observation sources the system defines for itself. |
| `velora-plan` | Durable intentions: a goal, steps, and what is left to do after a restart. |
| `velora-pulse` | A source that fires when nothing has happened: thinking nobody asked for. |

`velora-bin` wires them into a process.

## 3. The architecture, layer by layer

```
   sources ──▶ velora-loop ──▶ velora-mind ──▶ belief.* ──▶ velora-belief
   (adapters)   append first    judge: rule or    events       fold into beliefs
                then dispatch    model decides                       │
                    │                                                ▼
                    │                                     velora-agent (resolver)
                    │                                  a rule says this matters;
                    │                                  the situation is assembled
                    │                                  from beliefs + their events
                    │                                                │
                    │                                                ▼
                    │                                   reason.requested ──▶ velora-reason
                    │                                                            │
                    │                                     reason.thought  ◀──────┤
                    │                                     step.requested  ───────┤
                    │                                                            ▼
                    │                                                  velora-exec runs it
                    │                                                            │
                    │                                     step.completed  ◀───────┘
                    │                                                │
                    │                                     the reasoner is woken by
                    │                                     the result and thinks again
                    │
                    └── velora-event (the only thing that cannot be rebuilt)
                        └── velora-storage picks where it is kept:
                            sqlite, a file, or memory
                        └── velora-conversation is the dialogue with
                            the operator, also just events
                        └── velora-web shows it all, and submits what
                            the operator types as an ordinary event
                        velora-config folds the system's decisions about itself
                        velora-llm provides the thinking; velora-context what it sees
```

**The loop is the agent loop.** A turn of thought is not a conversation held in
a task while the system waits for it — it is a sequence of events. Think, act,
observe, think, each through the loop, so each is durable and reviewable, and a
turn interrupted by a crash carries on from exactly what it had been given.

**`velora-event`** owns exactly four operations: `append`, `replay`, `digest`,
recovery (which *is* replay). The total order is over `(time, sequence)`;
wall-clock time is data, sequence is truth. Payloads are opaque versioned bytes.
Do not add a fifth operation, a significance policy, a domain model, retention,
or a consumer concept as a top-level field. `Sensitivity` is the one exception,
because governance is orthogonal to meaning.

**`velora-loop`** owns the log exclusively, so it owns the sequence. Every event
is appended *before* any stage sees it. Stages may emit, and their output is
appended behind its cause; derivation depth is capped. A stage may also work
asynchronously: it hands a question to a task and sends the answer back through
the loop's **feedback channel**, reporting `Stage::is_idle` honestly so the loop
does not stop and discard an answer in flight.

**`velora-belief`** is a projection. Beliefs are keyed claims with an opaque
versioned value, a confidence in basis points (integers: a projection rebuilt by
replay must be identical everywhere), and **evidence, which is event sequences**.
Evidence cannot be empty, and must be strictly earlier than the event citing it
— so a belief can never rest on its own conclusion. Disagreements and failed
withdrawals are kept, never silently dropped. Revision, including whether a
withdrawal succeeds, is policy supplied by the caller.

**`velora-mind`** decides. A rule claimed by a `BeliefRule` is decided
deterministically and never reaches a model; named candidates go to the model.
The decision is then **recorded as an event**, which is the load-bearing idea:

> The model is consulted once, at ingest, and never during a fold. If the belief
> stage asked the model while replaying, replay would consult a non-deterministic
> process and could produce different beliefs each time — the store would stop
> being a function of history, and the premise would collapse.

A model is necessary for full function and must never be quietly worked around,
so an event that needs one is taken, appended, and **held**: it changes no belief
until something has actually judged it. A model that is merely unreachable is
waited for — the judge retries with a growing delay, says so at intervals rather
than once, and returns to full function the moment an answer arrives, with no
restart. Only an answer that could not be used at all is given up on, because
asking again would not change it.

An event left undecided stays in history; the next start finds candidate events
that no decision cites and judges those. Because a decision records what it was
drawn from, no separate ledger is needed.

**`velora-llm`** is a service, not a chat client, and depends on neither the
loop nor the log. Its seam is `Provider`, which knows nothing about transport:
`OpenAiCompatible` speaks HTTP (covering OpenAI, xAI, Groq, OpenRouter, Together,
and local runtimes), and `CommandProvider` runs a program, which is the shape a
subscription wrapper takes. `ApiKey` redacts itself — this system writes into a
history that is never rewritten, so a credential in a log line would be
permanent.

## 4. Invariants that must not be broken

1. **History is appended before anything observes an event.** An append failure
   stops the loop rather than letting the system act off the record.
2. **The model is never consulted during a fold.** Decisions are recorded; replay
   folds records.
3. **Beliefs are grounded.** No evidence, no belief.
4. **Nothing is silently dropped.** Torn history, refused beliefs, lost
   disagreements, refused withdrawals and capping are all reported.
5. **Sequence is the only order.** Never sort by timestamp.
6. **Frozen encodings change only by migration.** Record framing, and every
   payload type in `velora-belief`, `velora-exec`, `velora-config` and
   `velora-reason`, are pinned by tests. Changing one means a new version plus an
   upcast, and the old shape stays pinned.
7. **Replay never re-runs a side effect.** The executor does not rebuild from
   history, so restarting folds recorded results and executes nothing. For a
   system with a shell, that difference is the whole safety argument.
8. **A turn is its events.** The assembled situation travels with the request, so
   a resumed turn continues from what it was given rather than from whatever the
   system believes now, and tool-call ids derive from the turn and step ordinal
   so a rebuilt conversation is the same one.
9. **A backend is never less durable than the one it replaced.** Reaching for a
   database to go faster must not quietly trade away the flush. This is measured
   rather than assumed: matching SQLite's flush to the file store's moved appends
   from 0.1 ms to 4.2 ms and left every other operation untouched, which is why
   `SqliteStore::durability` is printed at start.
10. **Credentials cannot be set by an event.** `Setting::parse` is an allow-list
   and `ConfigChange::new` refuses a credential before the event exists. The loop
   appends before any stage sees an event, so a check at application time cannot
   protect the log; the steward refuses anyway, so such a change is never *acted
   on*.

## 4a. Talking to it

Start it and open the address it prints:

```sh
VELORA_LOG=info VELORA_LLM_PROVIDER=command VELORA_LLM_MODEL=fake \
VELORA_LLM_COMMAND=scripts/fake-model.sh cargo run -p velora-bin
# -> open this to watch and talk to it  url=http://127.0.0.1:7788
```

The screen has three panels: the conversation, a live timeline of every record
newest-first, and the state (beliefs, the model, the durability promise, the
procedures it can run). Click any record to expand it; for a `reason.requested`
that shows **the exact context the model was given**, which is the most useful
thing in the system for working out why it did something.

`VELORA_UI=off` runs it headless. `VELORA_UI=host:port` moves it. It is
loopback-only by default, deliberately: the page can run shell commands on this
machine through the system it is showing.

Note what the screen changes: while it is open it holds an ingress sender, so the
loop no longer stops when the other sources finish. That is the point of a screen
— the system stays up to be talked to — and it means a run with the screen on
ends by being asked to.

Two ways to check the screen, and they catch different things.

```sh
node scripts/check-screen.mjs            # against a running system, no browser
```

Runs the real `index.html` and `app.js` against the real API with a small DOM in
place of a browser. It has no layout and no CSS — it is the code path, not the
picture — but it needs nothing installed and it works anywhere.

```sh
chromium --headless=new --remote-debugging-port=9223 \
    --user-data-dir=/tmp/velora-cdp --no-sandbox about:blank &
python3 scripts/screen-in-browser.py                    # look
python3 scripts/screen-in-browser.py --say "who are you?"   # and talk
python3 scripts/screen-in-browser.py --close            # tidy up tabs
```

A real browser over CDP: a real mouse press and release at the button's real
coordinates, then what the page says afterwards. It needs a browser, and it is
worth the trouble — three defects were found this way and by nothing else:

- a message drawn as `#0`, because `Number(null)` is `0` and `isFinite` said so;
- every message drawn twice, because the page invented a placeholder and then
  could not recognise the real record when it arrived;
- a belief panel rendering four hundred evidence chips, because a belief gathers
  evidence for as long as it is reinforced.

The third is the reason the screen no longer draws a message until it is in
history. The lesson is in the second: a page that guesses what the system did
will eventually show something the system did not do.

## 5. Building, testing, running

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets

# Run the demo. Without a model, rules still decide what they cover.
# The placeholder source only runs when asked for; a real system must not be
# inventing observations about an operator who keeps asking about the weather.
VELORA_LOG=info VELORA_DEMO_EVENTS=4 cargo run -p velora-bin

# Where history is kept. Defaults to the database under target/velora/.
VELORA_STORAGE=sqlite cargo run -p velora-bin     # the default
VELORA_STORAGE=file   cargo run -p velora-bin     # one append-only file
VELORA_STORAGE=memory cargo run -p velora-bin     # scratch: nothing survives

# Where a database actually pays for itself, re-measurable on your machine.
cargo test -p velora-storage --test timings -- --ignored --nocapture

# Run the whole loop with no credentials: a script stands in for the model, and
# everything else is real — the shell command it asks for really runs.
VELORA_LOG=info VELORA_DEMO_EVENTS=4 \
VELORA_LLM_PROVIDER=command VELORA_LLM_MODEL=fake \
VELORA_LLM_COMMAND=scripts/fake-model.sh \
cargo run -p velora-bin
```

With that, the demo produces: an observation becomes a belief, the belief change
opens a turn, the model asks for a shell command, the command runs, and the
conclusion is about what it printed — `reason.requested`, `reason.thought`,
`step.requested`, `step.completed`, `reason.finished`, in that order.

## Configuration

Read once, at the start. Nothing reloads: a running system that changed its
model underneath itself would be harder to reason about than one that was
restarted.

Three places, lowest precedence first:

1. `~/.config/velora/config.toml` — the machine-wide file. **Credentials belong
   here**, because it can never be committed.
2. `./velora.toml` — the project file. `velora.toml.example` documents every key.
3. The environment — `VELORA_LOG`, `VELORA_STORAGE`, `VELORA_HISTORY`,
   `VELORA_IMPORT_FROM`, `VELORA_MAX_BELIEF_CONFIDENCE` (basis points, or
   `certain`; see §6a2), and the `VELORA_LLM_*` family (see `velora_llm::config`
   for the list and defaults).

A misspelled key is refused rather than ignored, because a setting that silently
does nothing is the failure this file exists to prevent.

**`--setup` asks, whenever you want it to.** The first-run prompt only appears
when something required is missing *and* stdin and stdout are both terminals, so
someone who wants to give Velora a key — or who runs it from a launcher, a
script, or a pipe — never sees it. `--setup` runs the prompt anyway, on a
configured system or not:

```sh
cargo run -p velora-bin -- --setup
```

It offers `openai`, `deepseek` and `command`. Naming an endpoint fills in its
base URL and model, so a DeepSeek key is two Enters and the key:

```text
Provider (openai, deepseek, or command for a subscription CLI) [openai]: deepseek
Model [deepseek-flash]:
API base URL [https://api.deepseek.com]:
API key: sk-...
```

What is written is the *shape* of the API, not the vendor — `provider = "openai"`
with `base_url = "https://api.deepseek.com"` — because that is what the loader
accepts, and what makes it DeepSeek is the URL. Any OpenAI-shaped endpoint works
the same way, which is what `velora.toml.example` says.

The key goes to `~/.config/velora/config.toml` (or `$XDG_CONFIG_HOME`), mode
0600 inside a 0700 directory, and never to the project file.

## 6a2. The judge records; it does not filter

The judge is asked **what an event shows to be true**, not whether it is worth
believing. That distinction is the whole of it, and getting it wrong is why
telling Velora about a new lamp produced no belief at all.

The prompt used to ask whether an event *required* creating or changing a belief,
and said plainly: **"If the event changes nothing, record nothing at all. That is
the normal case."** That is a filter for significance with silence as its declared
default, and a significance filter answers "no" to a plain statement of fact:
being told a lamp exists says nothing about whether it *matters*, so nothing was
recorded, and the lamp was never known about.

Whether something matters is a later question, asked by the resolver and the
reasoning engine — and neither of them can ask it about an event that was never
turned into a belief. So the judge records what is stated, and the prompt now says
so: an event that states something is evidence for it; the operator's word about
their own belongings is the only evidence there will ever be for it; an empty
answer means the event says nothing about anything, "which is rarer than it
sounds"; and a second statement should reuse the key already held rather than
start a parallel belief beside it.

Confidence for anything a model concludes is capped at 8000 (80%) by
`JudgeConfig::max_model_confidence`, deliberately: the cap stops a model's
*inference* from outranking what a **rule** established from the event itself —
the `key=value` observations that rules handle are held at 9000. A statement by
the operator is not an inference, and 80% is what this system will claim on a
model's word.

That is a policy, not a law, and it now has a lever: `VELORA_MAX_BELIEF_CONFIDENCE`
takes basis points or `certain`. `VELORA_MAX_BELIEF_CONFIDENCE=10000` says that a
direct statement by the operator may be held outright, and the cost of saying it
is exactly the one the cap was there to prevent — a model that misreads a message
can then assert something that outranks a rule. The log is the record of what was
actually said; a belief is the interpretation, and how much weight the
interpretation may carry is a choice this file will not make for you.

## 6a3. A decision to record nothing is still a decision

Every decision the judge makes is recorded — including the decision that an event
says nothing. `belief.considered` cites the event it is about, exactly as an
assertion cites its evidence, which is what lets the catch-up sweep tell "not yet
judged" from "judged, and there was nothing to say".

Without it those two states are identical, and the cost is not theoretical: the
sweep runs at every start, goes **oldest first**, and asks a model about
everything history has not decided. A backlog of events decided as nothing grows
without bound and is re-judged, at a model call each, *ahead of everything new*.
A person's message arrives at the back of a queue that never drains — and looks
exactly as though it was never read.

```sh
# Before: every start re-judged the same events.
velora_mind::stage: judging events that were never decided events=1
# After: nothing is left to decide.
```

## 6a4. An answer that could not be used is not an answer of "nothing"

The judge has two ways to end up with no decisions: the model said nothing
changes, or the model said something the parser could not read. Those are
opposite outcomes and they used to be the same — a tool call with unreadable
arguments, a call to a tool that is not the decision tool, a decision missing its
subject: each was skipped with a warning, and the caller then recorded
`belief.considered`, "nothing to record".

So a decision the model actually made was thrown away **and** the event was marked
decided, which meant it was never looked at again. The belief was lost in
silence, and the record said there had been nothing to record.

`parse` now reports what it could not use. A judgement that yields nothing
*and* had something unusable in it is an error, not an empty answer, so the event
stays undecided and is asked about again. `{"action":"none"}` — not in the schema,
and said anyway — is understood as the answer it is, and needs no subject.

## 6a5. The tool the prompt named was never offered

The judge had exactly the right tool defined and never put it in the request. For
a provider with `capabilities().tools` — the OpenAI-shaped one, which is DeepSeek
— `build_request` set the system prompt to "say what the event supports, as
beliefs: what it is about, what it says, how strongly, and why", and attached no
tools at all. Nothing in that prompt is JSON-shaped. So the model answered in a
sentence, the sentence had no `{` in it, and the old parser read "words I cannot
use" as "nothing to record".

Three faults stacked, and the third is the one that explains the report:

1. The prompt asked for prose by omission — `JSON_PROTOCOL` was only appended when
   the provider *could not* call tools, and `TOOL_PROTOCOL` did not exist.
2. The tool was never attached, so even a model that wanted to call
   `revise_belief` could not: the request named a function the provider had not
   been told about.
3. A prose answer was `Ok(vec![])` — indistinguishable from a decision of
   "nothing" — so the event was marked considered and never asked again.

The fix is in `build_request` and `build_reconsideration_request`: when the
provider supports tools, the request carries `decision_tool()`, and the prompt
says so (`TOOL_PROTOCOL`: call `revise_belief` once per belief, and reply with the
single word `none` if the event supports nothing). Prose is now
`JudgeError::Unreadable` and leaves the event undecided, which is the honest
outcome: an answer that cannot be read is not an answer of "nothing".

Two tests pin the whole path rather than the helper: a `Scripted` provider with
`Capabilities::FULL` asserts the request it received had exactly one tool and that
the belief was recorded, and a provider that flattens to text asserts it was
offered none.

This is the fourth time a bug has had the same shape — **"said nothing" versus
"said something we dropped, could not read, or never asked for"** — after the
reply pairing, the considered marker, and the unreadable answer. It is the first
thing to check against any new silence.

## 6a6. A confidence is read in the notation it was written in

The decision schema says `confidence` is an integer in basis points, and the
field was `Option<u16>`. A model that wrote `0.9` — which models do, constantly —
made the whole decision fail to deserialize, so it landed in the same pit as
everything else: the belief was lost, the event was marked considered, and
nothing was asked again. A belief thrown away over the notation of a number is
indistinguishable, from the outside, from a model that said nothing.

`read_confidence` reads the notation instead: `8500`, `0.9`, `"0.9"`, `"90%"` and
`"8500"` are all the same claim, and a number of at most one is read as a fraction
of certainty, because one basis point is not something a model means. Anything
that cannot be read as a number at all is not an error either: the judgement is
recorded with the configured default confidence, because the belief is the part
that matters and the answer looked fine. A value written as a number (`"value": 2`)
is read the same way — as text — for the same reason.

## 6b1. What the judge is shown

The judge decides what an event means for what Velora believes, and it was being
shown the event's **canonical bytes**. For a message that is a length-prefixed
payload, so what reached the model was:

```text
kind: conversation.message
content: \0\0\0\x1cI have a lamp just installed
```

Four bytes of binary glued to the front of the sentence — and for any payload
whose value is not text, real garbage. This is why telling Velora something did
not reliably become a belief: it was being asked what an event meant while being
shown the event's encoding.

The judge now reads through the same `Explain` seam as the context assembler and
the screen. One vocabulary, three readers, and none of them can disagree about
what the system did. The regression test asserts that what reaches the model
contains no length prefix, and pins the fallback's behaviour too, so the
difference is visible rather than argued about.

## 6b2. What a provider is sent

Procedure names are dotted, because they read as paths: `local.shell.run`. **No
OpenAI-shaped API accepts that.** The function-name pattern is
`^[a-zA-Z0-9_-]+$`, and a dot is refused with a 400 before the model sees
anything:

```text
Invalid 'tools[0].function.name': string does not match pattern.
Expected a string that matches the pattern '^[a-zA-Z0-9_-]+$'.
```

So the wire name is the procedure's name with its separators flattened —
`local_shell_run` — and `protocol::wire_name` is the only place that knows it.
The dotted name stays in the record, because that is what a person reads in the
log and on the screen. `protocol::resolve` turns a name from a model back into a
procedure by *looking* rather than by reversing the substitution: two procedure
names can flatten to the same wire name, and un-flattening would quietly run one
of them.

The conversation and the record differ on purpose, and this is the one place
they must: the assistant tool call keeps the name the model used, because that
is what it said and what it will be shown again, and the `step.requested` keeps
the procedure's own name, because that is what ran. A replayed turn therefore
converts back on the way in — a rebuilt conversation that used the dotted name
would be one that never happened, and one the provider rejects.

## 6c. What has actually been reached

The OpenAI-shaped provider has now been pointed at DeepSeek's live endpoint over
the network, with a deliberately wrong key. What came back was DeepSeek's own
error, verbatim:

```text
provider returned status 401: {"error":{"message":"Authentication Fails, Your api
key: ****ting is invalid","type":"authentication_error", ...}}
```

That is worth recording because of what it rules out. DNS, TLS, the URL the
client builds, the request shape and the `Authorization` header are all accepted
by the real API; the only thing it objected to was the credential. So the wire
format is not merely correct against a stub server. What has *not* happened is a
successful completion: no real model has ever answered Velora, so nothing is
known about how it behaves with one.

The 401 was not retried — it is not a retryable failure — and the judgement it
blocked was held rather than dropped, which is the behaviour running without a
model depends on.

**The whole path has since been run against a stub OpenAI endpoint, with the real
binary.** Real process, real SQLite history, real web server, real OpenAI-shaped
provider over HTTP; the model is a 40-line Python server that captures every
request body. A message typed into the screen — `u know I really love red.` via
`POST /api/say` — produced this, in order:

```text
#1 operator.said      u know I really love red.
#2 (to the judge)     tools: ["revise_belief"]
                      content: u know I really love red.
#3 belief.assert      operator.loves = red   (80.00%)  ← capped from the 0.9 the stub claimed
```

The captured judge request carries the tool, the sentence rather than its
encoding, and the belief lands in the store: `GET /api/snapshot` returns
`operator.loves = red 80.00% the operator said they love red`. Re-running with
`VELORA_MAX_BELIEF_CONFIDENCE=10000` returns 90.00%, which is the stub's own
claim — so the ceiling is the only thing that was holding it back. Both the
tool-attachment fix and the confidence-notation fix are verified on the wire, not
merely in unit tests.

The stub is `scripts/stub-openai.py`, and it is in the repository on purpose: it
is how this was reproduced, and how to reproduce a report of "my message was never
judged" without spending a request. It writes one captured request body per call,
so what Velora put on the wire can be read directly. Its docstring has the four
environment variables that point the real provider at it. `scripts/fake-model.sh`
is the other stand-in — for the `command` provider, which cannot do tools.

```toml
[llm]
provider = "openai"          # or "command", for a subscription CLI
model = "gpt-4o-mini"
base_url = "https://api.openai.com/v1"
api_key = "sk-..."           # machine-wide file only
```

If nothing is configured and a terminal is attached, Velora offers a first-run
prompt and can write the machine-wide file itself, readable by its owner alone.
The prompt never runs unattended (`VELORA_NO_PROMPT=1` turns it off), and it is
never required.

Two switches shape the failure behaviour:

- **Absence is a mode.** With no model at all, Velora runs, rules still decide
  what they cover, and events that need a model are held. This is reported at
  startup and repeated on an interval by the judge until it is fixed.
- **`VELORA_REQUIRE_MODEL=1`** makes a model that is missing *or unusable*
  fatal, for anyone who would rather nothing ran half-configured. A configuration
  file that exists but cannot be honoured is reported loudly either way.

Credentials are never logged, and a credential found in the project file draws a
warning pointing at the machine-wide one.

The demo history lives under `target/`, so running it does not dirty the repo.

## Where history is kept

Storage is behind the `Store` seam — `read`, `append`, and a handful of questions
with honest slow defaults — so nothing above `velora-storage` knows which backend
it got. Adding Postgres is another variant and another module, not a change to
the log.

`VELORA_STORAGE` is `sqlite` (the default, at `target/velora/history.db`), `file`
(`target/velora/history.log`), or `memory`. `VELORA_HISTORY` overrides the path.

Every backend stores the *canonical* record bytes rather than re-deriving them
from columns, so a digest of the same history is identical whichever store it
came out of — asserted by a test, not assumed. What the database buys is the
questions *other* than append: recovery on start is a count instead of a scan
(60x), counting is 700x, a point lookup is 700x, a range is 16x, and appending is
a wash because both stores flush once per record. Those figures are from
`tests/timings.rs` on one machine; re-run it rather than trusting them.

Changing backend does not cost you history. The first time an empty store is
opened, an existing file history beside it is moved in — announced, with the
digest compared afterwards, and only ever into an *empty* store so it cannot
overwrite anything. An interrupted import resumes instead of duplicating. The old
file is left where it is and is not written to again.

## 6. Environment notes

- Rust 1.98, `edition = "2024"`, workspace `resolver = "3"`.
- The first build needs crates.io access. Under a sandbox that blocks writes
  outside the workspace, cargo cannot populate `~/.cargo/registry`; that path
  needs to be writable.
- There is a root `.gitignore` ignoring `target`.

## 6a. There is no conversation

A conversation is a kind of thing, and this system does not have one. What it has
is an event — `operator.said`, submitted by the screen the way any adapter
submits an observation — and a procedure, `operator.say`, which is how the system
reaches a person. Both are ordinary parts of the system:

```text
#38 operator.said      who are you?
#42 step.requested     wants to run operator.say {"text":"I am Velora. ..."}
#43 step.completed     told the operator: I am Velora. ...
```

Ask about the directory and it looks *first* — `local.shell.run`, and only then
`operator.say` — and answers from what the command printed.

Nothing is wired. An event arrives, the judge decides what it means for what
Velora believes, the resolver decides whether it is worth acting on, and the
reasoning engine decides what to do about it — which may be to look something up,
to say something, or to do nothing at all. There is no reply paired with a
message, no `replies_to`, no transcript record, and no branch in the resolver for
any particular kind:

- **Acting on an event is a rule**, configured in the runtime like every other
  decision about what to pay attention to: `with_event(EventWatch::kind(...))`,
  with the task saying what to do about it and which channel to answer through.
- **What has been said is derived**, never stored: the operator's words are an
  event they submitted, and the system's are what a step of speaking reported.
  The context assembler reads the same two, which is why the model is shown a
  dialogue without anything in the system being one.
- **A model that writes prose and stops has said nothing to anybody.** That is
  the point. When a turn ending automatically produced a reply, every thought
  became a remark whether or not it had anything to say and whether or not anyone
  was listening.

The crate is `velora-operator` rather than `velora-conversation` for the same
reason: the old name described a thing that should not exist.

## 6b. What a record says

Payloads are canonical bytes, which is right for the log and unreadable
everywhere else — a belief is a claim, a confidence, a list of sequences and a
rationale, and as text it is noise with control characters in it. `Explain` in
`velora-context` is the seam: the assembler decides where a description goes and
how much fits, and the runtime supplies the vocabulary
(`crates/velora-bin/src/explain.rs`, which knows every payload shape).

One description serves the model *and* the screen. That is the point of it being
one function: what the operator sees a record say and what the model was told it
says cannot drift apart.

## 6d. Where the operator channel is not enough

The judge sees one event at a time and no dialogue. "I have a lamp just
installed" is a sentence it can now read; "and it is in the living room" is not,
because there is nothing to attach it to. Facts that arrive across several
messages — which is most of what a conversation is — are the reasoning engine's
to notice, because it is the only part that is shown the transcript.

That is the shape of the next step rather than a defect in this one: the
reasoning engine can already act through procedures, so letting it *remember*
something is a procedure of the same kind as saying something.

## 6e. Thinking is given back

DeepSeek's thinking mode is **on by default**, returns the model's chain of
thought beside its content as `reasoning_content`, and requires it back:

> If the request carries the `tools` parameter, the `reasoning_content` of all
> previous turns should be passed back to the API… If your code does not
> correctly pass back `reasoning_content`, the API will return a 400 error.

So it is part of the conversation, not something read and dropped. It is carried
on the assistant message, which also settled a second thing: the API's own
example appends **one message** holding `content`, `reasoning_content` and
`tool_calls` together, which is what the model actually produced.

It is durable, because a turn is its events. `reason.thought` is version 2 and
carries the thinking beside the words, upcast from version 1 with none recorded —
there was none kept then, and history cannot be rewritten to add it. It is
written even when the model said nothing at all, because thinking without words
is ordinary and the thinking still has to come back.

A provider that does not think sends no such field and none is invented:
`reasoning_content` is omitted from the wire when there is nothing to send.

## 6f. A response is one message

A model that asks for two things at once has said one thing, and the
conversation has to say so: **all of a response's calls go into a single
assistant message**, followed by the answers to each. Every OpenAI-shaped API
requires it — a `tool_calls` message must be answered call for call — and
recording each call as its own message produces a conversation no provider will
read:

```text
assistant(tool_calls: [A])     # asked for, never answered
assistant(tool_calls: [B])     # the answers below belong to B, not A
tool(B), tool(A)
```

The refusal arrives as a 400 about `tool_call_id` that says nothing about where
the hole came from, and looks like the model misbehaving. History keeps one
record per call, so replay puts a response back together: **a run of calls with
no answer between them was one response**, which is sound because the loop only
asks the model again once every outstanding step has come back.

A refusal is settled by id, not by order — it happens while earlier calls in the
same message may still be running, and answering by order would settle the wrong
one.

There is a check for this over real data:

```sh
cargo test -p velora-reason --test real_turn -- --ignored --nocapture
```

It reads `target/velora/history.db` if there is one, replays *every* turn in it,
and asserts that no assistant message asking for calls is left unanswered. On the
history that contained the bug it reports 22 turns checked and none broken.

## 6g. Silence is never the answer

A thought that ends without the operator hearing anything is reported, through
the channel, because the channel is the only way anything reaches a person:

```text
#28 reason.failed      gave up after 8 steps: it reached the 8-step limit
#29 step.requested     wants to run operator.say {"text":"I could not answer that: ..."}
#30 step.completed     told the operator: I could not answer that: ...
```

Whether to answer is the model's decision, and one that used the channel has
already been heard. What the system will not do is leave a question looking like
one nobody read. The check is made live and again at start, so a process that
stopped between the thought ending and the report being sent still says it; and it
is idempotent, because the first thing it looks for is the channel step that a
report would have produced.

## 6h. What the system makes for itself

Everything else in Velora has a fixed reach: the shell, the filesystem, the
operator's channel. `velora-capability` is the one place where the reach can
*grow*, and it holds two things.

A **skill** is a procedure with a name, a description, a parameter schema and a
script. Once defined it is a procedure like any other — the model is offered
`skill.<name>` as a tool, calls it when it wants it, and the executor runs it
exactly as it runs `local.shell.run`. This is deliberately not a new power:
anything a skill can do, `local.shell.run` could already do. What it adds is
**memory**. The second time the system needs the camera, it does not work out how
to reach it again; it calls what it worked out the first time.

An **observer** is a source. It runs a skill on an interval and reports what came
back as an `observation.self` event, which is the thing that was impossible
before: an adapter that did not exist until the system wrote one. A camera, a
mailbox, a directory that fills up, a build that keeps failing — the same loop,
with the skill and the interval being the only differences.

### Definitions are events

A definition is not state that lives in the new crate. The model calls
`skill.define`; the device validates it and hands back what it understood; the
executor records the step result; `CapabilityStage` turns that result into
`skill.defined`, which is the durable form; and the stage folds it back on the
next start. The live registry is therefore a projection of history, rebuildable by
replay, and a skill defined on one machine is defined on every machine that reads
the same log. The definition exists twice in the log on purpose: the request is
what was asked for, the `skill.defined` is what the system decided to hold — the
same relationship a belief has to the event behind it.

### Order is not the caller's problem

The device holds a definition **as soon as it is made**, not when the fold reaches
it, and that is not an optimisation. A model that defines a procedure and then
defines a source that runs it does both in quick succession; a registry that only
filled in on the fold would refuse the second step for depending on the first. A
definition in flight that history never recorded disappears with the process,
which is the correct outcome — it was never a definition, it was an attempt.

### What was actually reached

Run against the stub (`scripts/stub-openai.py --camera`), with the real binary,
real SQLite, real web server and the real OpenAI-shaped provider. One message
typed into the screen:

```text
operator:  I have a camera. It is on localhost of computer B at port 7821.
           Set yourself up so you can look at it.
velora:    I have written a procedure for looking at the camera and set up a
           source that looks every few seconds. I will record what it sees.
```

and then, with nobody touching it again:

```text
skill.defined       defined skill.camera.snapshot — Look at the camera on the other
                    computer and report what is in front of it…
observer.defined    set up camera.watch to run skill.camera.snapshot every 3s — Keep
                    looking at the camera so the system hears what is in front of it.
observation.self    5 × over the next 15 seconds, from camera.watch
belief.assert       camera.saw = an empty hallway (80.00%)
```

Nothing in the runtime knows what a camera is. The step that made it happen was
one skill definition and one source definition, both written by the model, both
remembered, both still running after a restart.

### A chain: its own report makes it build another sense

The step that matters is the one where the system is not told anything new by a
person, but by something it made. Run with the stub, one message and then nothing:

```text
operator          I have a camera at localhost:7871. Set it up, and tell me when
                  you see a person.
plan.created      made a plan, camera: hear from the camera (3 steps)
skill.defined     defined skill.camera.snapshot
observer.defined  set up camera.watch to run skill.camera.snapshot every 3s
step.completed    told the operator: The camera is set up…
…
observation.self  camera.watch reported: a person standing at the door
wake.granted      decided this needs doing: Somebody is in front of the camera…
skill.defined     defined skill.door.state — Look at the door sensor and report
                  whether the door is open.
observer.defined  set up door.watch to run skill.door.state every 3s
step.completed    told the operator: Somebody is at the door. I cannot tell whether
                  the door opens, so I have written a procedure for the door sensor
                  and set up a source that watches it; I will tell you when it opens.
…
belief.assert     door.state = the door is closed (80.00%)
```

Two sources, both written by the system, both running, the second one built
because the first one told it something it could not act on. Nothing in the
runtime knows what a camera or a door is, and nothing was recompiled.

### Being told it can

The tools described all of this, and the standing rules did not — so the only
thing that told a model it could extend itself was a description attached to a
tool it might never look at. The rules now say it in as many words: what it can do
is not fixed, it is expected to add to it, and keeping an eye on something is what
a source is for. Pinned by a test, because it is the kind of line that gets
dropped in a prompt edit and whose absence is invisible.

### The decision that goes with it

A definition is not the whole story. A source that reports and is never acted on
is a diary, and the thing that was missing was the other half of the judge's
answer: not only *what does this event show to be true*, but *does it call for
something to be done*.

That decision is now made where the event is read, on the call the judge was
already making — see §6i.

## 6i. Whether an event needs doing something about, decided per event

The judge reads every event no rule covers. Reading an event is almost all of what
deciding whether to *act* on it needs, so the second decision is made in the same
call as the first: `wake_reasoning` is a second tool beside `revise_belief`, and
its answer is recorded as `wake.granted`, citing the event it is about.

What this replaces is a list. Until now the resolver held the kinds somebody
compiled in — belief keys by prefix, and "the operator said something" — so what
the system paid attention to was fixed at build time. That is fine right up until
the system invents a source of its own, at which point it has no way to say that
*this* report, unlike the last hundred, is the one that matters. A camera that
sees a person is now a thing that can happen rather than a thing that cannot.

Three properties make it work:

- **A denial is recorded too.** No wake and no belief is a `belief.considered`; no
  wake with beliefs is just the beliefs. Either way the event is decided, so the
  catch-up sweep does not ask about it again — and asking again would act twice.
- **It costs nothing.** The decision rides on a call the judge was making anyway.
  The alternative — a stage that asks a model whether to wake a model — is a
  second round trip per event for an answer that the first round trip already had
  in front of it.
- **A wake with an unreadable belief beside it is still an answer.** Normally a
  decision that cannot be read leaves the event undecided and it is asked about
  again; here that would grant the same wake twice. The unusable part is reported
  with a warning and the wake stands.

The one thing the prompt says that is policy rather than instruction: do not call
it for something the operator has said to you. Being addressed is attended to by a
rule, and a wake beside it would have the system answer twice.

The resolver's side is one rule, on the kind `wake.granted`. What the judge
decided is in the event, so the model reads what it is being woken for rather than
a generic "something happened" — and because attention can reach the same event by
several roads at once, what stops a judgement and a rule from each opening their
own turn about it is §6k.

Verified with the stub, one message and nobody touching it again:

```text
operator   I have a camera at localhost:7831. Set it up, and tell me when you
           see a person.
velora     I have written a procedure for looking at the camera and set up a
           source that looks every few seconds.
…
wake.granted   decided this needs doing: Somebody is in front of the camera; tell
               the operator. — they asked to be told when the camera sees somebody
velora     Somebody is standing at the door — the camera I set up saw them.
```

The frames that held an empty hallway produced beliefs and no wake; the frame with
somebody in it produced a wake, and a turn.

## 6j. Plans that outlive the turn that made them

A turn is how the system thinks about one thing, and when it ends the thinking is
over. `velora-plan` is what survives that: a goal, an ordered list of steps, and
the fact that the system will still be in the middle of it tomorrow.

A plan is not a script. Nothing runs the steps. The plan is *announced* — one step
at a time — and the same reasoning engine that handles everything else works out
how to do the step it was handed. That is why revising a plan is normal rather
than exceptional, and why giving up on one is a recorded decision rather than a
failure.

### The step in hand is derived

A plan has an id, a goal, and steps. The current step is *the first that is not
done* — there is no stored cursor that can disagree with the steps after a
revision or a replay. Three events move a plan along: `plan.created`,
`plan.step.done` and `plan.revised`. `plan.step.done` marks the step in hand and
nothing else: a record saying step 5 is done while the plan is on step 1 is a
record of something that did not happen, and applying it would lose a step. The
fold refuses it, and the plan stays where it was.

Revising replaces everything *from* the current step onward and keeps what is
already done, notes and all. A plan that changed its mind about the future did not
un-happen its past.

### The stage hands out the work

`PlanStage` emits `plan.step.ready` whenever a plan comes to rest on a step —
because it was made, because the step before it finished, because it was revised,
or because the process just started over a history that says so. The resolver
watches that kind, so the step becomes a turn with nobody asking.

That last case is the whole point. On start the stage folds the plans out of
history and announces whatever has a step waiting, which is what makes a plan
survive a restart rather than being a note about something the system once
intended.

### What it will not do

It will not chase a step. A step that is worked on and never marked done stays in
hand until something changes the plan or the process restarts — one announcement,
not one per turn. A system that re-announced it would spend every turn it had on
the same unfinished step, which is how a plan becomes a loop. A plan that is never
finished is visible in `plan.list` as a plan that is not finished, which is the
honest outcome.

### What was actually reached

One message, and then nothing but the plan. Real binary, real SQLite, real web
server, stub model (`scripts/stub-openai.py --camera`):

```text
operator   I have a camera at localhost:7851. Set it up so you can look at it,
           and tell me when you see a person.

plan.created     made a plan, camera: hear from the camera on the other computer (3 steps)
plan.step.ready  step 1 of 3 in camera is ready: Work out how to look at the camera…
skill.defined    defined skill.camera.snapshot — Look at the camera on the other computer…
plan.step.done   step 1 in camera is done: wrote skill.camera.snapshot
plan.step.ready  step 2 of 3 in camera is ready: Set up a source that runs that procedure…
observer.defined set up camera.watch to run skill.camera.snapshot every 3s
plan.step.done   step 2 in camera is done: camera.watch is running
plan.step.ready  step 3 of 3 in camera is ready: Tell the operator it is done.
step.completed   told the operator: The camera on the other computer is set up…
plan.step.done   step 3 in camera is done: told the operator

observation.self   9 × from camera.watch, over the next half minute
wake.granted       decided this needs doing: Somebody is in front of the camera…
step.completed     told the operator: Somebody is standing at the door — the camera I set up saw them.
```

Three steps, three turns, nobody asked again in between. The last two lines are
the wake decision from §6i arriving on an observation the plan produced.

## 6k. One happening, one thought

The operator asked for their files to be watched. One message produced **three**
turns: the rule that watches what they say, the judge deciding the message called
for action, and a belief drawn from it under the `operator.` prefix. Each ran to
the step limit and failed, and the operator was told three times that the system
could not answer — twenty-four model calls of work, none of it finishing.

The rule that was missing is that **a turn is about an event, not about the
paperwork an event generates.** A belief is the system's reading of an event; a
wake is the judge saying an event calls for action. Both are *about* the event
behind them, so all three of those triggers name the same happening, and a
happening that is already being thought about is not a reason to think again.

The resolver now keeps the events its turns are about, and drops a trigger whose
event is already in hand. Two things make that work:

- **Identity and subject are separate.** What the model is *shown* is still the
  record that prompted the thought — the belief that changed, the event that
  arrived, the judgement that called for action — and the turn still inherits that
  record's time and sensitivity, so nothing is lowered. What the turn is *about*
  is the event behind it, and that is used for identity only.
- **A turn's id is not its cause.** `ReasonRequest.turn` is the sequence the loop
  assigned to the request, which this stage cannot know when it emits it. So the
  cause is read back from history when the turn ends — a point lookup, not a
  replay — and the open set is rebuilt from history on start by the same
  derivation, which is what lets a restart know what it was in the middle of.

Two events are still two thoughts, however alike: a change that arrives while the
system is busy and is about a *different* happening is remembered and taken up
when there is room. Two claims read from the *same* event are one thought — the
claim's own watch task is not asked, and the belief is in the context either way.
That is the trade, and it is the right way round: the alternative is what
happened above.

### The step budget

`reason.max_steps` was 8, chosen when a turn was "look something up and answer
it". A turn is now also expected to write a script, define it as a procedure, set
up a source that runs it, and check that it works — and being too small does not
produce a shorter answer, it produces **no** answer: the turn is discarded,
whatever it did is left half-done, and the next event starts it again from
scratch. The default is 20, it is a setting the system can change for itself, and
the ceiling is 64.

The model is now told its budget, and told what to do when a job is bigger than
it: write the steps down as a plan and work through it one turn at a time (§6j).
A system that does not know it is about to be cut off cannot decide to write the
job down instead of running out of room doing it. The rule is in the standing
instructions, so it applies to every turn; the number is appended per turn.

## 6l. Running out of steps is not failing

The operator asked for their files to be watched. The model did the hard part —
it found the procedure it had written before, called it, found it broken, worked
out why, and wrote a POSIX-safe replacement — and then ran out of steps. The turn
was **discarded**. Everything it had done was real and stayed in history, but the
turn failed, the operator was told it could not answer, and the next event
started the whole job again from the beginning.

A turn that has used its budget has not failed. It has run out of room, which is
a different thing and has a different answer: it gets **one last reply** to write
down what is left. That reply is told plainly what it is for, and what it writes
is a plan — so the plan stage announces the first step, a new turn picks it up,
and the work carries on instead of restarting.

```text
20 steps of work                    →  out of room
one more call, told "this is your   →  plan.create(2 steps) + operator.say
last reply: write down what is left"
                                      turn finished, not failed
plan.step.ready                     →  a new turn for step 1
plan.step.done                      →  a new turn for step 2
```

Run against the stub's `--burn` mode, which spends its whole budget and then
wraps up: **three turns, three finished, none failed**, and the sentence the
operator gets is "I ran out of room part-way through, so I have written down what
is left and will carry on" rather than an apology.

Two bounds keep it from being a way to ignore the budget. The final reply may
ask for at most four steps — enough for a plan and something said, not enough for
a turn's work — and a turn whose *first* reply crosses the budget is asked to
wrap up exactly once. A thought that will not stop still stops itself; it stops
having written down where it got to.

### The shell is now stated, not discovered

The replacement script exists because the first one used process substitution
(`< <(...)`), which the `sh` that runs it rejects:

```text
sh: -c: line 0: syntax error near unexpected token `<'
```

`/bin/sh` on this machine is bash 3.2 in POSIX mode. Nothing anywhere said so, so
the model wrote a bash script, and then spent five steps working out why its own
procedure did not run. `local.shell.run` and `skill.define` now say what the
interpreter is: `sh -c`, POSIX, process substitution and here-strings are syntax
errors, write POSIX shell. Whether to run scripts under `bash -c` instead is a
real question and not one to answer by accident — it would make the natural script
work and make the system's behaviour depend on which shells a machine has.

## 6m. A thought that was never had still has to be given back

The turn died with the 400 that names this field:

```text
The `reasoning_content` in the thinking mode must be passed back to the API.
```

DeepSeek's rule, from its own documentation: *"for requests carrying the `tools`
parameter, the `reasoning_content` must be fully passed back to the API in all
subsequent requests — even for turns where the model did not perform a tool
call."* Read narrowly, that is "give back what you were given". Read as the API
enforces it, it is "**every** assistant message carries the field" — and a
response that thought nothing still has to carry something.

That is the case that broke this. The turn's history holds the whole story:

```text
#608 reason.thought   reasoning=2341 chars     ← thought about it
#609 step.requested   local.fs.write
#610 step.completed
#611 step.requested   local.shell.run          ← no reason.thought between
#612 step.requested   local.shell.run             these and #610: this response
#613 step.completed                               thought nothing at all
#614 step.completed
#615 reason.failed    …must be passed back…
```

The response that asked for `#611` and `#612` had no content and no reasoning, so
nothing was recorded for it and the field was left out of the next request
entirely. `skip_serializing_if = "Option::is_none"` on a field the API requires is
a 400, and the whole turn is lost — including everything it had done.

The fix is the one DeepSeek's own integration uses: on a request that carries
tools, **every assistant message is given a `reasoning_content`, empty when there
was nothing to give**. It is scoped to DeepSeek by name — the model or the base
URL — because the field is not part of the OpenAI API and a provider that has
never heard of it should not be sent it.

## 6m1. How this one was found

Worth writing down, because the next one will look like it.

The report was one line of error text, and the failing system was on this
machine. So: read the live history rather than asking for logs. The
`reason.failed` record holds the provider's own words; the records *before* it are
the conversation that produced the request; and the question that finds the bug is
"which response in this turn has no `reason.thought` before its steps?"

That question is now a test. `crates/velora-reason/tests/real_turn.rs` replays
every turn in a local history and checks the two things a provider will refuse:

- every assistant message that asks for steps has an answer for every one of them
- every chain of thought the log recorded comes back out of the rebuild

Run against the history that failed: **28 turns, 0 unanswered calls, 0 lost
chains of thought, 15 messages that thought nothing** — which is the shape of the
bug in one line. It is `#[ignore]`d, because it reads a database that only exists
on the machine where it happened.

## 6n. Working on two things at once

The system did one thing at a time, and said so twice: `ResolverStage` opened at
most one turn, and `ExecutorStage` ran at most one procedure. Both defaults were
about a sequence — a plan's steps, two commands over the same files — and both
were wrong about what a person means when they say "do this for me" and then
carry on talking.

What changed is the *limit*, not the model. Attention was already per happening:
the same event cannot open two turns (§6k), so two open turns are two different
things and never one thing twice. A thought that spends its time waiting for a
command to finish is not using the attention it is holding, and with more than one
open, a long command stops being a wall between the operator and an answer.

- `VELORA_PARALLEL` sets how many things at once. One is the old behaviour and the
  default for the crate; the runtime uses three.
- The resolver's limit and the executor's are read from the same number, because
  more turns open than procedures that may run is a queue, and the other way round
  is a limit that never binds.

### Being aware of it

The other half, and the half that makes it usable rather than merely fast: a
thought has to know it is not the only one. "Install this" means something
different when something else is already half-installed, and a question is a
different question when the system is in the middle of something the person asking
may be asking about.

The situation now carries it:

```text
## What you are already in the middle of
- The operator has said something, and is waiting for an answer. (last step: local.shell.run {"command": "sleep 8; echo installed"})
```

Derived, not remembered: a turn is open exactly while its request has no finish or
failure after it — the same question the reasoning engine asks when it decides what
to resume — and the last step each one took is read out of the same history. The
heading is pushed before the beliefs, because it changes what the task means.

### Delegating is a long step, and the limit was invisible

The other half of "work on different things" is handing one of them to something
else — an installer, a build, another agent like `opencode` with its own context
and its own steps. The system can already do that: it defines a procedure, or
calls `local.shell.run`, and the program does the work.

What stopped it working was a limit nobody had told it about. Every command was
stopped after **60 seconds**, and nothing in any tool description said so. A
delegated task takes minutes, so the step would come back "it did not finish
within 60s and was stopped" — and the model would have no way to know why, having
never been told there was a clock.

`shell.run` now takes `timeout_secs`, the schema says what the default is and what
the ceiling is, and the ceiling is the operator's: an hour. A procedure's own
script still runs under the default and cannot ask for more, which is now said in
`skill.define` — so a long job is a `local.shell.run` with a longer clock, or a
plan whose steps are.

### What was actually reached

Two messages, 1.5 seconds apart, against the stub (`--parallel`), with the install
running for eight seconds:

```text
#1  operator   please install ffmpeg for me
#6  operator   are you there?                              ← while #1 is still running
#11 velora     I am here.                                  ← answered in the middle of it
#16 velora     ffmpeg is installed.
```

Two turns, both finished, neither failed — and the answer to the second arrived
while the first was still asleep. The second turn's context, read off the wire,
carried the section above naming the install and the exact command it was waiting
on.

## 6o. Thinking when nothing has happened

Everything else in Velora happens *because* something happened. That is the right
shape for a system that acts on events, and it leaves one thing out: a person does
not only think when spoken to. They notice the house has gone quiet, that they
still have not finished something, that they have believed a thing for a while and
never looked at it again.

`velora-pulse` is that, and it is deliberately an ordinary source. It sits on the
ingress channel beside the screen and the observers, it produces events, and
nothing downstream knows or cares that its reason for producing them was a clock.
The invariant holds: a turn is still its events, and a thought the system has on
its own traces back to a record that says why.

Two events, on a quiet moment:

- **`idle.noted`**, which the resolver turns into a thought of the system's own —
  *nothing has happened for ten minutes; look at what you hold and at what you are
  in the middle of; if nothing needs doing, do nothing.* The watch is
  `only_when_idle`, so a pulse that arrives while something is running is dropped
  rather than piled on top: a long install leaves the log quiet for minutes at a
  time, and "nothing has happened" while something is plainly happening would be
  the system talking itself into a turn about nothing.
- **`belief.reconsider`**, naming one belief, which the judge already knew how to
  ask about. This is the memory half, and it needed no new machinery at all — the
  question and its answer were built and tested long before anything asked them.

`VELORA_PULSE=off` runs a system that only thinks when something happens;
`VELORA_PULSE=<minutes>` sets the gap. On by default, because a system that never
thinks unless spoken to is the thing this exists to change — and off by a word,
because every quiet thought is a model call.

### What was actually reached

One message, then nothing at all for two and a half minutes, with the pulse turned
down to a minute so it could be watched:

```text
#3   belief.assert      operator.stated = my favourite colour is red (80%)
#7   reason.finished    concluded after 1 step
#8   idle.noted         nothing has happened for 20 seconds — thinking on its own
#9   reason.requested   asked to think about: Nothing has happened for a while, and this thought is your own…
#10  belief.reconsider  asked itself whether operator.stated still holds
#11  belief.assert      operator.stated = my favourite colour is red (75%) — looked at it again
#12  reason.finished    concluded after 0 steps: Nothing needs doing.
```

Nobody asked for any of that. The turn opened on its own, looked at what it held
and what it was in the middle of, decided nothing needed doing, and **said nothing
to the operator** — which is the property that makes this usable: `report_silence`
only reports to a person for a turn *they* started, so a self-started thought that
concludes nothing is indistinguishable from no thought at all, on the screen.

The belief came back at 80%: the re-assertion at 75% merged its evidence and did
not lower what was held.

### Two things the pulse found

- **A reconsideration that concluded nothing left no record**, so the event stayed
  undecided and the catch-up sweep would have asked about it again on every start,
  forever, at a model call each. Nothing had ever emitted a reconsideration, so it
  had never come up. Fixed, and pinned by a test.
- **A belief the system re-examined itself woke another thought.** The answer
  re-asserts the belief, that is a change under a watched prefix like any other,
  and the system was then asked whether anything needed doing about the fact that
  it had just thought about something — a loop, on a timer, at a model call each
  time round. The resolver now ignores a change whose evidence is the
  reconsideration itself.

## 6p. The system that stopped answering

It went silent. Messages arrived, were judged into beliefs, and nothing was ever
thought about them again — no error, no warning a person would see, and the screen
still live.

The history says it exactly. The last three turns ever opened were `#1131`, `#1132`
and `#1133`, and their focuses were wakes `#709`, `#868` and `#906` — judgements
from the *beginning* of that session that nothing had ever acted on. The start-up
sweep had picked them up. After those three turns ended, two operator messages
arrived and neither opened anything.

The cause is one line, and it is the shape of bug that a second caller creates:

```rust
// in the start-up sweep
self.act_on(ctx, &kind, sequence, sequence);          // the event's own sequence
// everywhere else
self.act_on(ctx, other, sequence, cause_of_event(..)); // the event *behind* a wake
```

A turn is identified by the event it is about. Three roads reach the resolver — a
watched event, a change in what is believed, and the start-up sweep — and all three
have to name the *same* event for the same happening. The sweep did not: it opened
a thought about `#709` while every other path derived `#703`, the event the
judgement was about. So when that turn ended, the resolver gave back `#703` and
kept `#709` held. Three of those filled `max_open_turns` and the system could not
open another turn for the rest of its life. Not for an hour: ever.

Two things fixed it, and the second is the one that matters:

- **One derivation, used by every road.** `cause_of_parts` decides what a trigger
  is about, and the live path, the sweep and the close path all go through it.
  There is no second place that could disagree.
- **The state and the log cannot disagree.** A cause is held only if the request
  was actually emitted: `ctx.is_replaying()` now returns before anything is
  inserted. An event the loop drops would otherwise leave this stage believing a
  turn is open that no ending will ever come back for.

The regression test is two processes, because the sweep only sees history at
start: the first writes a judgement nothing acts on, the second takes it up and
then somebody speaks. With the bug it is **two** turns and the person who spoke is
never answered; with the fix it is **three**. It was checked both ways, because a
test that passes is worth nothing until it is known to fail without the fix.

## 7. Known caveats and open items

Honest and current. None of these are surprises hiding in the code.

1. **Start-up sweeps are O(history).** Both the catch-up pass and the belief
    rebuild walk every record. There is no index, and no snapshotting; this is
    the first thing that will hurt on a long-lived log.
2. **Waiting is in memory and bounded.** Events held for a model live in a
    bounded queue (`max_pending`, default 1024). Past that they stay in history
    undecided and are picked up by the next start's catch-up pass. Nothing is
    lost, but a long outage with heavy traffic means some events wait for a
    restart rather than for the model.
3. **The prompt cap is a keyword heuristic.** Beliefs whose key appears in the
    event text are ranked first, then by recency. A belief about the same subject
    under a different name will not be ranked up, and the scan is linear in the
    number of beliefs.
4. **The derivation-depth cap does not cross the feedback channel.** A stage that
    feeds work back must avoid unbounded loops itself. The judge does, by never
    judging belief events.
5. **Whole-history reads.** `replay` materialises everything, and `Store::read`
    returns a `Vec`. Streaming replay is the natural evolution and `Store` is the
    seam for it. The database answers the cheaper questions — count, contiguity,
    a point, a range — from its index, but a full replay is still a full replay.
6. **Single-writer is a deployment contract.** In-process it is enforced by
    `&mut self`; there is no file lock. Note that *opening* a store is a mutating
    operation — it truncates a torn tail — so a second opener can disturb a
    writer. The database turns concurrent writers into SQLite errors rather than
    corruption, but two Velora processes on one database is still unsupported.
7. **The database is one connection behind a mutex.** Reads therefore serialize
    with writes. Nothing contends today, because the loop appends and its stages
    ask on the same thread; a connection pool is the upgrade if that changes.
8. **The screen polls history every 150ms.** The loop does not notify anyone
    when it appends, and threading a screen's needs through the loop would put a
    display into the design of the thing being displayed. For an indexed store
    that is a range query; for a file store it is a scan, so a file backend with
    the screen open does real work for it.
9. **A screen that falls behind the stream loses the gap.** The broadcast buffer
    holds 64 updates; a screen slower than that is told it fell behind and can
    re-read the snapshot, but nothing fills the gap for it.
10. **The timeline is the last 500 records.** The conversation is sent whole,
    because that is the part a person reads; the timeline says it was truncated.
11. **The screen has been looked at, by one pair of eyes.** The layout renders,
    nothing overflows its column, and the three panels are readable at
    1680x929. It has not been tried narrow, on a phone, in Safari, or by anyone
    else. `scripts/screen-in-browser.py` is how to see it.
12. **`process.list` needs `ps`.** In a sandbox that denies it the procedure
    reports the failure rather than returning an empty list, which is the
    behaviour that matters — but a machine where `ps` cannot run is a machine
    where that procedure does nothing.
13. **A file history is adopted, never merged.** The import runs only into an
    empty store and only from one path, so two histories are never interleaved.
14. **Projection state grows in memory.** Challenges and withdrawal attempts
    accumulate per key; nothing compacts them.
15. **No CI, no license file.**
16. **`velora-core` is an empty placeholder** and nothing depends on it.
17. **An unreachable model keeps the process alive**, deliberately: the loop will
    not stop while a judgement is waiting, because stopping would discard it. A
    supervisor watching for a hung process should know that this is intended.
18. **The setup prompt shows the API key as it is typed.** Suppressing terminal
    echo needs a terminal-handling dependency this crate does not carry, and the
    alternative — hiding the input badly — would be worse.
19. **A thought is serial and can hold everything up.** One turn at a time, one
    procedure at a time. A model that never answers leaves its turn open, which
    occupies the system's attention; nothing new is thought about until it
    finishes or the process restarts.
20. **Assembling a situation replays history.** `velora-context` needs the events
    behind each belief, and `History::replay` copies the log to find them. This is
    the next thing that will hurt on a long-lived log, alongside the start-up
    sweeps.
21. **A turn that speaks and then fails has already been heard.** Speaking is a
    step, so it happens when it happens; if the thought later fails, the operator
    has the words and no indication that anything went wrong afterwards. The
    failure is in the log and on the timeline, and not in the chat.
22. **A failing provider is only visible in the log.** A wrong key produces a
    401 on every judgement, each held and retried on the next start. The events
    are not lost — nothing is — but the screen shows the model that is
    configured without showing that it is refusing, so the operator has to read
    the log to find out why nothing is being decided.
23. **A panic in a worker is caught, not diagnosed.** The judge and the reasoner
    each run one long-lived worker, so a panic inside a single judgement or
    thought used to unwind the worker itself: nothing was decided again, every
    turn stayed open, and the loop never went idle, so it never stopped. A panic
    now fails that one judgement or thought, is recorded with what it said, and
    leaves the worker alive. The underlying bug is still a bug — this turns a
    hang into a reported failure, which is the difference between something to
    read and something to guess at.
24. **The judge records; it does not decide what matters.** A chatty operator
    therefore produces a lot of small beliefs. That is the intended direction of
    the trade — a missed event is gone, whereas a belief is evidence-backed,
    revisable and can be withdrawn — but nobody has looked at what a long
    conversation does to the size of a store.
25. **The judge is shown one event and no dialogue.** It can read what was said
    now that it is given words rather than bytes, but it cannot resolve "it" or
    "that one", because the message is all it has. Beliefs that need more than
    one message need the engine that has the transcript.
26. **Attention is a rule per *belief*, and a decision per *event*.** Whether a
    belief change is worth thinking about is still a `Watch` — a rule, so that what
    the system pays attention to is readable rather than chosen — and being
    addressed is still attended to by a rule, because asking is not a decision.
    What is new (§6i) is that the judge decides, per event, whether the event calls
    for action, and that decision is a `wake.granted` the resolver acts on. The two
    can disagree, and when they do the *event* is what settles it: a turn is about
    a happening, so a second reason to think about a happening a turn is already
    about does not open a second turn (§6k).
27. **The catch-up sweep is oldest-first and unbounded.** It asks about
    everything history has not decided, in order, so a long backlog delays the
    newest thing. It no longer grows on its own — a decision of nothing is
    recorded — but a genuine backlog (a long outage, or a model that was down)
    is still worked through from the beginning.
28. **The model has no way to look at what it believes.** It can read history and
    the filesystem but cannot ask "what do I hold about the operator", so a model
    that wants to check whether something it was told is on record can only go
    looking through the repository — which is what one did, for eight steps, until
    it hit the limit. A procedure that answers that question is missing.
29. **A resolver decision pending across a restart is lost.** Changes that
    arrived while the system was busy are held in memory; the turns themselves
    resume, but a change still waiting for room does not.
30. **A plan step that is never finished waits for a restart.** Nothing
    re-announces it: the stage announces a step when a plan comes to rest on it, and
    working on a step without marking it done does not move the plan. That is
    deliberate — the alternative spends every turn the system has on the same step —
    but it means a step the model tries and does not conclude is not retried until
    the process restarts or the operator asks. There is also no notion of a step
    being *due* at a time, and nothing schedules a plan for later.
31. **A truncated turn's continuation is unbounded.** A turn that runs out of room
    writes a plan, the plan hands out a step, that step's turn can run out of room
    and write another plan, and nothing counts how many times that has happened.
    Each turn is bounded; the sequence is not. It is visible — every plan, step and
    turn is in the log and on the timeline — and it stops when the work stops being
    worth doing, which is a judgement the model makes and the operator can see. But
    there is no ceiling on it, for the same reason as the caveat below.
32. **Every report from a source costs a judge call, and nothing budgets for it.**
    An observer reporting every second is a model call every second, forever: the
    interval floor (1s) and the observer ceiling (8, in the binary) bound how many
    sources there can be but not what they cost together, and nothing measures or
    limits the spend. A camera at 3s is a call every 3s whether or not anything
    about it changed. The judge holds up to `max_in_flight` (4) at once and past
    that events wait, which means a chatty set of sources eventually delays the
    newest thing rather than losing it — the same shape as the catch-up backlog,
    with the same cause: there is no notion of a budget anywhere in this system.
33. **Which provider gets a `reasoning_content` is a guess about a name.** The
    field is added when the model or the base URL contains "deepseek". A gateway
    that renames both would miss it and the 400 would come back; a provider that
    happens to be called that and is not would be sent a field it does not know.
    A setting would answer this properly, and there is not one.
34. **More things at once is more of everything.** Each open turn is its own model
    calls and its own procedures, and nothing measures or caps the total — the same
    absence as caveat 32, now multiplied by `VELORA_PARALLEL`. Three open turns is
    three conversations' worth of context being sent, per step.
35. **One model call at a time, however many turns are open.** There is one worker;
    turns interleave at *step* boundaries, not at model calls. That is the right
    shape for the case this exists for — the long thing is a command, not a thought
    — but a slow model makes every open turn wait for the one before it.
36. **Two steps a model asked for in one message may now run at the same time.**
    The executor's limit used to serialize them; it does not any more. Steps in
    separate messages are unaffected, because the model is not asked again until
    every outstanding step has come back, and that is how a plan works through
    anything. But a model that asks to write a file and run it in one breath has
    asked for a race, and will now get one.
37. **The pulse spends money while nothing happens.** Every quiet moment is a turn
    plus, when something is held, a judge call — and a turn can take up to twenty
    steps if the model decides to act on what it noticed. A system left alone for a
    day is not free, and nothing measures it. The interval is the only brake, and
    the default is thirty minutes.
38. **The resolver holds a turn only while the request it emitted is in the log.**
    That is now checked rather than assumed, but it is still in-memory state
    reconstructed from history at start: a stage that emits and is then dropped by
    the loop is the failure mode, and there is exactly one `ctx.emit` in the
    resolver. Anything that adds a second one has to keep the two in step.
39. **Quiet is judged by the log being unchanged, not by the system being free.**
    A long step leaves the log quiet while the system is busy, which the resolver
    catches by refusing to open a pulse turn while another is open. But a command
    that writes nothing for ten minutes and is not part of a turn — there is no
    such thing today — would read as idle.
40. **An empty answer is still read as "nothing to record".** A response with no
    words and no tool calls is the one remaining place where the two meanings
    meet, and it is a deliberate choice: a model that had something to say would
    have said it, and treating an empty reply as unreadable would re-ask about
    that event forever. Everything else that is said and could not be used — a
    malformed call, an unknown tool, prose — leaves the event undecided and is
    asked about again.

## 8. Working agreements

- Keep crates small and single-purpose; prefer a new crate over packing
  responsibilities into `velora-core`.
- Match existing style. Errors are hand-written rather than macro-derived;
  history encodings are hand-rolled and canonical, while wire formats use serde.
- Prefer refusing over guessing, and reporting over silently degrading.
- Verify with build, clippy and tests, and report exact commands and output.
- Committing is fine when asked; do not rewrite history.
