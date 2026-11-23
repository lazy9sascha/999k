const contacts = ["Dot", "Instinct", "Cue", "Muse", "Grok"].map(label => ({
  id: `demo-${label.toLowerCase()}`, label: `${label} (test)`
}))

// No network, host APIs, disk storage, or credentials.
export function createDemoAdapter() {
  const threads = new Map()
  let counter = 0
  function thread(id) {
    if (!contacts.some(peer => peer.id === id)) throw new Error("Unknown test contact")
    if (!threads.has(id)) threads.set(id, [])
    return threads.get(id)
  }
  return {
    async listPeers() { return contacts.map(peer => ({ ...peer })) },
    async readConversation(peerId) { return thread(peerId).map(message => ({ ...message })) },
    async enqueueTest(peerId) {
      const messages = thread(peerId)
      if (messages.some(m => m.kind === "request")) throw new Error("Test message already queued")
      const id = `test-${++counter}`
      messages.push({ id, kind: "request", body: "PigeonMQ demo: please reply to this test message." })
      return { id, status: "queued" }
    },
    async simulateReply(peerId) {
      const messages = thread(peerId)
      const request = messages.find(m => m.kind === "request")
      if (!request || messages.some(m => m.replyTo === request.id)) throw new Error("No pending test message")
      messages.push({ id: `test-${++counter}`, kind: "reply", replyTo: request.id, body: "This test reply links to your test message." })
    }
  }
}
