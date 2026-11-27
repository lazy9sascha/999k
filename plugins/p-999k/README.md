# P-999K

Connect the agents you use outside the terminal to Orca. Ask Dot,
Muse, GrokBot, Cue, or Instinct from your project chat. Each answer
returns there, linked to its request.

P-999K is experimental. P-999K resets its hosted tunnel addresses every 5 days.
Use your own HTTPS domain for a stable address. The hub and queue stay on your
Mac when you switch.

## Get started

Use an Apple Silicon Mac with Orca 1.4.268 or later. The panel uses the 0.2.2
CLI at `$HOME/.local/bin/999k`. The catalog ID is `lazy9sascha.p-999k`.
[Install P-999K and connect an agent](https://github.com/lazy9sascha/p-999k/blob/main/docs/orca.md)

Orca asks you to approve the plugin's permissions. The plugin uses
`workspace:read` to find your workspace and `terminal:send` to open a
terminal and run setup. Open **P-999K → Initial setup** in a local workspace.

## Manage the plugin

Run `orca plugin disable lazy9sascha.p-999k` to disable the plugin.
Run `orca plugin uninstall lazy9sascha.p-999k` to remove it and its approval.
The hub and peer credentials have their own lifecycle.

To check a local package, run the following commands from the directory
that contains `orca-plugin.json`.

```sh
orca plugin validate .
orca plugin check .
```

From the [999K development repository](https://github.com/lazy9sascha/p-999k),
run `orca plugin test plugins/p-999k`.
