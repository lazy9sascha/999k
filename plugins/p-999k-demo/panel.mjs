import { createDemoAdapter } from "./demo.mjs"

const adapter = createDemoAdapter()
const get = id => document.getElementById(id)
const select = get("contact")
const list = get("messages")
const empty = get("empty")
const send = get("send")
const reply = get("reply")
const refresh = get("refresh")
const status = get("status")
let peers = []
let messages = []
let conversationReady = false
let busy = false

function render() {
  select.disabled = busy || peers.length === 0
  refresh.disabled = busy
  send.disabled = busy || !conversationReady || messages.some(m => m.kind === "request")
  reply.disabled = busy || !conversationReady || !messages.some(m => m.kind === "request") || messages.some(m => m.kind === "reply")
  empty.hidden = messages.length > 0
  empty.textContent = !select.value ? "Choose a contact to see its conversation." :
    conversationReady ? "No messages yet." : busy ? "Loading test conversation…" : "Test conversation not available."
  list.replaceChildren(...messages.map(message => {
    const row = document.createElement("li")
    const label = document.createElement("small")
    label.textContent = message.replyTo ? `Simulated reply to ${message.replyTo}` : `Queued in demo · ${message.id}`
    row.append(label, document.createTextNode(message.body))
    return row
  }))
}

async function run(action, errorText) {
  if (busy) return
  busy = true
  status.textContent = ""
  render()
  try {
    await action()
  } catch {
    status.textContent = errorText
  } finally {
    busy = false
    render()
  }
}

async function loadConversation() {
  messages = []
  conversationReady = false
  if (!select.value) return
  messages = await adapter.readConversation(select.value)
  conversationReady = true
}

async function loadPeers() {
  const previous = select.value
  peers = []
  messages = []
  conversationReady = false
  select.replaceChildren(new Option("Choose a contact", ""))
  peers = await adapter.listPeers()
  select.append(...peers.map(peer => new Option(peer.label, peer.id)))
  if (peers.some(peer => peer.id === previous)) {
    select.value = previous
    await loadConversation()
  }
  if (!peers.length) status.textContent = "No test contacts available."
}

select.addEventListener("change", () => {
  messages = []
  conversationReady = false
  run(loadConversation, "Could not open this test conversation.")
})
refresh.addEventListener("click", () => run(loadPeers, "Could not load test contacts. Use Refresh contacts to retry."))
send.addEventListener("click", () => run(async () => {
  const peerId = select.value
  const queued = await adapter.enqueueTest(peerId)
  messages = await adapter.readConversation(peerId)
  status.textContent = `${queued.id}: Queued in demo. Nothing was delivered.`
}, "Could not queue the test message."))
reply.addEventListener("click", () => run(async () => {
  const peerId = select.value
  await adapter.simulateReply(peerId)
  messages = await adapter.readConversation(peerId)
  status.textContent = "Linked test reply added. No real agent responded."
}, "Could not add a simulated reply."))
run(loadPeers, "Could not load test contacts. Use Refresh contacts to retry.")
