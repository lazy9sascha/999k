export const setupCommand = '"$HOME/.local/bin/999k" init --orca'
export const setupTitle = '999K setup'

declare module 'orca' {
  interface PluginRequests {
    'setup.open': { output: { opened: boolean } }
  }
}
