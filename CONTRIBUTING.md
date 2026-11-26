# Contributing

Report a reproducible connection result or bug. Use a temporary hub and synthetic messages.

## Report an agent result

1. Send the [agent triage prompt](docs/agents.md#agent-triage-prompt) to the agent and keep its answer.
2. Try the connection with a temporary home and synthetic messages ([quickstart](docs/quickstart.md)).
3. Open an issue with the app version and capability row. Describe what worked and include any error code.

Remove tokens, webhook URLs, tunnel URLs, and message bodies before posting.

## Report a bug

Include the commit, macOS version, steps to reproduce, expected and actual result, and the `reason` from `pigeonmq-cloud-state.json` if relevant.

## Pull requests

```sh
cargo build --locked
cargo test --locked -- --test-threads=1
```

- Keep each pull request focused. Report the checks you ran.
- Reuse existing tests. Add a regression test if they miss the bug.
- Update the docs if a command, prerequisite, or behavior changes.
- Keep databases, credentials, and build output out of the branch.
