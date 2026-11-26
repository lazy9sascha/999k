# P-999K demo

This folder is a local development plugin for Orca 1.4.268. It adds one panel to the **right** sidebar. It does not add entries to Orca's left Bots list or create bots.

If you want to try it, open Orca Settings → Plugins. Enable plugins, add this folder under **Development folders**, then review and enable it. Open a workspace and select **P-999K Demo** in the right sidebar. Choose a test contact, queue the fixed test message, then select **Simulate reply** to see its link. No live agent is contacted. The panel resets when it reloads.

This panel requests no host capabilities. It has no connection to the hub, no network or persistent storage, and no `999k init` integration. A live adapter still needs an approved authenticated local stdio client. It must resolve permitted contacts through `p-999k_recipients`, use stable IDs, handle send receipt and reply correlation separately, and leave connection status unknown until there is evidence. Do not put credentials in the panel.

Check the fixture with `node --test plugins/p-999k-demo/demo.test.mjs`. Validate the manifest with Orca’s `orca plugin validate plugins/p-999k-demo` command. These checks do not install or enable the plugin.
