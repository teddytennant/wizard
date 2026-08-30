//! `wizard peers`: the terminal surface over [`super::Mesh`].
//!
//! Two halves. Most of it is a read of the local store or a decision written
//! back to it, and needs no network at all. Three commands — `ping`, `refresh`
//! and `watch` — reach an actual machine over [`QuicTransport`], which is what
//! makes the store's other columns fillable: a pasted address carries no name
//! and no capability, so a mesh that could never fetch an announcement would
//! render every peer as its own address forever.
//!
//! # This surface dials and never listens
//!
//! [`open`] builds the QUIC transport with `[mesh] listen` forced off, whatever
//! the config says, and that is a decision rather than an oversight. A listener
//! belongs to a long-running session (see [`super::MeshTee`]): it is the
//! thing a peer dials to watch a *session*, and there is no session here. A
//! one-shot command that bound the configured port would also fight the running
//! TUI for it, so `wizard peers list` would start failing on exactly the
//! machines that have the mesh switched on.
//!
//! A failure to bind even the ephemeral client socket is fatal for a command
//! that has to reach a peer and a warning for one that does not: `wizard peers
//! list` is a read of a file on this disk, and refusing to print it because a
//! UDP socket could not be bound would be a surface failing for a reason it
//! does not need.
//!
//! So the presence column is still what this machine last *observed*, and the
//! listing says so under every table, where somebody reading a column of
//! `unseen` will actually see it — but now `wizard peers ping` is the command
//! that makes an observation, and it says what it did.
//!
//! # The posture is the module's, not this file's
//!
//! [`super`] decides the security model and this surface neither widens nor
//! narrows it. Five things a CLI is tempted to soften, and does not here:
//!
//! - **Adding is not approving.** [`Mesh::add_peer`] lands a pasted address at
//!   [`Trust::Known`], and there is deliberately no `--trusted` flag on `add`
//!   collapsing the paste and the decision into one keystroke. A paste is a
//!   fact about an address; trust is a claim about a machine, and a human
//!   checks the two in different ways (hence the fingerprint that
//!   [`PeersCmd::Address`] prints).
//! - **Nothing here offers this machine as compute.** `accepts_work` stays
//!   false because no command sets it, so a node that is merely running is not
//!   somebody else's worker.
//! - **A blocked peer is never contacted.** Every command that opens a
//!   connection checks [`Trust::may_contact`] before it dials, so "blocked"
//!   costs the peer a packet rather than a refusal it could time.
//! - **Watching needs trust on both machines.** [`Mesh::subscribe`] refuses
//!   anything that is not [`Trust::Trusted`] here, and the far end refuses it
//!   again against its own store. Neither check is this file's to make.
//! - **Forgetting is not blocking.** [`Mesh::forget`] cannot tell which one
//!   was meant, so `wizard peers forget` says which one it did.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;

use clap::Parser as _;

use crate::app::PeerStream;
use crate::config::{Config, MeshConfig};

use super::consent::TrustLedger;
use super::discovery::Discovery;
use super::quic::QuicTransport;
use super::transport::Subscription;
use super::{
    Identity, LoopbackTransport, Mesh, NodeId, Peer, PeerStore, PeerText, Transport, Trust,
};

/// How long a network command waits for mDNS to fill in a missing route.
///
/// Only spent when `[mesh] mdns` is on *and* the peer has no route written
/// down, so the common case (a route in `config.toml`) pays none of it. Long
/// enough for a multicast round trip on a quiet link, short enough that a
/// command which is going to fail fails while somebody is still watching.
const MDNS_WAIT: Duration = Duration::from_secs(3);

/// What the presence column is and is not, printed under every listing.
///
/// Under the listing rather than only in `--help`, because presence is the
/// column most likely to be read as a live answer and the help text is not on
/// screen at the moment somebody reads a row.
const PRESENCE_NOTE: &str = "\
Presence is what this machine last observed, never a probe: `wizard peers list` reads
the local store and contacts nobody. `wizard peers ping <peer>` is the command that
makes an observation, and `wizard peers refresh <peer>` fills in a peer's name and
capability. Both need a route; see `[mesh]` in config.toml and docs/mesh.md.";

/// What to do when there are no peers yet. Discovery is a paste, so the empty
/// state is the one place that has to explain where the text comes from.
const EMPTY_STORE_HELP: &str = "\
A mesh address is pasted in, not discovered: there is no directory to look a node
up in, because a node's name is its public key. Run `wizard peers address` on the
other machine and paste what it prints into `wizard peers add <address>` here.";

/// What `wizard peers` is, in the one line `wizard --help` gives a
/// subcommand.
///
/// Core used to hold this sentence as a doc comment on its `clap` variant and
/// print it on every build, mesh or no mesh. It is registered now — see
/// [`super::MeshPlugin::apply`] — so a build without this plugin has no line
/// to print and `--help` drops the row instead of describing a surface that
/// is not there.
///
/// It is also the first paragraph of [`long_about`], because the two were
/// separate strings saying nearly the same thing and had already drifted:
/// core's ran to "watch its live session" and this file's stopped three
/// clauses earlier. No trailing full stop — the subcommand table does not
/// want one, and `long_about` adds it back for the paragraph it opens.
pub const SUMMARY: &str = "Mesh peers: other machines running Wizard, what each one advertises, \
                           and what this machine has decided about it. List the store, add a \
                           peer by pasted address, record a trust decision, forget one — and \
                           reach a peer over the network: ping it, refresh what it advertises, \
                           watch its live session";

/// The caveat under [`SUMMARY`]: what `list` does not do, and what reaching a
/// peer at all needs switched on.
const CAVEAT: &str = "`list` contacts nobody, so its presence column is what this machine last \
                      observed rather than a live probe; `ping` is the command that makes an \
                      observation. Reaching a peer needs a route for it here (`[mesh.routes]`, \
                      or mDNS on the same LAN) and `[mesh] listen = true` on that machine, \
                      which is off by default. Nothing here listens. See docs/mesh.md.";

/// The two paragraphs `wizard peers --help` opens with.
///
/// A function rather than a `const` because a `const` cannot concatenate
/// without pulling in a macro crate for it, and this runs once per `--help`.
fn long_about() -> String {
    format!("{SUMMARY}.\n\n{CAVEAT}")
}

