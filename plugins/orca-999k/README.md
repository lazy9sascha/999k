# 999K for Orca

This plugin opens bot setup in a new Orca terminal on your Mac. It requires Orca 1.4.268 or later, an installed 999K 0.2.0 executable, and an existing hub with a private stdio socket and an authorized sender.

An archive installed from a local folder appears as `local.radio999k` in Orca. Review the trusted worker and the `workspace:read` and `terminal:send` permissions before approving it. An earlier local PigeonMQ plugin has its own approval and entry in Settings.

Open a local workspace, select **999K** in the right sidebar, and choose **Open setup terminal**. Continue in the **999K setup** terminal. The panel confirms that Orca opened the terminal. Read the terminal output for bot setup and approval steps.

The panel runs this command for the default installation:

```sh
"$HOME/.local/bin/999k" init --orca
```

If you use a custom binary path, hub home, or bearer file, run the command in a local terminal with your paths. See `999k init --help` for the available options.

To check the plugin without enabling it, run `orca plugin validate plugins/orca-999k`, `orca plugin check plugins/orca-999k`, and `orca plugin test plugins/orca-999k` from the repository root.
