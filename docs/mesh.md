# Mesh peers

Other machines running Wizard, what each one advertises, and what this machine has decided about it.

```bash
wizard peers address                       # this machine's address, to paste elsewhere
wizard peers add wiz1AbC…                  # a peer, from a pasted address
wizard peers list                          # trust state and presence, from the local store
wizard peers trust wiz1AbC… trusted        # trusted | known | blocked
wizard peers forget wiz1AbC…

wizard peers ping wiz1AbC…                 # is it there, and how far away
wizard peers refresh wiz1AbC…              # fetch its name and capability into the store
wizard peers watch wiz1AbC…                # render its live session in this terminal
```

Those eight are the whole surface; there is no `revoke`, because revoking is what `trust <peer> known|blocked` and `forget` do.

The first five answer out of `~/.wizard` and contact nobody. The last three reach another machine, which needs a route here (`[mesh.routes]`, or mDNS) and `[mesh] listen = true` there. **None of them listens** — but all eight open a socket, and it is worth being exact about which one: every branch builds the same transport with `listen` forced off (`src/plugins/mesh/cli.rs:151`), and that binds an *ephemeral* UDP port to dial from (`0.0.0.0:0`, `src/plugins/mesh/quic.rs:274`), never the configured one. So `wizard peers list` while a session holds `4242` open does not fight that session for it, and nothing an inbound packet arrives at is listening for it. A bind that fails is fatal only for the three commands that need a machine on the other end.

## What ships, and what does not

There are two transports. The **loopback** one runs in this process, opens no socket and speaks no wire format; it is what a single node talks to and the fallback `wizard peers` drops to on a machine with no usable UDP stack. The **QUIC** one crosses machines, and **its listener is off by default**: nothing *accepts* a connection until `[mesh] listen` is set in `config.toml`, and until then no session joins the mesh at all (`MeshTee::join` answers `None`, `src/app/tee.rs`). The precise claim is about listening rather than about sockets — a `wizard peers` command binds an ephemeral client port whatever the config says, as above — and it is the one that matters: a mesh that *accepted* connections on install would be a security surface nobody asked for.

The scope is narrower than "peer-to-peer" usually implies, and the narrowing is deliberate rather than incidental:

- **Two nodes on different machines, each directly reachable or on the same LAN.** There is no NAT traversal, no relay, no hole punching, and no anonymizing overlay. If neither machine can open a UDP socket the other can reach, they cannot talk, and nothing in this release changes that.
- **Three message kinds:** liveness, announcement, and session-event subscription. Delegated work — sending a task to a peer and having it run — is not built. There is no task frame on the wire, because a wire format that carried a task nothing would run is a wire format that has to keep carrying it.
- **`wizard peers list` still does not contact anybody.** It is the store and the decisions. The presence column is what this machine last *observed*, never a live probe: a peer that went dark a minute ago reads as `stale`, and a pasted address that has never answered reads as `unseen` rather than as offline. A graph that is beautiful and lies about who is online is worse than a plain one that does not. `wizard peers ping` is the command that makes an observation, and it writes what it learned to disk before it returns.

## Turning the mesh on

Nothing below is on by default. Every key here is opt-in, and there is deliberately no key that offers this machine as somebody else's compute.

```toml
[mesh]
listen = true                 # accept inbound connections (default false)
listen_addr = "0.0.0.0:4242"  # where, when listening
mdns = true                   # announce on the local network and look for peers (default false)

# Where to find peers. Routing, not identity — see below.
[mesh.routes]
"wiz1AbC…" = "192.168.1.20:4242"
```

With `listen = false` a node can still dial peers it has routes for; what it cannot do is be dialled. That asymmetry is the point: reaching out is something this machine chose to do, and listening is something other machines get to do to it. A connection a node opened is never used to serve requests back to the peer that answered it, so `listen = false` really does mean "nobody can watch this machine".

That is also why a Wizard session only joins the mesh when `listen` is on. With it off there is no way for anybody to subscribe, so a session that bound a socket and assembled an announcement anyway would be paying the whole cost of the mesh in exchange for a stream nobody can open. When it *is* on, the session says so on its first line:

```
mesh: listening on 0.0.0.0:4242 as wiz1AbC… — trusted peers may watch this session
```

A socket this process opened because a config file asked it to is exactly the thing you should not have to go and check for.

### Identity is not location

A mesh address is a public key. It does not say where a machine is, and it never will, because that is exactly what makes it un-forgeable. So the `host:port` has to be written down separately, and `[mesh.routes]` is where.

A route carries **no authority at all.** It says where to send the first packet. Whether the machine that answers is really that peer is decided by the TLS handshake against the peer's key, so a wrong or hostile route produces a refused connection, never a misdirected stream. Adding a route is not adding a peer: `wizard peers add` is still the only way in, and it is still a paste and a human decision.

