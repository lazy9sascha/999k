import assert from "node:assert/strict"
import { test } from "node:test"
import { createDemoAdapter } from "./demo.mjs"

test("lists stable synthetic contact IDs and independent empty conversations", async () => {
  const demo = createDemoAdapter()
  const peers = await demo.listPeers()
  assert.deepEqual(peers.map(peer => peer.id), ["demo-dot", "demo-instinct", "demo-cue", "demo-muse", "demo-grok"])
  assert.ok(peers.every(peer => peer.label.endsWith("(test)")))
  peers[0].id = "changed"
  assert.equal((await demo.listPeers())[0].id, "demo-dot")
  assert.deepEqual(await demo.readConversation("demo-dot"), [])
})

test("stores one queued test request and attaches only the exact simulated reply", async () => {
  const demo = createDemoAdapter()
  const receipt = await demo.enqueueTest("demo-dot")
  assert.equal(receipt.status, "queued")
  assert.deepEqual((await demo.readConversation("demo-dot")).map(m => [m.id, m.kind]), [[receipt.id, "request"]])
  await assert.rejects(demo.enqueueTest("demo-dot"), /already queued/)
  await assert.rejects(demo.simulateReply("demo-cue"), /No pending/)
  await demo.simulateReply("demo-dot")
  const [request, reply] = await demo.readConversation("demo-dot")
  assert.equal(reply.kind, "reply")
  assert.equal(reply.replyTo, request.id)
  assert.notEqual(reply.id, request.id)
  assert.deepEqual(await demo.readConversation("demo-cue"), [])
  await assert.rejects(demo.simulateReply("demo-dot"), /No pending/)
  assert.deepEqual(await createDemoAdapter().readConversation("demo-dot"), [])
})

test("rejects unlisted IDs and display names", async () => {
  const demo = createDemoAdapter()
  for (const id of ["", "Dot", "dot", "demo-missing"]) {
    await assert.rejects(demo.readConversation(id), /Unknown test contact/)
    await assert.rejects(demo.enqueueTest(id), /Unknown test contact/)
    await assert.rejects(demo.simulateReply(id), /Unknown test contact/)
  }
})