/// The `wizard peers` argument list, parsed here rather than in core.
///
/// Core's clap variant is `Peers { args: Vec<String> }` and hands the vector
/// straight to [`run_args`]. This type is the reason for that indirection:
/// `Trust` below is [`super::Trust`], the peer store's own recorded decision,
/// and its `clap::ValueEnum` is derived on the store's type precisely so a
/// second spelling on the argument-parsing side cannot drift into a fourth
/// state. Core cannot name it, so core does not parse this.
///
/// `no_binary_name` because what arrives has already had `wizard peers`
/// stripped off it: `wizard peers trust <peer> trusted` reaches [`run_args`]
/// as `["trust", "<peer>", "trusted"]`.
///
/// `about` and `long_about` are spelled out rather than left to this doc
/// comment because clap would otherwise print the paragraph above — an
/// argument about where a plugin boundary goes — to somebody who typed
/// `wizard peers --help` wanting to know what `refresh` does.
#[derive(Debug, clap::Parser)]
#[command(
    name = "wizard peers",
    no_binary_name = true,
    disable_help_subcommand = true,
    about = "Mesh peers: other machines running Wizard, what each one advertises, and what \
             this machine has decided about it.",
    long_about = long_about()
)]
pub struct PeersCli {
    #[command(subcommand)]
    pub cmd: PeersCmd,
}

/// `wizard peers` subcommands. Self-contained in the sense `sync` is: no
/// config beyond `[mesh]`, no onboarding, no LLM. They read and write
/// ~/.wizard/mesh/peers.json and ~/.wizard/node.key.
///
/// Five of them touch nothing else (`list`, `address`, `add`, `trust`,
/// `forget`); three reach a peer over the network (`ping`, `refresh`,
/// `watch`), which needs a route and a listening far end. **None of them
/// listens**: this surface dials, so running `wizard peers` while a session
/// holds `[mesh] listen` open does not fight it for the port.
///
/// The security posture is the module's ([`super`]) and this surface does
/// not soften it. A pasted address lands at [`Trust::Known`],
/// which may neither be sent work nor submit any; `accepts_work` is false
/// until this machine says otherwise; trust is a three-state decision a human
/// makes and nothing infers it from how a peer behaves; and a blocked peer is
/// never contacted, which is why there is no command here that reaches out to
/// one.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum PeersCmd {
    /// List the peers on this machine with their trust state and presence.
    /// Reads the local store only: presence is when this machine last
    /// observed the peer, never a live probe, so a peer that went dark two
    /// minutes ago reads as stale rather than as online.
    List,

    /// Print this machine's own mesh address and fingerprint, the text
    /// another machine pastes into `wizard peers add`. Mints
    /// ~/.wizard/node.key on first use.
    Address,

    /// Add a peer from a pasted address.
    ///
    /// Adding is not a decision: the peer lands at `known`, which is approved
    /// for nothing. Re-adding a peer does not change what was decided about
    /// it either, so pasting a blocked peer's address again does not unblock
    /// it.
    Add {
        /// The peer's address (`wiz1...`), as printed by
        /// `wizard peers address` on that machine.
        address: String,
    },

    /// Record what this machine has decided about a peer.
    ///
    /// Moving away from `trusted` also drops anything live for that peer in
    /// the same call, because a revocation that leaves a stream running has
    /// revoked nothing.
    Trust {
        /// The peer: its full address, or a unique prefix of one. An
        /// ambiguous prefix is refused rather than resolved to the first
        /// match.
        peer: String,

        /// blocked (never contacted), known (approved for nothing), or
        /// trusted (may exchange work, within its limits).
        #[arg(value_enum)]
        state: crate::plugins::mesh::Trust,
    },

    /// Drop a peer's record entirely.
    ///
    /// Not the same as blocking. A forgotten address pasted in again lands at
    /// `known`, so forgetting a blocked peer discards the decision that was
    /// keeping it out; use `trust <peer> blocked` when that is what you meant.
    Forget {
        /// The peer: its full address, or a unique prefix of one.
        peer: String,
    },

    /// Ask a peer whether it is there, and how long the round trip took.
    ///
    /// The one command whose answer is a fact about *now* rather than about
    /// the store. It needs a route (`[mesh.routes]`, or mDNS on the same
    /// LAN) and a listener on the far end. A blocked peer is not contacted.
    Ping {
        /// The peer: its full address, or a unique prefix of one.
        peer: String,
    },

    /// Fetch a peer's announcement and fold it into the local store.
    ///
    /// What fills in a peer's name and capability: a pasted address carries
    /// neither, so until a peer is refreshed it renders as its own address.
    /// The record is written to disk before this returns, and the peer is
    /// marked seen at the moment it answered.
    Refresh {
        /// The peer: its full address, or a unique prefix of one.
        peer: String,
    },

    /// Watch a trusted peer's live session stream.
    ///
    /// Read-only in both senses: this cannot drive the peer's session, and
    /// nothing arriving on the stream drives this one. Trusted peers only,
    /// on both machines — the far end decides separately whether this one may
    /// watch it.
    ///
    /// Every line the peer wrote is marked with the peer's address, and every
    /// line wizard wrote is marked `wizard`. Runs until the stream ends (the
    /// peer revoked it, or the connection dropped) or Ctrl-C.
    Watch {
        /// The peer: its full address, or a unique prefix of one.
        peer: String,

        /// Stop after this many events instead of running until the stream
        /// ends.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },
}

/// Parse `wizard peers`'s argument list and run it. What
/// [`MeshPlugin`](super::MeshPlugin) registers under
/// [`entrypoint::PEERS`](crate::entrypoint::PEERS).
///
/// A parse failure — a misspelled subcommand, a trust state the store cannot
/// hold, `--help` — is clap's to render, and it renders it against *this*
/// command rather than against `wizard`, so the usage line and the subcommand
/// list are the eight below. `exit()` rather than returning the error because
/// clap's own exit code carries the difference between help (0) and a bad
/// argument (2), and flattening both into the dispatch chain's `Err` would
/// make `wizard peers --help` a failure.
pub async fn run_args(args: Vec<String>) -> Result<i32> {
    let parsed = match PeersCli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(err) => err.exit(),
    };
    run(parsed.cmd).await
}

