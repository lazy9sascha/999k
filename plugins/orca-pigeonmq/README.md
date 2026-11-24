# PigeonMQ for Orca

This plugin opens bot setup in a new Orca terminal on your Mac. It requires Orca 1.4.268 or later, an installed PigeonMQ 0.2.0 executable, and an existing hub with a private stdio socket and an authorized sender.

Review Orca's workspace and terminal permissions when you enable the plugin. Open a local workspace, select **PigeonMQ** in the right sidebar, and choose **Open setup terminal**. Continue in the **PigeonMQ setup** terminal. The panel reports when Orca opens the terminal. Read the terminal output for the setup result.

The panel runs this command for the default installation:

```sh
"$HOME/.local/bin/pigeonmq" init --orca
```

If you use a custom binary path, hub home, or bearer file, run the command in a local terminal with your paths. See `pigeonmq init --help` for the available options.

To check the plugin without enabling it, run `orca plugin validate plugins/orca-pigeonmq`, `orca plugin check plugins/orca-pigeonmq`, and `orca plugin test plugins/orca-pigeonmq` from the repository root.
