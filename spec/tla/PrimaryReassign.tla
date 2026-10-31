----------------------------- MODULE PrimaryReassign -----------------------------
(* The designated primary of the replica set presentation (018 FR-007, FR-015).                *)
(* Group 0 keeps the presentation: the designated primary, its electionId and setVersion, and  *)
(* which nodes are alive (from their heartbeats). Every change is an entry of Group 0's log.   *)
(* When the designated node stops being alive, Group 0 designates another live node with a new *)
(* electionId and setVersion (Reassign); an operator may also move it (replSetStepDown or       *)
(* setClusterParameter, Designate). Each node applies the entries in log order at its own pace *)
(* and, on every entry it applies, raises its topologyVersion counter so that the awaitable    *)
(* hellos of the drivers' monitors waiting on it answer. A dead node applies nothing until it   *)
(* comes back. A monitor remembers the last hello it got from a node and waits with its        *)
(* counter.                                                                                     *)
(* Invariants: two nodes never answer as primary with the same electionId; Group 0 only        *)
(* designates live nodes; a monitor whose last hello no longer matches the node's presentation *)
(* is woken. Property: a dead designated primary is replaced while some node is alive.         *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Node, MaxChanges, MaxCrashes

\* MaxChanges bounds the operator's moves; reassignments are bounded by the crashes.

VARIABLES
    glog,       \* Group 0's log of presentations: [primary, eid] (eid: electionId and setVersion)
    alive,      \* Group 0's live nodes
    crashes,    \* crashes so far
    up,         \* nodes running
    applied,    \* entries of glog each node has applied
    tv,         \* each node's topologyVersion counter
    deadPick,   \* Group 0 designated a node that was not alive
    mon         \* per node, the monitor's last hello: [eid, counter] or [eid |-> 0] before any

vars == <<glog, alive, crashes, up, applied, tv, deadPick, mon>>

Last == glog[Len(glog)]

\* The presentation node n answers with: the last entry it applied.
View(n) == glog[applied[n]]

\* Node n answers hello as the primary.
IsPrimary(n) == View(n).primary = n

Init ==
    /\ \E p \in Node : glog = <<[primary |-> p, eid |-> 1]>>
    /\ alive = Node /\ crashes = 0 /\ up = Node
    /\ applied = [n \in Node |-> 1]
    /\ tv = [n \in Node |-> 0]
    /\ deadPick = FALSE
    /\ mon = [n \in Node |-> [eid |-> 1, counter |-> 0]]

\* Group 0 appends a new presentation designating p.
Designate(p) ==
    /\ p /= Last.primary
    /\ glog' = Append(glog, [primary |-> p, eid |-> Last.eid + 1])
    /\ deadPick' = (deadPick \/ p \notin alive)

\* Group 0 sees the designated node dead and designates a live one.
Reassign ==
    /\ Last.primary \notin alive
    /\ \E p \in alive : Designate(p)
    /\ UNCHANGED <<alive, crashes, up, applied, tv, mon>>

\* replSetStepDown or setClusterParameter moves the primary to a live node.
Operator ==
    /\ Len(glog) < MaxChanges
    /\ \E p \in alive : Designate(p)
    /\ UNCHANGED <<alive, crashes, up, applied, tv, mon>>

Crash(n) ==
    /\ n \in up /\ crashes < MaxCrashes
    /\ up' = up \ {n} /\ crashes' = crashes + 1
    /\ UNCHANGED <<glog, alive, applied, tv, deadPick, mon>>

Recover(n) ==
    /\ n \notin up
    /\ up' = up \cup {n}
    /\ UNCHANGED <<glog, alive, crashes, applied, tv, deadPick, mon>>

\* Group 0's view of liveness follows the heartbeats, with a delay.
Heartbeat ==
    /\ alive /= up
    /\ alive' = up
    /\ UNCHANGED <<glog, crashes, up, applied, tv, deadPick, mon>>

\* A running node applies the next entry of Group 0's log and answers its waiting hellos.
Apply(n) ==
    /\ n \in up /\ applied[n] < Len(glog)
    /\ applied' = [applied EXCEPT ![n] = @ + 1]
    /\ tv' = [tv EXCEPT ![n] = @ + 1]
    /\ UNCHANGED <<glog, alive, crashes, up, deadPick, mon>>

\* A monitor's awaitable hello to node n returns: its counter moved or the wait timed out.
Hello(n) ==
    /\ n \in up
    /\ mon' = [mon EXCEPT ![n] = [eid |-> View(n).eid, counter |-> tv[n]]]
    /\ mon' /= mon
    /\ UNCHANGED <<glog, alive, crashes, up, applied, tv, deadPick>>

Next ==
    \/ Reassign \/ Operator \/ Heartbeat
    \/ \E n \in Node : Crash(n) \/ Recover(n) \/ Apply(n) \/ Hello(n)

Spec == Init /\ [][Next]_vars /\ WF_vars(Reassign) /\ WF_vars(Heartbeat)
            /\ \A n \in Node : WF_vars(Apply(n))

\* Drivers tell primaries apart by electionId: two nodes never claim it with the same one.
OnePrimaryPerElection ==
    \A n, m \in Node : n /= m /\ IsPrimary(n) /\ IsPrimary(m) => View(n).eid /= View(m).eid

LiveDesignation == ~deadPick

\* A monitor whose last hello is stale waits on a counter the node has already passed.
StaleMonitorWakes ==
    \A n \in Node : mon[n].eid /= View(n).eid => tv[n] > mon[n].counter

\* While some node is alive, a dead designated primary is replaced (or comes back).
DeadPrimaryReplaced ==
    (Last.primary \notin alive /\ alive /= {}) ~> (Last.primary \in alive \/ alive = {})
=============================================================================
