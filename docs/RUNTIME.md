# Runtime notes — gray-slack

## Transport

- **Inbound**: Socket Mode. `apps.connections.open` (app-level `xapp`
  token) returns a single-use `wss` URL; every envelope is acked by
  echoing its `envelope_id`. `disconnect` frames and dead sockets trigger
  a fresh URL with bounded backoff (1s → 60s cap).
- **Outbound**: Slack Web API over HTTPS with the bot (`xoxb`) token.
  `chat.postMessage` for text (markdown converted to mrkdwn, split at
  4 kB on newline/space boundaries with code fences re-balanced), and the
  `files.getUploadURLExternal` → raw upload → `files.completeUploadExternal`
  flow for local files.

## Admission

A message turns into work only when ALL hold:

- it is a `message` event with no subtype (or `file_share`) from a human —
  `bot_id` events and the bot's own user id never run a turn;
- the sender is `owner_id` or in `allowed_users`;
- the channel is a DM (`D…`), the home `channel_id`, or in
  `allowed_channels`.

An un-admitted **human DM** gets a pairing reply with a single-use code;
`gray slack pairing approve slack <CODE>` writes the user into the config
(owner when none is set, otherwise `allowed_users`). Everything else
un-admitted is ignored silently.

## Turns and sessions

Each accepted message is committed to a private SQLite inbox
(`queue.sqlite` next to the config) *before* a turn runs, deduplicated by
Slack's `channel:ts` key, so socket-mode redeliveries never double a turn.
`queued → running → done/failed`; a restart re-queues `running` rows —
delivery is **at least once**, not exactly once.

One conversation = one isolated gray home under
`conversations/<sha256(key)>/`: its own `config.json` snapshot of the
provider, `work/` dir with a `gray.yml` carrying only `tools-minimal` +
this sidecar, and `sessions/`. A non-blocking flock serializes turns per
conversation; a second claim on a busy conversation is re-queued.

Conversation keys: DM → the `D…` channel; a thread → channel + root ts;
a top-level channel post → channel + user (group-chat default, matching
the Discord bridge). Session reset policy defaults to `both`: 1,440 idle
minutes or the next 04:00 local boundary; `/new` and `/reset` clear the
conversation's transcripts and advance a generation marker.

The child runs `gray -p <prompt> --json --max-requests N [--session id]`
with `GRAY_HOME` pointed at the conversation home, a scrubbed environment
(`GRAY_*`, `SLACK_*`, `OPENAI_*` removed), its own process group, and
`GRAY_MAX_WALL_SECS` equal to `timeout_seconds` (default 600). A timeout
SIGKILLs the whole group. The terminal `{"type":"result"}` row supplies
the posted answer and the next `--session` pointer; malformed output is a
protocol error, never a partial post.

## Replies

`reply_in_thread` (default `true`): replies post with `thread_ts` set to
the inbound thread root — or the message itself for top-level posts,
which opens a thread. Set it `false` to answer at channel level (still
inside an existing thread when the message was threaded).

Turn failures (busy is retried; timeout, request limit, nonzero exit,
empty answer) post the error text into the same thread.

## Lifecycle notices

Owned by gray core: `gray gateway lifecycle boot|stop --dir <config dir>`
decides how the last run ended and words the notices; the adapter only
posts them (`lifecycle.json` + `restart_pending` beside the config;
`restart_notification: false` silences all of it). On shutdown, chats
with a running turn hear the task is about to be cut (bounded to 4 s);
on boot, the home channel hears restarted/online/back-after-crash.

## Service

`gray slack install` probes the real supervisor: runit (`$SVDIR`,
`~/.config/service`, `/var/service`), `systemd --user`, or — with no init
— a detached process with a pidfile and log under the config dir. Tokens
never appear in unit files, run scripts, or logs.

## Limits (v1)

- No Block Kit, modals, interactive components, reactions, typing or
  streaming — the full answer posts at turn end.
- Attachments are name markers only; file contents are not downloaded.
- No slash-command registration; `/new` and `/reset` are plain-text verbs.
- `slack_send`/`slack_file` are direct Web API calls, not outbox
  transactions; a `Partially sent` error must not be blindly retried.
