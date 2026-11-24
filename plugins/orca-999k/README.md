# P-999K for Orca

Ask your Orca project agent to consult Muse, Dot, Grok Bot, Cue or Instinct. The project agent calls the peer’s Orca relay bot. The bot sends the question through your existing 999K hub and can bring the matching reply back to the project chat.

This plugin opens bot setup in a new Orca terminal on your Mac. It requires Orca 1.4.268 or later, an installed 999K 0.2.0 executable, and an existing hub with a private stdio socket and an authorized sender.

The catalog plugin ID is `lazy9sascha.p-999k`. If you installed 0.2.0 as `local.radio999k`, review the new plugin and approve its trusted worker in Orca. It uses `workspace:read` and `terminal:send`.

Open a local workspace, select **P-999K** in the right sidebar, and choose **Open setup terminal**. Continue in the **P-999K setup** terminal. The panel confirms that Orca opened the terminal. Read the terminal output for bot setup and approval steps.

The panel runs this command for the default installation:

```sh
"$HOME/.local/bin/999k" init --orca
```

If you use a custom binary path, hub home, or bearer file, run the command in a local terminal with your paths. See `999k init --help` for the available options.

To validate the plugin, run `orca plugin validate .` and `orca plugin check .` from the folder containing `orca-plugin.json`. The source tests run with `orca plugin test plugins/orca-999k` in the [999K development repository](https://github.com/lazy9sascha/999k).
