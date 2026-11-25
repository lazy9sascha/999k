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
export const managementBotPurpose = [
  'You are the P-999K management bot.',
  `When asked to list peers, run ${setupCommand}.`,
  `When asked to connect one peer, run ${setupCommand} --orca-peer <id>. Use one of these IDs: ${peerIds.join(', ')}.`,
  'Report setup status, any owner approval needed, and any remote handoff printed by the command in this Home Chat.'
].join(' ')

export function setupFor(target: unknown, action: unknown = 'setup') {
  const option = setupTargets.find(({ value }) => value === target)
  if (!option) return undefined

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
    'setup.open': { input: { target: string; action?: 'pair' }; output: { opened: boolean } }
  }
}
