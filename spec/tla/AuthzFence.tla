------------------------------- MODULE AuthzFence -------------------------------
(* The authorization fence of 013 FR-012: a change of users or roles that takes a privilege     *)
(* away (dropUser, revokeRolesFromUser, dropRole, updateRole, ...) answers only once no node   *)
(* can still authorize with the state before it. Group 0 commits the changes of users and roles *)
(* at increasing indices of its log; some take privileges away (revocations), the others add   *)
(* them. Each node authorizes every command with its own copy of Group 0, applied at its own    *)
(* pace, and only while it holds an authorization lease. Group 0's heartbeat to a node carries  *)
(* its commit index and the time it left; the node renews its lease up to that time plus Lease  *)
(* once it has applied that index: a heartbeat that comes before waits until the node applies   *)
(* it, and a crash forgets it. A node acknowledges what it has applied.                         *)
(* Group 0 answers a revocation at index i once every node has acknowledged i or the last lease *)
(* it granted to that node has run out. Nodes may crash (they stop applying, acknowledging and  *)
(* receiving heartbeats, and keep the copy they had applied) and come back. Time is one clock:  *)
(* the margin for clock skew of D-004 is taken off Lease in the code and is not modelled here.  *)
(* Invariants: after a revocation answers, a node that authorizes has applied it; Lease after a *)
(* revocation's commit, likewise. Property: every revocation is answered; additions answer     *)
(* without waiting.                                                                             *)
EXTENDS Naturals, FiniteSets

CONSTANTS Node, Lease, MaxTime, MaxChanges, MaxCrashes

VARIABLES
    now,        \* the clock
    commit,     \* Group 0's commit index: the number of changes committed
    revoking,   \* the indices of the committed changes that take privileges away
    committedAt,\* per index, the time of its commit
    answered,   \* the indices whose command has answered
    applied,    \* per node, the index of its copy of Group 0
    acked,      \* per node, the index Group 0 knows it has applied
    lease,      \* per node, until when it may authorize
    granted,    \* per node, the latest lease end Group 0 has sent it
    inflight,   \* per node, the heartbeat in flight to it ([commit, sent]) or NoMsg
    waiting,    \* per node, the lease a heartbeat ahead of its copy gives once applied, or NoWait
    up,         \* nodes running
    crashes

vars == <<now, commit, revoking, committedAt, answered, applied, acked, lease, granted,
          inflight, waiting, up, crashes>>

Index == 1..MaxChanges

NoMsg == [commit |-> 0, sent |-> 0, none |-> TRUE]
NoWait == [commit |-> 0, end |-> 0]

Init ==
    /\ now = 0 /\ commit = 0 /\ revoking = {} /\ answered = {}
    /\ committedAt = [i \in Index |-> 0]
    /\ applied = [n \in Node |-> 0] /\ acked = [n \in Node |-> 0]
    /\ lease = [n \in Node |-> Lease] /\ granted = [n \in Node |-> Lease]
    /\ inflight = [n \in Node |-> NoMsg] /\ waiting = [n \in Node |-> NoWait]
    /\ up = Node /\ crashes = 0

\* Node n authorizes a command, with its copy of Group 0 at applied[n].
CanAuth(n) == n \in up /\ now < lease[n]

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<waiting, commit, revoking, committedAt, answered, applied, acked, lease, granted,
                   inflight, up, crashes>>

\* A change of users or roles commits: one that takes privileges away, or one that adds them.
Change(takesAway) ==
    /\ commit < MaxChanges
    /\ commit' = commit + 1
    /\ committedAt' = [committedAt EXCEPT ![commit + 1] = now]
    /\ revoking' = IF takesAway THEN revoking \cup {commit + 1} ELSE revoking
    /\ UNCHANGED <<waiting, now, answered, applied, acked, lease, granted, inflight, up, crashes>>

\* Leases never end past MaxTime, so that the bound on time does not hide an answer.
Heartbeat(n) ==
    /\ now + Lease <= MaxTime /\ inflight[n] = NoMsg
    /\ inflight' = [inflight EXCEPT ![n] = [commit |-> commit, sent |-> now, none |-> FALSE]]
    /\ granted' = [granted EXCEPT ![n] = IF now + Lease > @ THEN now + Lease ELSE @]
    /\ UNCHANGED <<waiting, now, commit, revoking, committedAt, answered, applied, acked, lease, up,
                   crashes>>

\* The heartbeat in flight reaches n: it renews the lease if n has applied its commit index, waits
\* for n to apply it if n is behind, and is lost when n is down.
Receive(n) ==
    LET m == inflight[n] IN
    /\ m /= NoMsg
    /\ inflight' = [inflight EXCEPT ![n] = NoMsg]
    /\ IF n \in up /\ applied[n] >= m.commit
       THEN /\ lease' = [lease EXCEPT ![n] = IF m.sent + Lease > @ THEN m.sent + Lease ELSE @]
            /\ UNCHANGED waiting
       ELSE /\ waiting' = IF n \in up /\ m.sent + Lease > waiting[n].end
                          THEN [waiting EXCEPT ![n] = [commit |-> m.commit, end |-> m.sent + Lease]]
                          ELSE waiting
            /\ UNCHANGED lease
    /\ UNCHANGED <<now, commit, revoking, committedAt, answered, applied, acked, granted, up,
                   crashes>>

\* A node applies the next committed change.
Apply(n) ==
    /\ n \in up /\ applied[n] < commit
    /\ applied' = [applied EXCEPT ![n] = @ + 1]
    /\ IF waiting[n] /= NoWait /\ applied[n] + 1 >= waiting[n].commit
       THEN /\ lease' = [lease EXCEPT ![n] = IF waiting[n].end > @ THEN waiting[n].end ELSE @]
            /\ waiting' = [waiting EXCEPT ![n] = NoWait]
       ELSE UNCHANGED <<lease, waiting>>
    /\ UNCHANGED <<now, commit, revoking, committedAt, answered, acked, granted,
                   inflight, up, crashes>>

Ack(n) ==
    /\ n \in up /\ acked[n] < applied[n]
    /\ acked' = [acked EXCEPT ![n] = applied[n]]
    /\ UNCHANGED <<waiting, now, commit, revoking, committedAt, answered, applied, lease, granted,
                   inflight, up, crashes>>

\* A change that adds privileges answers at once; a revocation waits for the fence.
Answer(i) ==
    /\ i <= commit /\ i \notin answered
    /\ i \in revoking => \A n \in Node : acked[n] >= i \/ granted[n] <= now
    /\ answered' = answered \cup {i}
    /\ UNCHANGED <<waiting, now, commit, revoking, committedAt, applied, acked, lease, granted,
                   inflight, up, crashes>>

Crash(n) ==
    /\ n \in up /\ crashes < MaxCrashes
    /\ up' = up \ {n} /\ crashes' = crashes + 1
    /\ waiting' = [waiting EXCEPT ![n] = NoWait]
    /\ UNCHANGED <<now, commit, revoking, committedAt, answered, applied, acked, lease, granted,
                   inflight>>

Restart(n) ==
    /\ n \notin up
    /\ up' = up \cup {n}
    /\ UNCHANGED <<waiting, now, commit, revoking, committedAt, answered, applied, acked, lease, granted,
                   inflight, crashes>>

Next ==
    \/ Tick \/ Change(TRUE) \/ Change(FALSE)
    \/ \E i \in Index : Answer(i)
    \/ \E n \in Node : Heartbeat(n) \/ Receive(n) \/ Apply(n) \/ Ack(n) \/ Crash(n) \/ Restart(n)

Spec == Init /\ [][Next]_vars /\ WF_vars(Tick) /\ \A i \in Index : WF_vars(Answer(i))

TypeOK ==
    /\ commit \in 0..MaxChanges /\ revoking \subseteq 1..commit /\ answered \subseteq 1..commit
    /\ \A n \in Node : applied[n] <= commit /\ acked[n] <= applied[n]

\* Once a revocation has answered, every node that authorizes has applied it.
FencedAfterAnswer ==
    \A i \in answered \cap revoking : \A n \in Node : CanAuth(n) => applied[n] >= i

\* Lease after a revocation commits, every node that authorizes has applied it.
FencedWithinLease ==
    \A i \in revoking : now >= committedAt[i] + Lease =>
        \A n \in Node : CanAuth(n) => applied[n] >= i

RevocationsAnswered == \A i \in Index : (i \in revoking) ~> (i \in answered)
=============================================================================