/// Run one `wizard peers` subcommand. Returns the process exit code.
///
/// Every branch builds the same [`Mesh`], including the read-only ones. The
/// identity is part of it because [`Mesh::with_consent`] drops any stored
/// record whose id is this node (a peer file can be hand-edited, or restored
/// from another machine's backup), so a listing built without one could show
/// this machine as its own peer. The cost is that `wizard peers list` mints
/// `~/.wizard/node.key` on a machine that has never run a mesh command. That
/// key is this node's name, generating it is idempotent, and it is the same
/// file [`PeersCmd::Address`] would have created a moment later anyway.
pub async fn run(cmd: PeersCmd) -> Result<i32> {
    let wizard_dir = Config::wizard_dir()?;
    let identity = Identity::load_or_generate(&wizard_dir)?;
    let store = PeerStore::load(&wizard_dir)?;
    // `[mesh]` and nothing else is read out of it: the routes to dial along,
    // and whether mDNS may be asked where a peer went.
    let config = Config::load()?.mesh;
    let (mut mesh, transport) = open(identity, store, &config)?;

    match cmd {
        PeersCmd::List => list(&mesh),
        PeersCmd::Address => address(&mesh),
        PeersCmd::Add { address } => add(&mut mesh, &address),
        PeersCmd::Trust { peer, state } => trust(&mut mesh, &peer, state).await,
        PeersCmd::Forget { peer } => forget(&mut mesh, &peer).await,
        PeersCmd::Ping { peer } => ping(&mut mesh, &reachable(transport)?, &config, &peer).await,
        PeersCmd::Refresh { peer } => {
            refresh(&mut mesh, &reachable(transport)?, &config, &peer).await
        }
        PeersCmd::Watch { peer, limit } => {
            watch(&mut mesh, &reachable(transport)?, &config, &peer, limit).await
        }
    }
}

/// Build the mesh this surface runs one command against.
///
/// The real transport, dialling only — see the module docs for why `listen` is
/// forced off here rather than honoured. The ledger is created once and handed
/// to *both* halves: `Mesh::new` would give the transport a private empty one,
/// which refuses everybody, and the failure would be silent and
/// indistinguishable from having no peers.
///
/// The fallback is deliberate and narrow. A machine with no usable UDP stack
/// still has a peer store on disk, and `wizard peers list` is a read of it; the
/// commands that actually need a socket ask for one through [`reachable`] and
/// get the reason it is missing.
///
/// `pub(crate)` because the graph explorer opens a mesh too, and the ledger
/// rule above is exactly the kind of thing a second call site gets wrong once
/// and then cannot see: a second implementation that used `Mesh::new` would
/// draw an explorer that serves nobody and looks like an explorer with no
/// peers.
pub(crate) fn open(
    identity: Identity,
    store: PeerStore,
    config: &MeshConfig,
) -> Result<(Mesh, Result<Arc<QuicTransport>>)> {
    let consent = TrustLedger::new();
    let dialling = MeshConfig {
        listen: false,
        ..config.clone()
    };
    match QuicTransport::from_config(&identity, consent.shared(), &dialling) {
        Ok(transport) => {
            let mesh = Mesh::with_consent(
                identity,
                store,
                Arc::clone(&transport) as Arc<dyn Transport>,
                consent,
            );
            Ok((mesh, Ok(transport)))
        }
        Err(why) => {
            // Reported here as well as at the point of use, because the two
            // differ: this says *why*, and the caller only knows *that*.
            tracing::debug!("mesh: no network transport for `wizard peers`: {why:#}");
            let mesh =
                Mesh::with_consent(identity, store, Arc::new(LoopbackTransport::new()), consent);
            Ok((mesh, Err(why)))
        }
    }
}

/// The transport, or the reason there is not one, for a command that cannot
/// work without it.
fn reachable(transport: Result<Arc<QuicTransport>>) -> Result<Arc<QuicTransport>> {
    transport.context(
        "this command has to reach another machine and wizard could not open a socket to \
         dial from",
    )
}

/// One line of the peer table. Header and rows share it so the columns cannot
/// drift apart as either is edited.
fn row(peer: &str, trust: &str, presence: &str, work: &str, address: &str) -> String {
    format!("{peer:<24}  {trust:<8}  {presence:<8}  {work:<6}  {address}")
}

/// How a peer's own `accepts_work` claim reads in the table.
///
/// "offers", not "yes": this is what the *peer* advertises about itself, and
/// what this machine will actually let it do is the trust column beside it. A
/// reader who saw only one of the two would draw the wrong conclusion from
/// either, so both are printed and the wording keeps them apart.
fn work_column(accepts_work: bool) -> &'static str {
    if accepts_work { "offers" } else { "no" }
}

/// Print the peer table: what each peer is called, what was decided about it,
/// and when it was last observed.
fn list(mesh: &Mesh) -> Result<i32> {
    let now = Utc::now();
    let peers: Vec<&Peer> = mesh.store().iter().collect();
    if peers.is_empty() {
        println!("no peers on this machine.");
        println!();
        println!("{EMPTY_STORE_HELP}");
        return Ok(0);
    }

    println!("{}", row("peer", "trust", "presence", "work", "address"));
    for peer in &peers {
        let label = truncate(&peer.node.label(), 24);
        let trust = peer.trust.label();
        let presence = peer.presence(now).label();
        let work = work_column(peer.node.caps.accepts_work);
        let addr = peer.node.addr();
        println!("{}", row(&label, trust, presence, work, &addr));
    }

    let (online, stale, unseen) = mesh.store().presence_counts(now);
    let total = peers.len();
    println!();
    println!("{total} peer(s): {online} online, {stale} stale, {unseen} unseen.");
    println!("{PRESENCE_NOTE}");
    Ok(0)
}

/// The advice printed under this machine's own address.
const ADDRESS_NOTE: &str = "\
Compare the fingerprint out of band before the other machine trusts this one. It is
the same shape `wizard sync key` prints, and it is there for the same reason: an
address read off somebody else's screen is the one thing an attacker can substitute.

This node advertises nothing and accepts work from nobody.";

/// Print this machine's address and fingerprint: the text another machine
/// pastes into `wizard peers add`.
fn address(mesh: &Mesh) -> Result<i32> {
    let id = mesh.local_id();
    println!("{}", id.address());
    println!();
    println!("fingerprint: {}", id.fingerprint());
    println!();
    println!("{ADDRESS_NOTE}");
    Ok(0)
}

/// Add a pasted address.
fn add(mesh: &mut Mesh, address: &str) -> Result<i32> {
    // Looked up before the add purely to tell a new peer from an existing one.
    // `add_peer` returns the trust *after* the call, which for a peer that was
    // already blocked is `blocked`, and reporting that as the outcome of an add
    // would read as though pasting an address had just blocked a machine.
    let parsed = NodeId::parse_address(address).ok();
    let existing = parsed.and_then(|id| mesh.store().trust_of(&id));
    let (id, trust) = mesh.add_peer(address, Utc::now())?;

    let Some(before) = existing else {
        println!("added {} at trust '{}'.", id.short(), trust.label());
        println!();
        println!(
            "'{}' means approved for nothing: no work goes to it and none is taken \
             from it. Check the fingerprint against what that machine printed, then decide:",
            trust.label()
        );
        println!();
        println!("  fingerprint: {}", id.fingerprint());
        println!("  wizard peers trust {} trusted", id.address());
        return Ok(0);
    };
    println!(
        "{} was already a peer, recorded as '{}'. Adding an address is not a decision \
         and does not change one.",
        id.short(),
        before.label()
    );
    Ok(0)
}

