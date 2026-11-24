export const setupCommand = '"$HOME/.local/bin/pigeonmq" init --orca'
export const setupTitle = 'PigeonMQ setup'

declare module 'orca' {
  interface PluginRequests {
    'setup.open': { output: { opened: boolean } }
  }
}