### mDNS, and what it does not do

With `mdns = true` a node advertises itself on the local link as `_wizard-mesh._udp.local.` and watches for others. It is the **second** mechanism, not the first.

- It **does not add peers.** A node found on the LAN does not enter the peer store and is not contactable. All browsing does for a stranger is put it in a list a surface may *show* you.
- It **does not grant trust.** Trust is three-state, human, and on disk. An mDNS packet is none of those.
- It **does not authenticate anything**, and does not need to. Any machine on the link can claim any TXT record; all a claim can do is fill in a route for a node you already added, and a route carries no authority.
- It **does not reach past the local link.** No gossip, no DHT, no rendezvous server.

Advertising broadcasts this machine's name and public key to every device on the network, which is why it is off until you ask.

## How two nodes actually connect

The connection *is* the authentication. Each node presents a self-signed X.509 certificate whose key is its ed25519 node key, and whose signature is over its own contents. Recovering a peer's id from that certificate is not a lookup or a name check: it is reading the key out of the one field TLS proves possession of, and then verifying the certificate's own signature under it with `verify_strict`.

So there is no separate "check the signature on the announcement" step to forget. A dialled connection is refused unless the certificate carries the id that was dialled — a machine answering on the right address with the wrong key is a different machine, not a renamed one. An accepted connection is refused unless the certificate is a node this machine already has a peer record for and has not blocked: **a stranger does not get to learn that this node exists**, let alone what it is called or what models it runs.

There is no certificate authority, no chain, and no expiry. A chain is always one certificate long, and one arriving with intermediates is refused rather than skipped past. Nothing about a mesh identity expires, because there is no issuer to decline to renew it; the way to stop trusting a machine is `wizard peers trust <address> blocked`, which takes effect immediately.

The wire format is versioned from its first byte, and the version appears again in the TLS ALPN so that two incompatible nodes fail to *connect* rather than failing to understand each other. Every frame is a six-byte header — version, kind, and a 32-bit length — followed by a body. The length is checked against a hard cap before a byte of the body is read, so a peer cannot make this process allocate by claiming a large one.

## Identity is the address

A node's id is an ed25519 public key, and its address is a reversible encoding of that key:

```
wiz1<43 characters of base64url>
```

Nothing assigns it, so nothing has to be asked where a node lives: there is no registry to look a name up in, because the name *is* the key. That is the whole of what makes this serverless, and it is why discovery is a paste rather than a lookup. There is no DHT, no bootstrap list and no rendezvous server, so the peer store never grows except by somebody deciding it should.

`wizard peers address` also prints a fingerprint, the same shape `wizard sync key` prints:

```
SHA256:9f2c…
```

Compare it out of band before trusting a machine. An address read off somebody else's screen is the one thing an attacker can substitute.

Commands that take a peer accept the full address or any unique prefix of one. `wizard peers list` prints the whole 47-character address in its last column so it can be copied, but every other message names a peer by a short prefix, and a prefix is what you will have to hand. An ambiguous prefix is refused rather than resolved to the first match: this is the argument to `trust` and `forget`, and resolving a typo to *some* peer would mean a mistyped prefix blocks, or trusts, the wrong machine.

## Trust is three states, decided by a human

| State | This machine contacts it | This machine may watch it | It may watch this machine |
|-------|------------------------|---------------------------|---------------------------|
| `blocked` | **no** | no | no |
| `known` | yes (`ping`, `refresh`) | no | no |
| `trusted` | yes | yes | yes |

There is no work column, because there is no work. `Trust::may_send_work` (`src/plugins/mesh/peer.rs:105`) is named for the tier that was cut, and the only thing that consults it is a subscription — `Mesh::subscribe` on the watching side (`src/plugins/mesh/mod.rs:911`) and the `Watch` frame on the publishing side (`src/plugins/mesh/quic.rs:814`). `wizard peers trust <peer> trusted` says as much on the way out: "That is all trust grants: no work is delegated in either direction, and watching is read-only" (`src/plugins/mesh/cli.rs:314`).

A pasted address lands at `known`. Adding is not a decision, and there is deliberately no `--trusted` flag on `add` that collapses the paste and the decision into one keystroke: a paste is a fact about an address, trust is a claim about a machine, and a human checks the two in different ways.

Nothing infers trust from behaviour, and nothing a peer says about itself moves the dial. Re-adding a blocked peer does not unblock it. Announcing again does not promote anybody.

Moving away from `trusted` drops that peer's live subscriptions in the same call, and writes the store before returning. A revocation that leaves a stream running has revoked nothing, and a decision that is not on disk when the process exits has revoked nothing either: the peer would be contactable again on the next run, and nobody would be told.