/// Record a trust decision.
async fn trust(mesh: &mut Mesh, selector: &str, state: Trust) -> Result<i32> {
    let id = resolve(mesh, selector)?;
    let before = mesh
        .store()
        .trust_of(&id)
        .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
    if before == state {
        println!(
            "{} is already '{}'; nothing changed.",
            id.short(),
            state.label()
        );
        return Ok(0);
    }

    mesh.set_trust(&id, state).await?;
    println!("{}: {} -> {}", id.short(), before.label(), state.label());

    // What the decision now permits, in the store's own terms. A three-state
    // decision whose middle state goes unexplained is one people read as a
    // two-state decision, and `known` is where every pasted address lands.
    match state {
        Trust::Trusted => println!(
            "You may now watch this peer's session stream. That is all trust grants: \
             no work is delegated in either direction, and watching is read-only. \
             Whether it may watch you back is its own machine's decision, not this one."
        ),
        Trust::Known => println!(
            "No session stream flows either way. Anything live for this peer was \
             dropped in the same call."
        ),
        Trust::Blocked => println!(
            "This peer is not contacted at all any more, and anything live for it was \
             dropped in the same call. Re-adding its address does not undo this."
        ),
    }
    Ok(0)
}

/// Drop a peer's record.
async fn forget(mesh: &mut Mesh, selector: &str) -> Result<i32> {
    let id = resolve(mesh, selector)?;
    let before = mesh.store().trust_of(&id);
    if !mesh.forget(&id).await? {
        bail!("no peer {} in the store", id.short());
    }
    println!("forgot {}.", id.short());
    if before == Some(Trust::Blocked) {
        // The one case where forgetting undoes a decision rather than tidying
        // up after one, and the one an operator is least likely to have meant.
        println!();
        println!(
            "That peer was blocked. Forgetting discards the decision: pasting its address \
             again lands it at 'known', not back at 'blocked'. If the intent was to keep it \
             out, add it again and run `wizard peers trust <address> blocked`."
        );
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// The commands that reach another machine
// ---------------------------------------------------------------------------

/// Refuse before dialling anything this machine has decided not to talk to.
///
/// Checked here as well as inside [`Mesh::refresh`] and [`Mesh::subscribe`],
/// because [`QuicTransport::ping`] is the transport's own method and has no
/// trust decision behind it: liveness is a transport question and "may I ask"
/// is not. Without this, `wizard peers ping` would be the one command on this
/// surface that contacts a blocked peer.
fn require_contactable(mesh: &Mesh, id: &NodeId) -> Result<()> {
    let trust = mesh
        .store()
        .trust_of(id)
        .ok_or_else(|| anyhow!("no peer {} in the store", id.short()))?;
    if !trust.may_contact() {
        bail!(
            "peer {} is blocked; wizard does not contact blocked peers. \
             `wizard peers trust {} known` first if that is no longer what you meant",
            id.short(),
            id.short()
        );
    }
    Ok(())
}

/// Make sure the transport knows where to send the first packet.
///
/// A route is not identity and grants nothing: the handshake decides whether
/// the machine that answers is the peer, so a wrong route is a refused
/// connection rather than a misdirected stream. What it *is* is mandatory, and
/// it is the first wall anybody hits, because the address they pasted looks
/// like it should already be enough.
///
/// mDNS is consulted only when `[mesh] mdns` is on and only when there is no
/// route written down, so the ordinary case pays nothing for it.
async fn ensure_route(
    transport: &Arc<QuicTransport>,
    config: &MeshConfig,
    id: &NodeId,
) -> Result<()> {
    if transport.route(id).is_some() {
        return Ok(());
    }
    if !config.mdns {
        bail!(
            "no route to {}: a mesh address is a public key, not a location, so this machine \
             needs a `host:port` for it.\n\n\
             Write one down in config.toml:\n\n  \
             [mesh.routes]\n  \"{}\" = \"192.0.2.10:4242\"\n\n\
             or set `[mesh] mdns = true` to look for it on this LAN. Either way the far end \
             needs `[mesh] listen = true`, which is off by default.",
            id.short(),
            id.address()
        );
    }

    println!(
        "no route to {}; looking on the local network for {}s…",
        id.short(),
        MDNS_WAIT.as_secs()
    );
    let discovery =
        Discovery::start(transport).context("browsing the local network for the peer's address")?;
    let deadline = std::time::Instant::now() + MDNS_WAIT;
    while transport.route(id).is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Stopped either way: the route, if one arrived, is installed on the
    // transport and outlives the browse. Leaving the daemon running would keep
    // a multicast listener open for the rest of a one-shot command.
    discovery.stop();

    if transport.route(id).is_none() {
        bail!(
            "no route to {}: nothing on this link advertised it within {}s. It may be off, on \
             another network, or not listening (`[mesh] listen` is off by default). A route in \
             `[mesh.routes]` works without mDNS.",
            id.short(),
            MDNS_WAIT.as_secs()
        );
    }
    Ok(())
}

/// Ask a peer whether it is there.
///
/// The one command on this surface whose answer is about *now*. The observation
/// is written to disk before it returns: a presence learned and then thrown
/// away would leave the listing reading `unseen` on a peer somebody just
/// watched answer.
async fn ping(
    mesh: &mut Mesh,
    transport: &Arc<QuicTransport>,
    config: &MeshConfig,
    selector: &str,
) -> Result<i32> {
    let id = resolve(mesh, selector)?;
    require_contactable(mesh, &id)?;
    ensure_route(transport, config, &id).await?;

    let elapsed = transport.ping(&id).await?;
    mesh.mark_seen(&id, Utc::now());
    // `persist`, not `save`: an ephemeral store (a test's) has deliberately
    // chosen not to have a file, and there is nothing there to fail.
    mesh.persist()
        .context("the peer answered but the observation could not be written to disk")?;
    println!(
        "{} answered in {:.1}ms — presence is now 'online'.",
        id.short(),
        elapsed.as_secs_f64() * 1000.0
    );
    Ok(0)
}

/// Fetch a peer's announcement and fold it into the store.
///
/// The only path a peer's *name* has. A pasted address carries an identity and
/// nothing else, so a machine that never refreshed anybody would render every
/// peer as its own address forever.
async fn refresh(
    mesh: &mut Mesh,
    transport: &Arc<QuicTransport>,
    config: &MeshConfig,
    selector: &str,
) -> Result<i32> {
    let id = resolve(mesh, selector)?;
    require_contactable(mesh, &id)?;
    ensure_route(transport, config, &id).await?;

    let caps = mesh.refresh(&id, Utc::now()).await?;
    mesh.persist()
        .context("the peer answered but its record could not be written to disk")?;

    let label = mesh
        .store()
        .get(&id)
        .map_or_else(|| id.short(), |peer| peer.node.label());
    println!("{}: {label}", id.short());
    for kind in super::CapabilityKind::ALL {
        let entries: Vec<&str> = caps.entries(kind).iter().map(PeerText::as_str).collect();
        if !entries.is_empty() {
            println!("  {}: {}", kind.label(), entries.join(", "));
        }
    }
    println!("  accepts work: {}", work_column(caps.accepts_work));
    println!();
    println!(
        "That is what {} says about itself, and nothing more. What it may actually do here is \
         the trust decision beside it.",
        id.short()
    );
    Ok(0)
}

/// Watch a trusted peer's live session stream.
///
/// The receiving end of [`super::MeshTee`]. What arrives is
/// [`crate::agent::AgentEvent`], so it renders through the same
/// [`crate::transcript::TranscriptModel`] the TUI uses rather than a second
/// reducer written for peers — see [`PeerStream`], which is that model plus the
/// attribution a line-oriented surface needs.
async fn watch(
    mesh: &mut Mesh,
    transport: &Arc<QuicTransport>,
    config: &MeshConfig,
    selector: &str,
    limit: Option<usize>,
) -> Result<i32> {
    let id = resolve(mesh, selector)?;
    require_contactable(mesh, &id)?;
    ensure_route(transport, config, &id).await?;

    let label = mesh
        .store()
        .get(&id)
        .map_or_else(|| id.short(), |peer| peer.node.label());
    let mut subscription = mesh.subscribe(&id).await?;
    let mut screen = PeerStream::new(&id, label);
    println!("{}", screen.banner());
    let taken = stream(&mut screen, &mut subscription, limit, &mut |line| {
        println!("{line}")
    })
    .await;
    println!(
        "{}",
        PeerStream::local(&format!(
            "{taken} event(s) rendered, {} lost to backpressure.",
            subscription.dropped()
        ))
    );
    Ok(0)
}

/// The watch loop proper: events in, attributed lines out. Returns how many
/// events it took.
///
/// `out` is a sink rather than a `println!` so a test can drive this against a
/// real socket without a terminal — which is what makes "one node watches
/// another and sees its turns render" a test rather than a screenshot.
///
/// `+ Send` on the sink is not decoration: this runs inside the future
/// `MeshPlugin` hands the kernel as a [`Subcommand`](crate::entrypoint::Subcommand),
/// and a `&mut dyn FnMut` held across the `recv().await` below is what would
/// make that whole future non-`Send`. Every caller's sink is a `println!` or a
/// `Vec` push, so the bound costs nothing and the alternative is a plugin
/// entrypoint that cannot be spawned.
///
/// Two kinds of line come out of here and they are never confusable. A line
/// derived from a peer's *content* goes through [`PeerStream::apply`], which
/// stamps the peer's marker onto every one of its physical lines. A line wizard
/// wrote — the session frames, the end of the stream — goes through
/// [`PeerStream::local`]. The peer chooses neither marker: one is derived from
/// its public key and the other is a constant it never touches.
async fn stream(
    screen: &mut PeerStream,
    subscription: &mut Subscription,
    limit: Option<usize>,
    out: &mut (dyn FnMut(&str) + Send),
) -> usize {
    use super::PeerEventKind;

    let peer = subscription.peer().short();
    let mut session: Option<String> = None;
    let mut taken = 0usize;

    while limit.is_none_or(|cap| taken < cap) {
        let Some(event) = subscription.recv().await else {
            // The one thing a watcher has to be told, because the alternative
            // is a screen that simply stops: a revocation on either machine
            // closes the QUIC connection, and every stream on it fails at once.
            out(&PeerStream::local(&format!(
                "the stream from {peer} ended — the peer stopped trusting this machine, this \
                 machine stopped trusting it, or the connection dropped. Nothing further will \
                 arrive on it."
            )));
            return taken;
        };
        taken += 1;

        // One subscription carries every session that node is running, so say
        // which one each stretch belongs to rather than interleaving them
        // silently.
        if session.as_deref() != Some(event.session.as_str()) {
            session = Some(event.session.as_str().to_string());
            out(&PeerStream::local(&format!(
                "{peer} session {:?}",
                event.session.as_str()
            )));
        }

        match &event.what {
            PeerEventKind::Turn(_) => {
                let Some(report) = event.report() else {
                    continue;
                };
                for line in screen.apply(report) {
                    out(&line);
                }
            }
            // Frames rather than content: wizard is the speaker here and the
            // peer is only the subject, so these are wizard's lines. The peer
            // cannot choose a word of them.
            other => out(&PeerStream::local(&format!(
                "{peer} {}",
                other.label().replace('_', " ")
            ))),
        }
    }
    taken
}

/// Resolve what the operator typed into exactly one peer in the store.
///
/// A full address is the unambiguous form and is tried first. Anything else is
/// matched as a prefix of the peers' addresses, because every message this
/// surface prints names a peer by [`NodeId::short`] and somebody who has just
/// read one of those will type it rather than go and find the whole 47
/// characters again.
///
/// An ambiguous prefix is refused rather than resolved to the first match.
/// This selector reaches `trust` and `forget`, so resolving a typo to *some*
/// peer would make a mistyped prefix a security incident: the wrong machine
/// gets blocked, or worse, trusted. It is the same reason
/// [`NodeId::parse_address`] is strict about the alphabet and the length
/// instead of accepting anything that decodes.
fn resolve(mesh: &Mesh, selector: &str) -> Result<NodeId> {
    let selector = selector.trim();
    if selector.is_empty() {
        bail!("no peer given; `wizard peers list` shows what is on this machine");
    }

    if let Ok(id) = NodeId::parse_address(selector) {
        return match mesh.store().get(&id) {
            Some(_) => Ok(id),
            None => Err(anyhow!(
                "{} is a valid address but not a peer of this machine; \
                 `wizard peers add {selector}` first",
                id.short()
            )),
        };
    }

    let matches: Vec<NodeId> = mesh
        .store()
        .iter()
        .map(Peer::id)
        .filter(|id| id.address().starts_with(selector))
        .collect();
    match matches.as_slice() {
        [] => bail!("no peer matches {selector:?}; `wizard peers list` shows what is here"),
        [only] => Ok(*only),
        many => bail!(
            "{selector:?} matches {} peers ({}); paste the whole address",
            many.len(),
            many.iter()
                .map(NodeId::short)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Clip a label to `max` characters so one peer's name cannot push the rest of
/// a row off the side of the terminal.
///
/// Characters, not bytes. [`super::PeerText`] already capped the name and
/// stripped everything that draws nothing, but it did not make it ASCII, and a
/// byte truncation would split a multi-byte character.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept = max.saturating_sub(1);
    text.chars().take(kept).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::mesh::Node;

    /// A mesh over an ephemeral store holding `count` deterministic peers.
    fn mesh_with(count: usize) -> Mesh {
        let store = crate::plugins::mesh::peer::synthetic_store(count, 7, Utc::now());
        Mesh::new(
            Identity::from_seed([9u8; 32]),
            store,
            Arc::new(LoopbackTransport::new()),
        )
    }

    /// A pasteable address for a deterministic identity.
    fn address_of(seed: u8) -> String {
        Identity::from_seed([seed; 32]).id().address()
    }

    /// Parse an argument vector the way `wizard peers` hands one over: no
    /// binary name, no subcommand name, just what followed `peers`.
    fn parse(args: &[&str]) -> Result<PeersCmd, clap::Error> {
        PeersCli::try_parse_from(args.iter().copied()).map(|parsed| parsed.cmd)
    }

    /// The eight subcommands, parsed here rather than in `src/cli.rs`.
    ///
    /// This test moved with the enum. Core's half is now one assertion that
    /// the argument list crosses unchanged
    /// (`cli::tests::peers_takes_its_whole_argument_list_unparsed`); what the
    /// arguments *mean* is this file's, because this is where the types are.
    #[test]
    fn peers_subcommands_parse() {
        assert!(matches!(parse(&["list"]).expect("list"), PeersCmd::List));
        assert!(matches!(
            parse(&["address"]).expect("address"),
            PeersCmd::Address
        ));

        let PeersCmd::Add { address } = parse(&["add", "wiz1abc"]).expect("add") else {
            panic!("expected add");
        };
        assert_eq!(address, "wiz1abc");

        assert!(matches!(
            parse(&["forget", "wiz1abc"]).expect("forget"),
            PeersCmd::Forget { .. }
        ));

        let PeersCmd::Watch { peer, limit } =
            parse(&["watch", "wiz1abc", "--limit", "3"]).expect("watch")
        else {
            panic!("expected watch");
        };
        assert_eq!((peer.as_str(), limit), ("wiz1abc", Some(3)));
    }

    /// The three states are the store's, not this surface's: a fourth spelling
    /// on the argument-parsing side would be a CLI that can express a decision
    /// `peers.json` cannot record, which is why [`Trust`] derives `ValueEnum`
    /// itself instead of being mirrored.
    ///
    /// It is also the whole reason `wizard peers` is a plugin subcommand
    /// rather than a clap tree in core: core cannot name this type.
    #[test]
    fn peer_trust_takes_exactly_the_three_recorded_states() {
        for (raw, expected) in [
            ("blocked", Trust::Blocked),
            ("known", Trust::Known),
            ("trusted", Trust::Trusted),
        ] {
            let PeersCmd::Trust { peer, state } =
                parse(&["trust", "wiz1abc", raw]).expect("trust state parses")
            else {
                panic!("expected trust");
            };
            assert_eq!((peer.as_str(), state), ("wiz1abc", expected));
        }
        let err = parse(&["trust", "wiz1abc", "allowed"])
            .expect_err("a state the store cannot hold must be rejected");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    /// `--help` and a misspelled subcommand are answered against *this*
    /// command, which is the half of the passthrough core cannot check.
    ///
    /// The usage line matters: it is what somebody reads after mistyping, and
    /// on the arrangement this replaced it would have said
    /// `wizard peers [ARGS]...` and listed nothing.
    #[test]
    fn help_and_a_misspelling_are_answered_against_this_tree() {
        let help = parse(&["--help"]).expect_err("--help is an error carrying the help text");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let rendered = help.render().to_string();
        assert!(rendered.contains("wizard peers"), "{rendered}");
        for subcommand in [
            "list", "address", "add", "trust", "forget", "ping", "refresh", "watch",
        ] {
            assert!(
                rendered.contains(subcommand),
                "{subcommand} missing: {rendered}"
            );
        }

        let err = parse(&["lst"]).expect_err("a subcommand that does not exist");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn a_full_address_resolves_only_when_it_is_a_peer() {
        let mut mesh = mesh_with(0);
        let addr = address_of(1);
        let err = resolve(&mesh, &addr).expect_err("not a peer yet");
        assert!(format!("{err:#}").contains("not a peer"), "{err:#}");

        mesh.add_peer(&addr, Utc::now()).expect("add");
        assert_eq!(resolve(&mesh, &addr).expect("resolves").address(), addr);
    }

    #[test]
    fn a_prefix_resolves_and_an_ambiguous_one_is_refused() {
        let mut mesh = mesh_with(0);
        let addr = address_of(2);
        mesh.add_peer(&addr, Utc::now()).expect("add");
        let short: String = addr.chars().take(12).collect();
        assert_eq!(resolve(&mesh, &short).expect("prefix").address(), addr);

        // `wiz1` prefixes every address there is, so with two peers it names
        // neither. Refusing is the point: this selector reaches `trust` and
        // `forget`, and resolving a typo to whichever peer happened to sort
        // first would block or trust the wrong machine.
        mesh.add_peer(&address_of(3), Utc::now()).expect("add");
        let err = resolve(&mesh, "wiz1").expect_err("ambiguous prefix refused");
        assert!(format!("{err:#}").contains("matches 2 peers"), "{err:#}");
    }

    #[test]
    fn an_empty_selector_matches_nothing_rather_than_everything() {
        // `""` is a prefix of every address, so falling through to the prefix
        // match would make `wizard peers forget ""` mean "whichever peer sorts
        // first" on a store that holds exactly one.
        let mut mesh = mesh_with(0);
        mesh.add_peer(&address_of(4), Utc::now()).expect("add");
        let err = resolve(&mesh, "  ").expect_err("empty selector refused");
        assert!(format!("{err:#}").contains("no peer given"), "{err:#}");
    }

    #[test]
    fn adding_a_peer_from_this_surface_never_trusts_it() {
        // The posture this CLI must not soften: a paste is not an approval,
        // and there is no flag on `add` that turns it into one.
        let mut mesh = mesh_with(0);
        let addr = address_of(5);
        assert_eq!(add(&mut mesh, &addr).expect("add runs"), 0);
        let id = NodeId::parse_address(&addr).expect("address parses");
        assert_eq!(mesh.store().trust_of(&id), Some(Trust::Known));
        let peer = mesh.store().get(&id).expect("peer is in the store");
        assert!(!peer.node.caps.accepts_work);
    }

    #[tokio::test]
    async fn re_adding_a_blocked_peer_does_not_unblock_it() {
        let mut mesh = mesh_with(0);
        let addr = address_of(6);
        mesh.add_peer(&addr, Utc::now()).expect("add");
        let id = NodeId::parse_address(&addr).expect("address parses");
        mesh.set_trust(&id, Trust::Blocked).await.expect("block");

        add(&mut mesh, &addr).expect("re-add runs");
        assert_eq!(
            mesh.store().trust_of(&id),
            Some(Trust::Blocked),
            "pasting an address again must not clear a decision"
        );
    }

    #[tokio::test]
    async fn trust_moves_between_all_three_states() {
        let mut mesh = mesh_with(0);
        let addr = address_of(7);
        mesh.add_peer(&addr, Utc::now()).expect("add");
        let id = NodeId::parse_address(&addr).expect("address parses");

        for state in [Trust::Trusted, Trust::Blocked, Trust::Known] {
            trust(&mut mesh, &addr, state).await.expect("trust runs");
            assert_eq!(mesh.store().trust_of(&id), Some(state));
        }
    }

    #[tokio::test]
    async fn forgetting_removes_the_record_and_refuses_an_unknown_peer() {
        let mut mesh = mesh_with(0);
        let addr = address_of(8);
        mesh.add_peer(&addr, Utc::now()).expect("add");
        let id = NodeId::parse_address(&addr).expect("address parses");

        forget(&mut mesh, &addr).await.expect("forget runs");
        assert!(mesh.store().get(&id).is_none());
        let err = forget(&mut mesh, &addr)
            .await
            .expect_err("a peer that is gone cannot be forgotten twice");
        assert!(format!("{err:#}").contains("not a peer"), "{err:#}");
    }

    #[test]
    fn listing_an_empty_store_and_a_full_one_both_work() {
        assert_eq!(list(&mesh_with(0)).expect("empty listing"), 0);
        assert_eq!(list(&mesh_with(12)).expect("full listing"), 0);
    }

    #[test]
    fn a_long_peer_name_cannot_push_the_rest_of_the_row_off_screen() {
        let long = "x".repeat(64);
        let clipped = truncate(&long, 24);
        assert_eq!(clipped.chars().count(), 24);
        assert!(clipped.ends_with('…'));
        // Clipped by character, not by byte, so the result is always text.
        let wide = "スキル".repeat(20);
        assert_eq!(truncate(&wide, 10).chars().count(), 10);
        assert_eq!(truncate("short", 24), "short");
    }

    #[test]
    fn the_local_node_is_never_its_own_peer() {
        let mesh = mesh_with(0);
        assert_eq!(address(&mesh).expect("address prints"), 0);
        assert!(mesh.store().get(&mesh.local_id()).is_none());
    }

    #[test]
    fn a_node_cannot_add_itself() {
        let mut mesh = mesh_with(0);
        let own = mesh.local_id().address();
        let err = add(&mut mesh, &own).expect_err("self-add refused");
        let text = format!("{err:#}");
        assert!(text.contains("cannot be its own peer"), "{text}");
    }

    #[test]
    fn the_address_this_surface_prints_is_the_one_it_takes_back() {
        // `peers address` on one machine feeds `peers add` on another. If the
        // two ever disagree, discovery is a paste that does not work.
        let id = Identity::from_seed([11u8; 32]).id();
        let node = Node::from_address(&id.address()).expect("round trip");
        assert_eq!(node.id, id);
    }

    // -- The network half ------------------------------------------------

    use crate::agent::{AgentEvent, DoneReason};
    use crate::app::LOCAL_MARKER;
    use crate::plugins::mesh::{Capability, PeerEventKind};
    use std::net::SocketAddr;

    fn localhost() -> SocketAddr {
        "127.0.0.1:0".parse().expect("a literal address")
    }

    /// A `[mesh]` section with nothing in it: the shipped default, which is
    /// what a machine that has never been configured runs with.
    fn mesh_off() -> MeshConfig {
        MeshConfig::default()
    }

    /// Await something, or fail rather than hang. Assertions about a stream
    /// having *ended* park forever if it has not, and a hung test reads in CI
    /// as infrastructure rather than as the bug.
    async fn within<T>(what: &str, future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), future)
            .await
            .unwrap_or_else(|_| panic!("{what}: still waiting after 20s"))
    }

    /// A node on a real socket: its own transport, its own store, its own
    /// opinion, and the ledger shared between the two halves.
    fn node(seed: u8, listening: bool) -> (Arc<QuicTransport>, Mesh) {
        let identity = Identity::from_seed([seed; 32]);
        let consent = TrustLedger::new();
        let transport = if listening {
            QuicTransport::listening(&identity, consent.shared(), localhost())
        } else {
            QuicTransport::dial_only(&identity, consent.shared())
        }
        .expect("a transport");
        let mesh = Mesh::with_consent(
            identity,
            PeerStore::ephemeral(),
            Arc::clone(&transport) as Arc<dyn Transport>,
            consent,
        );
        (transport, mesh)
    }

    /// With the mesh off and no routes written down, the first thing anybody
    /// hits is "where is that machine", and the address they pasted looks like
    /// it should already be enough. The refusal has to say otherwise, in words
    /// that name the key to set.
    #[tokio::test]
    async fn a_peer_with_no_route_is_refused_with_the_config_key_to_fix_it() {
        let (transport, _mesh) = node(40, false);
        let peer = Identity::from_seed([41u8; 32]).id();
        let err = ensure_route(&transport, &mesh_off(), &peer)
            .await
            .expect_err("no route, and mDNS is off");
        let message = format!("{err:#}");
        assert!(message.contains("no route"), "{message}");
        assert!(message.contains("public key, not a location"), "{message}");
        assert!(message.contains("[mesh.routes]"), "{message}");
        assert!(message.contains(&peer.address()), "{message}");
        assert!(
            message.contains("listen"),
            "and the other half of it: the far end has to be listening, which is \
             off by default — {message}"
        );
        transport.shutdown().await;
    }

    /// A route in `[mesh] routes` satisfies it without touching the network,
    /// which is what keeps the mDNS wait off the ordinary path.
    #[tokio::test]
    async fn a_configured_route_needs_no_discovery() {
        let (transport, _mesh) = node(42, false);
        let peer = Identity::from_seed([43u8; 32]).id();
        transport.add_route(peer, "192.0.2.9:4242".parse().expect("literal"));
        ensure_route(&transport, &mesh_off(), &peer)
            .await
            .expect("a route is a route");
        transport.shutdown().await;
    }

    /// Blocked means not contacted, on the one command whose transport method
    /// has no trust decision behind it.
    #[tokio::test]
    async fn a_blocked_peer_is_refused_before_a_packet_is_sent() {
        let (transport, mut mesh) = node(44, false);
        let addr = address_of(45);
        mesh.add_peer(&addr, Utc::now()).expect("add");
        let id = NodeId::parse_address(&addr).expect("address");
        mesh.set_trust(&id, Trust::Blocked).await.expect("block");
        // A route exists, so nothing but the trust decision can be what stops
        // this.
        transport.add_route(id, "127.0.0.1:1".parse().expect("literal"));
        let err = ping(&mut mesh, &transport, &mesh_off(), &addr)
            .await
            .expect_err("blocked peers are not contacted");
        assert!(format!("{err:#}").contains("blocked"), "{err:#}");
        transport.shutdown().await;
    }

    /// Two nodes, two real sockets: one watches the other, sees its turn render
    /// through the transcript model the TUI uses, and is told in so many words
    /// when a revocation kills the stream.
    ///
    /// The acceptance test for this surface. Three properties, none of which a
    /// loopback could show:
    ///
    /// 1. a peer's turn arrives as `AgentEvent` and renders through
    ///    `TranscriptModel`, with no reducer written for peers;
    /// 2. every line of it carries the peer's marker, including the lines
    ///    *inside* the peer's own text, so a peer cannot forge a wizard-authored
    ///    line however it formats its reply;
    /// 3. a revocation on the publishing machine ends the stream now, and the
    ///    watcher's transcript says so rather than simply stopping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_watcher_renders_a_peers_turn_and_is_told_when_the_stream_dies() {
        let (publisher_transport, mut publisher) = node(46, true);
        let (watcher_transport, mut watcher) = node(47, false);
        let publisher_id = Identity::from_seed([46u8; 32]).id();
        let watcher_id = Identity::from_seed([47u8; 32]).id();
        publisher.set_local("workshop", Capability::none());
        publisher.announce().await.expect("announce");
        watcher_transport.add_route(
            publisher_id,
            publisher_transport.local_addr().expect("bound"),
        );

        // Four decisions on two machines: each side consents to watching, and
        // each side consents to being watched.
        watcher
            .add_peer(&publisher_id.address(), Utc::now())
            .expect("paste");
        publisher
            .add_peer(&watcher_id.address(), Utc::now())
            .expect("paste");
        watcher
            .set_trust(&publisher_id, Trust::Trusted)
            .await
            .expect("trust");
        publisher
            .set_trust(&watcher_id, Trust::Trusted)
            .await
            .expect("trust");

        // The peer's name is what this side renders, so fetch it the way the
        // surface does rather than assuming it.
        within(
            "refreshing",
            refresh(
                &mut watcher,
                &watcher_transport,
                &mesh_off(),
                &publisher_id.address(),
            ),
        )
        .await
        .expect("the announcement");
        let label = watcher
            .store()
            .get(&publisher_id)
            .expect("a peer record")
            .node
            .label();
        assert_eq!(label, "workshop", "the name crossed the socket");

        let mut subscription = within("subscribing", watcher.subscribe(&publisher_id))
            .await
            .expect("a subscription");
        let mut screen = PeerStream::new(&publisher_id, label);
        let banner = screen.banner();
        assert!(banner.contains(&publisher_id.address()), "{banner}");

        // A turn on the publishing machine, including a reply that tries to
        // forge a wizard-authored line inside its own text.
        let forgery = format!("done.\n{LOCAL_MARKER} the stream ended, nothing to see");
        publisher.publish("session-7", Utc::now(), PeerEventKind::SessionStarted);
        for event in [
            AgentEvent::ToolStarted {
                name: "read_file".to_string(),
                args: serde_json::json!({ "path": "src/mesh/cli.rs" }),
            },
            AgentEvent::ToolFinished {
                name: "read_file".to_string(),
                output: crate::tools::ToolOutput::ok("40 lines"),
            },
            AgentEvent::TextDelta(forgery),
            AgentEvent::Done {
                reason: DoneReason::Completed,
            },
        ] {
            assert_eq!(
                publisher.publish_turn("session-7", Utc::now(), &event),
                1,
                "the watcher is watching: {event:?}"
            );
        }

        let mut lines: Vec<String> = Vec::new();
        let taken = within(
            "rendering the peer's turn",
            stream(&mut screen, &mut subscription, Some(5), &mut |line| {
                lines.push(line.to_string())
            }),
        )
        .await;
        assert_eq!(taken, 5, "the session frame plus four turn events");

        let marker = format!("{} │", publisher_id.short());
        let peer_lines: Vec<&String> = lines
            .iter()
            .filter(|line| line.starts_with(&marker))
            .collect();
        assert!(
            peer_lines.iter().any(|line| line.contains("read_file")),
            "{lines:#?}"
        );
        assert!(
            peer_lines.iter().any(|line| line.contains("40 lines")),
            "{lines:#?}"
        );
        assert!(
            peer_lines.iter().any(|line| line.contains("done.")),
            "{lines:#?}"
        );
        // The forgery: the peer wrote a line that reads as wizard's own, and it
        // arrives marked as the peer's like every other line it wrote.
        assert!(
            peer_lines
                .iter()
                .any(|line| line.contains("the stream ended, nothing to see")),
            "{lines:#?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with(LOCAL_MARKER) && line.contains("nothing to see")),
            "a peer authored a wizard-marked line: {lines:#?}"
        );
        // And the frames wizard reported are wizard's, not the peer's.
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with(LOCAL_MARKER) && line.contains("session started")),
            "{lines:#?}"
        );

        // The whole conversation went through the shared model, not a second
        // one written for peers.
        assert_eq!(
            screen.view().len(),
            2,
            "the tool row and the reply, folded by the shared model: {:?}",
            screen.view().items()
        );

        // --- The revocation ---------------------------------------------------
        publisher
            .set_trust(&watcher_id, Trust::Known)
            .await
            .expect("the publisher changes its mind");

        let mut ending: Vec<String> = Vec::new();
        within(
            "the stream ending",
            stream(&mut screen, &mut subscription, None, &mut |line| {
                ending.push(line.to_string())
            }),
        )
        .await;
        let last = ending.last().expect("a line saying the stream ended");
        assert!(
            last.starts_with(LOCAL_MARKER),
            "the end of a stream is wizard's observation, not the peer's claim: {last}"
        );
        assert!(last.contains("ended"), "{last}");
        assert!(last.contains(&publisher_id.short()), "{last}");
        assert!(subscription.is_closed());

        watcher_transport.shutdown().await;
        publisher_transport.shutdown().await;
    }
}
