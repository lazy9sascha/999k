# Contributing

Thanks for helping. The most useful thing you can do is **try an agent we haven't verified and tell us what happened**. Every agent connects differently, so real reports matter more than anything else.

## Report an agent result

1. Send the [agent triage prompt](docs/agents.md#agent-triage-prompt) to the agent and keep its answer.
2. Try the connection with a temporary home and synthetic messages ([quickstart](docs/quickstart.md)).
3. Open an issue with: app and version, the capability row, what worked, where it stopped, and the exact error.

Remove tokens, webhook URLs, tunnel URLs, and message bodies before posting.

## Report a bug

Include the commit, macOS version, steps to reproduce, expected and actual result, and the `reason` from `pigeonmq-cloud-state.json` if relevant.

## Pull requests

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
```

- One focused change per PR. Say what you tested and what you didn't.
- Reuse existing tests; add a regression test only when nothing covers the bug.
- Update the docs if a command, prerequisite, or behavior changes.
- Keep databases, credentials, and build output out of the branch.