**Forgetting is not blocking.** A forgotten address pasted in again lands at `known`, so `wizard peers forget` on a blocked peer discards the decision that was keeping it out. The command says so when that is what you just did.

### A decision reaches the process that made it

Revocation is immediate *within the process that records it*, and across the network to the machine it is about. It does **not** reach another Wizard process on the same machine: a `wizard peers trust <peer> known` typed in one terminal does not sever a stream held by a session running in another, because that session is holding its own copy of the peer store in memory and nothing tells it to re-read the file. The decision is on disk immediately and takes effect for every process started afterwards.

So the way to stop a running session publishing to a peer is to end that session, or to revoke from the *other* machine — which does work, because it closes the QUIC connection. This is a gap, it is named rather than papered over, and closing it means giving a live session a reason to re-read `peers.json`.

## Watching a peer's session

```
$ wizard peers watch wiz1AbC…
watching workshop at wiz1AbC… — every line below marked `wiz1AbC… │` was written by that
machine; lines marked `wizard │` are wizard's own
wizard │ wiz1AbC… session "main"
wiz1AbC… │ ⚙ read_file({"path":"src/plugins/mesh/quic.rs"})
wiz1AbC… │   ✔ 1644 lines
wiz1AbC… │ the transport already handles this — see the module header.
wizard │ the stream from wiz1AbC… ended — the peer stopped trusting this machine, this
machine stopped trusting it, or the connection dropped. Nothing further will arrive on it.
```

A trusted peer's session events can be subscribed to and rendered in your own transcript: "watch my agent work". It is read-only, in both senses: you cannot drive the peer's session, and nothing that arrives on the stream can drive yours.

### Whose line is whose

A peer's turn rendering indistinguishably from your own output is not a missing nicety. It is a machine somebody else controls writing lines into a surface you read as your own agent, and every string on that stream was written by the far end.

So **every physical line** a peer wrote carries a marker built from that peer's public key — per line, not per message, because the sanitiser deliberately keeps newlines (indentation and paragraphs are most of what a transcript means) and an item-level prefix would leave every line after the first unmarked. Everything wizard itself says into a watched transcript carries `wizard │` instead. A peer cannot produce a line with that prefix, because every line it produces gets its own marker; and it cannot influence its own marker, which comes from the key rather than from the name it chose.

The session lifecycle lines are wizard's, not the peer's: "that node started a session" is an observation this machine made about a frame it received, and the peer chooses not a word of it. The line saying the stream ended is wizard's for the same reason — and it is printed rather than left as silence, because a screen that simply stops is indistinguishable from a peer that has gone quiet.

### One model, not two

What arrives is `AgentEvent`, so a peer's session folds through the same `TranscriptModel` the TUI uses. There is no reducer written for peers, and therefore nothing for a peer's transcript to drift away in — the same argument that removed the TUI's second transcript one level up.

What crosses the wire is `AgentEvent`, the same type the local agent loop emits. That is the whole design: a remote node's turn *is* a local event stream, so any surface that can render a turn renders a peer's turn with no translation layer to keep in step. The frame around it adds the four things an agent event cannot carry on its own: which node it came from, which of that node's sessions it belongs to, when this machine observed it, and whether it is a turn or a session starting or ending.

The rules, each enforced rather than documented:

- **Trusted peers only.** A stream from a merely-`known` node is an inbound channel nobody approved.
- **Revocation severs live streams, both ways.** Moving a peer off `trusted` ends the stream you are watching *and* the stream it is watching of yours, in the same call. Un-trusting a peer that keeps receiving your sessions has revoked nothing, and that is the more expensive half: one leaks a screen, the other leaks a workspace.
- **Everything on the stream is sanitised.** Escape sequences, zero-width characters and bidirectional overrides are stripped from every string in every event, on the way out *and* again on the way in, so a record decoded off a wire gets the same treatment as one built in this process. A peer's turn reaches a terminal and a GUI; an unsanitised event is an escape-sequence injection into someone else's screen. Line breaks and indentation survive, because a transcript is not a table label.
- **Requests do not cross, only reports.** An agent event that asks a machine to *do* something rather than reporting what happened, Wizard's own slash-command line, never leaves the boundary. A plan review does cross, because its text is worth reading, but its approval ticket is voided: a watcher can read the plan and cannot answer it.
- **Images do not cross.** An image on an event is a file on the sender's disk, and the surfaces open what they are pointed at. A watcher is told an image was produced; it is not handed a local path to open.
- **Backpressure is lossy, and the bound is a count.** 64 events are buffered per subscription, each carrying at most 16k characters of text. A subscriber that falls further behind drops events rather than growing a queue, and the drops are counted so a transcript can say "3 events were lost" instead of showing a hole. A queue that grows to fit its producer is not a queue.

