export const cueSetupPrompt = 'Cue, add a separate P-999K connection for this Mac in this chat. Keep my other connections. Show me a secure form for this Mac’s HTTPS hub address and one-time pairing code. If private input is unavailable, tell me and stop. Pair at /cue/pair on the address I enter. Save the credential privately for that address and confirm that /cue/mcp on that same address lists the mailbox tools. Keep the address and code out of chat.'

export const initialTarget = 'initial'
export const setupTargets = [
  { value: initialTarget, label: 'Initial setup' },
  { value: 'dot', label: 'Dot' },
  { value: 'instinct', label: 'Instinct' },
  { value: 'cue', label: 'Cue' },
  { value: 'muse', label: 'Muse' },
  { value: 'grok-bot', label: 'GrokBot' }
]

const setupCommand = '"$HOME/.local/bin/999k" init --orca'
const setupTitle = 'P-999K setup'

const peerIds = setupTargets.filter(({ value }) => value !== initialTarget).map(({ value }) => value)
export const allPeersSetupRequest = [
  'Set up P-999K on this Mac from this Orca workspace. Use Computer Use for the existing peer apps.',
  'Start with "$HOME/.local/bin/999k" init --reserve-address. Let me approve the email verification in the browser if prompted.',
  `Then run ${setupCommand} --orca-peer <id> for each of ${peerIds.join(', ')}. Keep the same hub home and reuse the saved address, existing conversations and matching relays.`,
  'For Dot, use the configured OAuth endpoint or its existing private Secure MCP Tunnel. For Cue, Muse, GrokBot and Instinct, follow the terminal handoff in each existing app. Open its private setup form before requesting a one-time code. Transfer the saved address and code directly into the form. Keep both out of chat.',
  'Ask me only for an approval or a step that needs me. If one app is blocked, continue with the others.',
  'Check that each peer returns a linked answer to this Orca project chat. Report which peers answered and what is still pending.'
].join(' ')

export const managementBotPurpose = [
  'You are the P-999K management bot.',
  `When asked to list peers, run ${setupCommand}.`,
  `When asked to connect one peer, run ${setupCommand} --orca-peer <id>. Use one of these IDs: ${peerIds.join(', ')}.`,
  'To reserve a hosted address before connecting peers, run "$HOME/.local/bin/999k" init --reserve-address. The owner approves registration in their browser.',
  'Report setup status, any owner approval needed, and any remote handoff printed by the command in this Home Chat.'
].join(' ')

export function setupFor(target: unknown, action: unknown = 'setup') {
  const option = setupTargets.find(({ value }) => value === target)
  if (!option) return undefined

  if (action === 'hosted' && option.value === initialTarget) {
    return { command: '"$HOME/.local/bin/999k" init --reserve-address', title: 'P-999K hosted address' }
  }
  if (action === 'hosted' && option.value === 'cue') {
    return {
      command: `${setupCommand} --orca-peer ${option.value} --orca-reserve-address`,
      title: `P-999K hosted setup · ${option.label}`
    }
  }
  if (action === 'pair' && ['muse', 'grok-bot', 'cue', 'instinct'].includes(option.value)) {
    return {
      command: `"$HOME/.local/bin/999k" pair offer ${option.value}`,
      title: `P-999K pairing · ${option.label}`
    }
  }
  if (action !== 'setup') return undefined
  if (option.value === initialTarget) {
    return { command: setupCommand, title: setupTitle }
  }

  return {
    command: `${setupCommand} --orca-peer ${option.value}`,
    title: `${setupTitle} · ${option.label}`
  }
}

declare module 'orca' {
  interface PluginRequests {
    'setup.open': { input: { target: string; action?: 'pair' | 'hosted' }; output: { opened: boolean } }
  }
}
