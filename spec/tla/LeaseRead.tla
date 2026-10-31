------------------------------ MODULE LeaseRead ------------------------------
(* Lease reads of teseladb-raft (001 FR-015).                                                  *)
(* Time is global and discrete, kept as countdowns (what is left of each lease and guard) so that *)
(* states that only differ by when they happen are one. Every node measures durations with its  *)
(* own clock, whose rate                                                                         *)
(* differs by at most δ = 0.10: a duration of d local ticks lasts between d/(1+δ) and d/(1-δ)    *)
(* global ticks, and the model takes the worst case of each rule. A heartbeat round answered by *)
(* a quorum gives the leader a lease of 11 local ticks from its send (at most LeaseMax global   *)
(* ticks); a node that heard the leader does not vote for `election_ticks_min` = 15 local ticks *)
(* (at least GuardMin global ticks) unless the request comes from the leader's own transfer    *)
(* (`TimeoutNow`), and a node that starts votes for nobody during `election_ticks_max` = 30     *)
(* local ticks (at least BootMin). Sending `TimeoutNow` ends the leader's lease reads for the    *)
(* rest of its term, even if the transfer is abandoned and its vote requests arrive late. A new *)
(* leader reads only once its term's Noop is committed; until then it may know an old value.   *)
(* Invariant: a lease read never returns a value older than an acknowledged write.             *)
EXTENDS Naturals, FiniteSets

CONSTANTS Node, LeaseMax, GuardMin, BootMin, MaxTerm, MaxValue

VARIABLES
    term,       \* each node's term
    leader,     \* each node believes it leads its term
    lease,      \* global ticks left of a leader's lease
    noLease,    \* it sent TimeoutNow in its term
    heard,      \* global ticks left of the vote guard (it heard the leader)
    boot,       \* global ticks left of the boot guard
    known,      \* value a leader has applied
    ready,      \* its term's Noop is committed
    transfer,   \* a TimeoutNow in flight: <<from, to, term>> or <<>>
    latest      \* last acknowledged write

vars == <<term, leader, lease, noLease, heard, boot, known, ready, transfer, latest>>

Majority(Q) == 2 * Cardinality(Q) > Cardinality(Node)

Max(S) == CHOOSE x \in S : \A y \in S : y <= x

First == CHOOSE n \in Node : TRUE

Init ==
    /\ term = [n \in Node |-> 1]
    /\ leader = [n \in Node |-> n = First]
    /\ lease = [n \in Node |-> 0]
    /\ noLease = [n \in Node |-> FALSE]
    /\ heard = [n \in Node |-> 0]
    /\ boot = [n \in Node |-> 0]
    /\ known = [n \in Node |-> 0]
    /\ ready = [n \in Node |-> n = First]
    /\ transfer = <<>>
    /\ latest = 0

Dec(x) == IF x > 0 THEN x - 1 ELSE 0

Tick ==
    /\ lease' = [n \in Node |-> Dec(lease[n])]
    /\ heard' = [n \in Node |-> Dec(heard[n])]
    /\ boot' = [n \in Node |-> Dec(boot[n])]
    /\ UNCHANGED <<term, leader, noLease, known, ready, transfer, latest>>

\* Nodes of quorum q are still in l's term: they answer it.
InTerm(l, q) == \A n \in q : term[n] = term[l]

\* A heartbeat round answered by quorum q: lease from now, and q hears the leader.
Heartbeat(l, q) ==
    /\ leader[l] /\ l \in q /\ Majority(q) /\ InTerm(l, q)
    /\ lease' = [lease EXCEPT ![l] = LeaseMax]
    /\ heard' = [n \in Node |-> IF n \in q /\ n /= l THEN GuardMin ELSE heard[n]]
    /\ UNCHANGED <<term, leader, noLease, boot, known, ready, transfer, latest>>

\* The new leader commits its Noop: it learns every committed value (Leader Completeness).
CommitNoop(l, q) ==
    /\ leader[l] /\ ~ready[l] /\ l \in q /\ Majority(q) /\ InTerm(l, q)
    /\ ready' = [ready EXCEPT ![l] = TRUE]
    /\ known' = [known EXCEPT ![l] = latest]
    /\ UNCHANGED <<term, leader, lease, noLease, heard, boot, transfer, latest>>

\* An acknowledged write: committed by a quorum still in the leader's term.
Write(l, q) ==
    /\ leader[l] /\ ready[l] /\ l \in q /\ Majority(q) /\ InTerm(l, q)
    /\ latest < MaxValue
    /\ latest' = latest + 1
    /\ known' = [known EXCEPT ![l] = latest + 1]
    /\ UNCHANGED <<term, leader, lease, noLease, heard, boot, ready, transfer>>

\* Leadership transfer: the leader stops lease reads for the rest of its term; the target's vote
\* requests may arrive at any later time.
SendTimeoutNow(l, t) ==
    /\ leader[l] /\ t /= l /\ transfer = <<>>
    /\ noLease' = [noLease EXCEPT ![l] = TRUE]
    /\ transfer' = <<l, t, term[l]>>
    /\ UNCHANGED <<term, leader, lease, heard, boot, known, ready, latest>>

\* Node n votes in an election of term t: not right after starting, and not while it hears the
\* leader unless the request comes from a transfer started in n's current term `xfer` (the
\* request carries the transferring leader's term).
MayVote(n, t, xfer) ==
    /\ term[n] < t
    /\ boot[n] = 0
    /\ heard[n] = 0 \/ term[n] = xfer

\* Candidate c wins the election of term t with quorum q (votes and term change at once).
\* `base` is the term the candidate campaigns from (its own, or the TimeoutNow's it adopts).
Elect(c, q, xfer, base) ==
    LET t == base + 1 IN
    /\ t <= MaxTerm
    /\ c \in q /\ Majority(q)
    /\ boot[c] = 0
    /\ \A n \in q \ {c} : MayVote(n, t, xfer)
    /\ term' = [n \in Node |-> IF n \in q THEN t ELSE term[n]]
    /\ leader' = [n \in Node |-> IF n = c THEN TRUE ELSE IF n \in q THEN FALSE ELSE leader[n]]
    /\ lease' = [lease EXCEPT ![c] = 0]
    /\ noLease' = [noLease EXCEPT ![c] = FALSE]
    /\ ready' = [ready EXCEPT ![c] = FALSE]
    \* Until its Noop is committed it may only know an older value.
    /\ \E v \in 0..latest : known' = [known EXCEPT ![c] = v]
    /\ UNCHANGED <<heard, boot, latest>>

\* A node campaigns when its election timer fires: at least `election_ticks_min` after it last
\* heard the leader (the same guard as its vote for itself).
Campaign(c, q) ==
    /\ ~leader[c] /\ heard[c] = 0 /\ Elect(c, q, 0, term[c]) /\ UNCHANGED transfer

\* The transfer's target campaigns when the TimeoutNow arrives, maybe long after it was sent; a
\* TimeoutNow of an older term than the target's is dropped (DropTransfer).
TransferCampaign(q) ==
    /\ transfer /= <<>>
    /\ ~leader[transfer[2]]
    /\ term[transfer[2]] <= transfer[3]
    /\ Elect(transfer[2], q, transfer[3], transfer[3])
    /\ transfer' = <<>>

DropTransfer ==
    /\ transfer /= <<>>
    /\ transfer' = <<>>
    /\ UNCHANGED <<term, leader, lease, noLease, heard, boot, known, ready, latest>>

\* A node that hears a higher term steps down.
StepDown(n) ==
    /\ leader[n]
    /\ \E m \in Node : term[m] > term[n]
    /\ leader' = [leader EXCEPT ![n] = FALSE]
    /\ term' = [term EXCEPT ![n] = Max({term[m] : m \in Node})]
    /\ UNCHANGED <<lease, noLease, heard, boot, known, ready, transfer, latest>>

\* A restart: no leadership, no memory of the last heartbeat, and the boot guard.
Restart(n) ==
    /\ leader' = [leader EXCEPT ![n] = FALSE]
    /\ heard' = [heard EXCEPT ![n] = 0]
    /\ boot' = [boot EXCEPT ![n] = BootMin]
    /\ UNCHANGED <<term, lease, noLease, known, ready, transfer, latest>>

Next ==
    \/ Tick
    \/ DropTransfer
    \/ \E n \in Node : StepDown(n) \/ Restart(n)
    \/ \E l \in Node, q \in SUBSET Node :
        Heartbeat(l, q) \/ CommitNoop(l, q) \/ Write(l, q) \/ Campaign(l, q)
    \/ \E q \in SUBSET Node : TransferCampaign(q)
    \/ \E l, t \in Node : SendTimeoutNow(l, t)

Spec == Init /\ [][Next]_vars

\* A leader that may serve a lease read now knows the last acknowledged write.
CanLeaseRead(l) == leader[l] /\ ready[l] /\ ~noLease[l] /\ lease[l] > 0

LeaseReadsAreFresh == \A l \in Node : CanLeaseRead(l) => known[l] = latest

TypeOK ==
    /\ lease \in [Node -> 0..LeaseMax]
    /\ term \in [Node -> 1..MaxTerm]
    /\ latest \in 0..MaxValue
=============================================================================