- **Backpressure is visible.** The count of events lost to a slow subscription is printed when the stream ends, so a transcript with a gap in it says there was one.

- **The publisher consents too.** `wizard peers trust` is two decisions, not one, and they live on two machines. Trusting a peer here says this machine will *take* its stream. Whether that peer may watch *you* is its operator's decision on their machine, checked against their peer store before a single event is written — during the handshake, so a stranger never gets a stream to ask on, and again when the request arrives, because trust can change while a connection is open.

Nothing on a peer's stream may become trusted input to a prompt. Sanitising does not make it safe there; nothing does. A peer's turn is display data: it reaches a transcript and a graph, and there is no path from it into a system prompt, a tool argument, or a command dispatcher.

## Deny by default, everywhere

The peer store's defaults all lean the same way, and each is pinned by a test rather than by a comment:

- `accepts_work` is `false` unless a node says otherwise, and nothing in Wizard makes it true: a session advertises `false` (`src/app/tee.rs:128`) and no command sets it. It is a claim a peer makes *about itself*, it is displayed and nothing more — the `work` column of `wizard peers list` and a line in the graph inspector — and with delegated work cut there is nothing a `true` would unlock. A machine that is merely running Wizard is not somebody else's worker.
- Admission refuses anything that is not explicitly `trusted`. There is exactly one thing to be admitted to: a subscription, checked on both machines (above).
- Limits (`Limits`, `Meter`, `Mesh::admit`) are built and tested and **not on any live path**: nothing a peer can send costs this machine an API call, so nothing calls `admit` outside its own tests. They are the metering the cut tier would have needed — a trusted peer with a retry loop can spend a budget as effectively as a hostile one — kept rather than deleted, and named here as scaffolding rather than left to read as a control that is running.
- Nothing a peer sends is trusted input. Every string that crosses the boundary is sanitised at construction: control characters replaced, zero-width and bidirectional-override characters deleted, whitespace collapsed, length capped. It is data to render, never instructions to follow, and it must never reach a system prompt. Sanitising does not make it safe there; nothing does.

This codebase has shipped fail-open defaults before (a Telegram allowlist that defaulted to allow-all, project hooks that executed themselves on session start). A mesh is where that class of mistake stops being a local problem.

## Files

| Path | What |
|------|------|
| `~/.wizard/node.key` | This machine's ed25519 seed, written 0600 into an owner-only directory. Minted on first use by any `wizard peers` command. Losing it means a new identity, and every peer has to re-add you. |
| `~/.wizard/mesh/peers.json` | The peer store: the record, the decision, the limits. Written atomically and owner-only, so a crash mid-write cannot leave a truncated trust list that reads back as "no peers are blocked". |
| `~/.wizard/config.toml` | `[mesh]`: whether to listen, where, whether to use mDNS, and the routes. Every default is off. |

## What a watcher sees, and what it does not

A watched session publishes the same events the local transcript renders, taken from one hook where every agent event on the surface passes: a turn's stream, a session-start hook, a background task reporting in, a subagent run. There is no second filter deciding what is interesting enough to forward.

Which variants may cross is one exhaustive match, `AgentEvent::is_request`, next to the variants themselves — so adding one does not compile until somebody decides which half it is in. A **report** crosses; a **request** does not, because obeying one that came off a socket is letting a peer drive this machine.

Three of them are worth spelling out, because something is waiting on all three and they still cross:

- **A plan review** (`exit_plan`) crosses. Its text is the most interesting thing in a plan-mode turn and a watcher is there to read it. The approval ticket is voided on the way out, so a watcher can read the plan and cannot answer it.
- **An interview** crosses on the same terms.
- **A command's console** crosses, all three of `opened`, `output` and `closed`. A console *output* is plainly a report: while a command is blocked on `Do you want to continue? [Y/n]`, that question is the most interesting thing on the stream. A console *open* is the sharper call, and it is a report by the same rule — that a command on that machine is waiting on somebody is a fact about the turn — but what a claimed console ticket buys is a **writer into a shell on the publisher's machine**, which is the most dangerous thing any gate hands out. So the ticket is voided exactly as a plan's is: watching a peer's session never becomes typing into a peer's shell.

The one variant that does not cross is Wizard's own slash-command line. Arriving from a peer it is another machine driving this one's menu, so it does not become a frame at all: there is nothing on the wire to be dispatched.

The node key is also the mesh's TLS key. The certificate a node presents is derived from it and is a pure function of it, so it is not stored anywhere: it is rebuilt on every start, byte for byte the same.
