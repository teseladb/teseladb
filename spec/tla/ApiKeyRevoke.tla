------------------------------ MODULE ApiKeyRevoke ------------------------------
(* Revoking an API key in every node (019 FR-016, SC-005), with the fence of 013 FR-012.         *)
(* Group 0 commits the revocation at index R of its log. Each node applies the log at its own    *)
(* pace and only authorizes while it holds a lease. Group 0's heartbeat to a node carries the    *)
(* commit index and the time it left; the node renews its lease up to that time plus Lease, and *)
(* only if it has already applied that index. A node acknowledges what it has applied. Group 0   *)
(* answers the revocation once every node has acknowledged R or the last lease it granted to it  *)
(* has run out. Nodes may crash (they stop applying, acknowledging and receiving heartbeats) and *)
(* come back. Time is one clock: the margin for clock skew of D-004 is taken off Lease in the     *)
(* code and is not modelled here.                                                                *)
(* Invariants: after the answer no node authorizes with the key; Lease after the revocation no  *)
(* node authorizes with it (SC-005, with Lease of at most one second). Property: every           *)
(* revocation is answered.                                                                       *)
EXTENDS Naturals, FiniteSets

CONSTANTS Node, Lease, MaxTime, MaxCrashes

VARIABLES
    now,        \* the clock
    commit,     \* Group 0's commit index: 0, or R = 1 once the revocation is committed
    revokedAt,  \* time of the revocation's commit
    answered,   \* the revocation command answered
    applied,    \* per node, the index it has applied
    acked,      \* per node, the index Group 0 knows it has applied
    lease,      \* per node, until when it may authorize
    granted,    \* per node, the latest lease end Group 0 has sent it
    inflight,   \* per node, the heartbeat in flight to it ([commit, sent]) or NoMsg
    up,         \* nodes running
    crashes

vars == <<now, commit, revokedAt, answered, applied, acked, lease, granted, inflight, up, crashes>>

R == 1

NoMsg == [commit |-> 0, sent |-> 0, none |-> TRUE]

Init ==
    /\ now = 0 /\ commit = 0 /\ revokedAt = 0 /\ answered = FALSE
    /\ applied = [n \in Node |-> 0] /\ acked = [n \in Node |-> 0]
    /\ lease = [n \in Node |-> Lease] /\ granted = [n \in Node |-> Lease]
    /\ inflight = [n \in Node |-> NoMsg] /\ up = Node /\ crashes = 0

\* Node n may authorize a request with the key.
CanAuth(n) == n \in up /\ now < lease[n] /\ applied[n] < R

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<commit, revokedAt, answered, applied, acked, lease, granted, inflight, up, crashes>>

Revoke ==
    /\ commit = 0
    /\ commit' = R /\ revokedAt' = now
    /\ UNCHANGED <<now, answered, applied, acked, lease, granted, inflight, up, crashes>>

\* Leases never end past MaxTime, so that the bound on time does not hide an answer.
Heartbeat(n) ==
    /\ now + Lease <= MaxTime /\ inflight[n] = NoMsg
    /\ inflight' = [inflight EXCEPT ![n] = [commit |-> commit, sent |-> now, none |-> FALSE]]
    /\ granted' = [granted EXCEPT ![n] = IF now + Lease > @ THEN now + Lease ELSE @]
    /\ UNCHANGED <<now, commit, revokedAt, answered, applied, acked, lease, up, crashes>>

\* The heartbeat in flight reaches n (or is lost when n is down or behind its commit index).
Receive(n) ==
    LET m == inflight[n] IN
    /\ m /= NoMsg
    /\ inflight' = [inflight EXCEPT ![n] = NoMsg]
    /\ IF n \in up /\ applied[n] >= m.commit
       THEN lease' = [lease EXCEPT ![n] = IF m.sent + Lease > @ THEN m.sent + Lease ELSE @]
       ELSE UNCHANGED lease
    /\ UNCHANGED <<now, commit, revokedAt, answered, applied, acked, granted, up, crashes>>

Apply(n) ==
    /\ n \in up /\ applied[n] < commit
    /\ applied' = [applied EXCEPT ![n] = commit]
    /\ UNCHANGED <<now, commit, revokedAt, answered, acked, lease, granted, inflight, up, crashes>>

Ack(n) ==
    /\ n \in up /\ acked[n] < applied[n]
    /\ acked' = [acked EXCEPT ![n] = applied[n]]
    /\ UNCHANGED <<now, commit, revokedAt, answered, applied, lease, granted, inflight, up, crashes>>

Answer ==
    /\ commit = R /\ ~answered
    /\ \A n \in Node : acked[n] >= R \/ granted[n] <= now
    /\ answered' = TRUE
    /\ UNCHANGED <<now, commit, revokedAt, applied, acked, lease, granted, inflight, up, crashes>>

Crash(n) ==
    /\ n \in up /\ crashes < MaxCrashes
    /\ up' = up \ {n} /\ crashes' = crashes + 1
    /\ UNCHANGED <<now, commit, revokedAt, answered, applied, acked, lease, granted, inflight>>

Restart(n) ==
    /\ n \notin up
    /\ up' = up \cup {n}
    /\ UNCHANGED <<now, commit, revokedAt, answered, applied, acked, lease, granted, inflight, crashes>>

Next ==
    \/ Tick \/ Revoke \/ Answer
    \/ \E n \in Node : Heartbeat(n) \/ Receive(n) \/ Apply(n) \/ Ack(n) \/ Crash(n) \/ Restart(n)

Spec == Init /\ [][Next]_vars /\ WF_vars(Tick) /\ WF_vars(Answer)

NoAuthAfterAnswer == answered => \A n \in Node : ~CanAuth(n)

RevokedWithinLease == commit = R /\ now >= revokedAt + Lease => \A n \in Node : ~CanAuth(n)

RevocationAnswered == commit = R ~> answered
=============================================================================
