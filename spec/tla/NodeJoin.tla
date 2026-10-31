------------------------------ MODULE NodeJoin ------------------------------
(* Joining a cluster (001 FR-015). Group 0's leader turns a                                     *)
(* JoinRequest into AddNode, idempotent by certificate fingerprint, and answers with the        *)
(* current voters; the answer can be lost (the node crashes before writing node.meta) and the   *)
(* node asks again. Until it has applied Group 0, a new node takes Group 0 traffic only from     *)
(* the voters of its answer; a removed node, whose certificate is still valid, tries to install *)
(* a fake Group 0 on it. A node becomes a voter only once its NodeBoot is applied.               *)
(* Invariants: one NodeId per fingerprint; a new node only applies a Group 0 sent by a voter;   *)
(* every voter has booted.                                                                       *)
EXTENDS Naturals, FiniteSets

CONSTANTS Joiners, InitVoters, Removed, MaxId

VARIABLES
    nodes,      \* Group 0: the nodes added for the joiners, as <<fingerprint, NodeId>>
    nextId,
    voters,     \* Group 0: fingerprints of the voters
    booted,     \* Group 0: fingerprints whose NodeBoot was applied
    meta,       \* each joiner's node.meta: NodeId (0 = not written)
    told,       \* the voters each joiner received in its JoinResponse
    applied,    \* the joiner applied a Group 0
    source      \* who sent the Group 0 it applied

vars == <<nodes, nextId, voters, booted, meta, told, applied, source>>

Fingerprints == Joiners \cup InitVoters \cup Removed

Init ==
    /\ nodes = {}
    /\ nextId = 2
    /\ voters = InitVoters
    /\ booted = InitVoters
    /\ meta = [j \in Joiners |-> 0]
    /\ told = [j \in Joiners |-> {}]
    /\ applied = [j \in Joiners |-> FALSE]
    /\ source = [j \in Joiners |-> "none"]

\* The leader handles a JoinRequest; `lost` = the answer never arrives (or the node crashes before
\* writing node.meta), so the node asks again later.
Known(j) == {p \in nodes : p[1] = j}

Join(j, lost) ==
    /\ meta[j] = 0
    \* AddNode is idempotent by fingerprint: an existing node answers with its NodeId.
    /\ LET id == IF Known(j) /= {} THEN (CHOOSE p \in Known(j) : TRUE)[2] ELSE nextId
       IN /\ id <= MaxId
          /\ nodes' = nodes \cup {<<j, id>>}
          /\ nextId' = IF Known(j) /= {} THEN nextId ELSE nextId + 1
          /\ IF lost
             THEN UNCHANGED <<meta, told>>
             ELSE /\ meta' = [meta EXCEPT ![j] = id]
                  /\ told' = [told EXCEPT ![j] = voters]
    /\ UNCHANGED <<voters, booted, applied, source>>

\* Someone sends Group 0 to a joiner that has not applied it yet.
SendGroup0(s, j) ==
    /\ meta[j] /= 0
    /\ ~applied[j]
    /\ s \in told[j]
    /\ applied' = [applied EXCEPT ![j] = TRUE]
    /\ source' = [source EXCEPT ![j] = s]
    /\ UNCHANGED <<nodes, nextId, voters, booted, meta, told>>

\* A removed node tries too: the joiner drops what does not come from its voters.
FakeGroup0(r, j) ==
    /\ meta[j] /= 0
    /\ ~applied[j]
    /\ r \notin told[j]
    /\ UNCHANGED vars

Boot(j) ==
    /\ applied[j]
    /\ booted' = booted \cup {j}
    /\ UNCHANGED <<nodes, nextId, voters, meta, told, applied, source>>

\* The balancer promotes only nodes that booted; a removed voter leaves.
Promote(j) ==
    /\ j \in booted
    /\ voters' = voters \cup {j}
    /\ UNCHANGED <<nodes, nextId, booted, meta, told, applied, source>>

Demote(v) ==
    /\ v \in voters
    /\ Cardinality(voters) > 1
    /\ voters' = voters \ {v}
    /\ UNCHANGED <<nodes, nextId, booted, meta, told, applied, source>>

Next ==
    \/ \E j \in Joiners, lost \in BOOLEAN : Join(j, lost)
    \/ \E s \in Fingerprints, j \in Joiners : SendGroup0(s, j) \/ FakeGroup0(s, j)
    \/ \E j \in Joiners : Boot(j) \/ Promote(j)
    \/ \E v \in Fingerprints : Demote(v)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ nodes \subseteq Joiners \X (2..MaxId)
    /\ voters \subseteq Fingerprints
    /\ meta \in [Joiners -> 0..MaxId]

\* A fingerprint gets at most one NodeId, and the node keeps the one it was told.
OneIdPerFingerprint ==
    /\ \A p, q \in nodes : p[1] = q[1] <=> p[2] = q[2]
    /\ \A j \in Joiners : meta[j] /= 0 => <<j, meta[j]>> \in nodes

\* A retired node (never a voter again) cannot install Group 0 on a new node.
AppliedFromVoter ==
    \A j \in Joiners : applied[j] => source[j] \in told[j] /\ source[j] \notin Removed

VotersBooted == voters \subseteq booted
=============================================================================
