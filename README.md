# gray-slack

A standalone Slack channel adapter for [gray](https://github.com/vstaln/gray)
— Socket Mode events in, mrkdwn replies out, with its own setup wizard,
pairing allowlist, durable SQLite queue, and a background service. Same
shape as `gray-discord-plugin`, Slack transport underneath.

**Status: v1.** Inbound `message` events in admitted channels/DMs run one
isolated `gray` turn per conversation; outbound is `chat.postMessage` plus
the `files.upload` v2 flow. Block Kit and interactive components are out of
scope for v1. See [runtime policy and limits](docs/RUNTIME.md).

## Requirements

- `gray` on PATH, provider/model configured in `~/.gray/config.json`
- A Slack app with Socket Mode on and:
  - App-Level Token (`xapp-…`), scope `connections:write`
  - Bot Token (`xoxb-…`), scopes: `chat:write`, `files:write`,
    `channels:history`, `groups:history`, `im:history`, `mpim:history`,
    `app_mentions:read`
  - Event Subscriptions on; bot events: `message.channels`,
    `message.groups`, `message.im`, `message.mpim`
  - The bot installed to the workspace and invited where it should listen

No public URL or ingress is needed — events arrive over an outbound
WebSocket.

## Install

```sh
cargo install --path .          # puts gray-slack on PATH
gray plugin install slack       # or let gray find the binary
```

## Setup

```sh
gray slack setup                # or: gray gateway setup slack
```

The wizard prints the app-configuration checklist, then:

- asks for the bot token (`xoxb-…`) and verifies it with `auth.test`
  (`SLACK_BOT_TOKEN` in the environment is used instead of a paste)
- asks for the app token (`xapp-…`) and verifies it with
  `apps.connections.open` (`SLACK_APP_TOKEN` likewise)
- asks your member ID (profile → ⋯ → Copy member ID) as the owner, plus
  extra allowed member IDs
- opens a DM with you (`conversations.open`) as the home channel
- asks for extra channel IDs the bot may answer in
- saves the config privately (`~/.config/gray-slack/config.json`, mode
  0600), registers the `slack_send`/`slack_file` tools with gray, runs the
  doctor, and offers to install the background service

## Running

```sh
gray slack status      # service health
gray slack stop        # graceful stop (posts shutdown notices)
gray slack restart
gray slack uninstall   # service only; config and sessions stay
gray slack run         # foreground daemon (what the service execs)
gray slack doctor      # verify tokens + local gray config
gray slack queue list / cancel <id>
gray slack allowlist add|remove|list
gray slack pairing approve slack <CODE>
gray slack limits --timeout-seconds 600 --concurrency 2 --max-requests 200
```

The service probes what this box actually supervises with (runit,
`systemd --user`, or a detached pidfile daemon when no init is available).

## Talking to it

- DM the bot, or post in an allowed channel / the home channel.
- Replies land in the message's thread (`reply_in_thread`, default on).
- `/new` or `/reset` in a chat starts a fresh session.
- `/steer TEXT` (or `!steer TEXT`) adds guidance to the running turn; when
  nothing runs it is sent as a new message.
- A message sent while a turn runs queues as its own next turn ("Queued — N
  ahead.", at most once per 30s per conversation); `"busy_input_mode":
  "steer"` in the config steers the running turn instead.
- Long command output posts page 1 with a `page 1/N — /page 2` footer;
  `/page N` (or `!page N`) shows another page.
- A human who DMs an unconfigured allowlist gets a one-shot pairing code;
  the owner approves it with `gray slack pairing approve slack <CODE>`.

## Tools (registered with gray)

- `slack_send {content, channel?, thread_ts?}` — markdown → mrkdwn,
  4k-chunked; a `MEDIA:/path` line uploads that file after the text.
- `slack_file {path|paths, comment?, channel?, thread_ts?}` — uploads via
  `files.getUploadURLExternal` → upload → `files.completeUploadExternal`.

## Limits (v1)

- Text and files only: no Block Kit documents, buttons, modals, reactions,
  typing indicators, or streamed replies (the answer posts when the turn
  ends).
- Inbound file attachments appear as `[attached file: name]` markers;
  contents are not fetched.
- Slash commands are not registered; `/new`, `/reset`, `/steer` and `/page`
  are plain text (Slack's client may refuse an unknown `/name`; `!name` always
  works).
- Channel joins/leaves and edits/deletes are ignored.
- Delivery is at-least-once; a crash between a Slack accept and the local
  record can duplicate a reply chunk (see docs/RUNTIME.md).
