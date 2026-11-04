----------------------------- MODULE AuthzFenceLeaders -----------------------------
(* The authorization fence of 013 FR-012 across changes of Group 0's leader (D-073).           *)
(* AuthzFence.tla checks the fence under one leader; here leaders change. Each node is a voter  *)
(* of Group 0 with its term. A node becomes leader of a new term with the votes of a majority, *)
(* all of which move to that term, and only if its log holds every committed change (Raft).    *)
(* A node that led keeps believing it leads until it sees a later term: a leader cut off from  *)
(* the rest goes on as leader, and the rest may meanwhile elect another one. A leader commits   *)
(* changes with a majority in its term; every change takes privileges away.                    *)
(* Nodes apply what their log holds at their own pace.                                          *)
(*                                                                                               *)
(* A node asks the leader of its term for a lease, with the index it applied. That node holds   *)
(* the request and answers it only once a majority of the voters, in its term, acknowledged a   *)
(* heartbeat round it sent no earlier than the node asked (heard[l]); with RoleOnly it          *)
(* answers on its belief alone, the defect the fence had (the faulty variant                    *)
(* authz_lease_without_quorum). The answer records the node's index and a lease end one Lease  *)
(* after it in the leader's table, and carries the leader's commit index; the node takes a lease*)
(* of one Lease from when it asked, if it has applied that index. A new leader starts its table *)
(* counting every node as holding a lease until one Lease after it took over and having        *)
(* acknowledged nothing. A leader answers a change once every node in its table acknowledged it*)
(* or its lease there ran out. Time is one clock, as in AuthzFence.tla: the margin for the      *)
(* clocks' drift is taken off the node's lease in the code and is not modelled. A heartbeat     *)
(* round is sent and acknowledged in one step: a round acknowledged later than it left can only *)
(* make heard[l] smaller, which answers fewer requests. Heartbeat rounds and requests together  *)
(* are at most MaxSteps, which bounds the state space.                                          *)
(* Invariants: after a change answered, a node that authorizes has applied it; one Lease after  *)
(* a change committed, likewise.                                                                *)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Node, None, Lease, MaxTime, MaxTerm, MaxChanges, MaxSteps, RoleOnly

VARIABLES
    now,        \* the clock
    term,       \* per node, its term of Group 0
    leaderOf,   \* per term, the node elected in it, or None
    commit,     \* changes committed
    committedAt, \* per change, when it committed
    known,      \* per node, the commit index it knows as leader
    log,        \* per node, how many committed changes its log holds
    applied,    \* per node, how many it applied
    lease,      \* per node, until when it may authorize
    heard,      \* per node, one past when a majority last acknowledged a round of its term (0: never)
    held,       \* per node, the requests it holds: [node, at, idx]
    grants,     \* the answers in flight: [node, at, commit]
    acked,      \* per leader, per node, the index its table has
    until,      \* per leader, per node, the lease end its table has
    answered,    \* the changes answered
    steps        \* heartbeat rounds and requests so far, at most MaxSteps

vars == <<now, term, leaderOf, commit, committedAt, known, log, applied, lease, heard, held,
          grants, acked, until, answered, steps>>

Terms == 0..MaxTerm
Index == 1..MaxChanges
Majority == {Q \in SUBSET Node : 2 * Cardinality(Q) > Cardinality(Node)}

\* Node l believes it leads: it was elected in its own term.
Leads(l) == leaderOf[term[l]] = l

CanAuth(n) == now < lease[n]

Init ==
    /\ now = 0
    /\ term = [n \in Node |-> 0]
    /\ leaderOf = [t \in Terms |-> None]
    /\ commit = 0 /\ committedAt = [i \in Index |-> 0]
    /\ known = [n \in Node |-> 0] /\ log = [n \in Node |-> 0] /\ applied = [n \in Node |-> 0]
    /\ lease = [n \in Node |-> 0] /\ heard = [n \in Node |-> 0]
    /\ held = [n \in Node |-> {}] /\ grants = {}
    /\ acked = [l \in Node |-> [n \in Node |-> 0]]
    /\ until = [l \in Node |-> [n \in Node |-> 0]]
    /\ answered = {} /\ steps = 0

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<term, leaderOf, commit, committedAt, known, log, applied, lease, heard,
                   held, grants, acked, until, answered, steps>>

\* The nodes play the same part: TLC checks one state of each permutation of them.
Perms == Permutations(Node)

\* Node c wins term t, one past its own, with the votes of majority Q. Its log holds every committed change, so its
\* commit index covers them once the first entry of its term commits (the code waits for that).
Elect(c, t, Q) ==
    /\ t = term[c] + 1 /\ c \in Q /\ leaderOf[t] = None /\ log[c] = commit
    /\ \A q \in Q : term[q] < t
    /\ term' = [n \in Node |-> IF n \in Q THEN t ELSE term[n]]
    /\ leaderOf' = [leaderOf EXCEPT ![t] = c]
    /\ known' = [known EXCEPT ![c] = commit]
    /\ heard' = [heard EXCEPT ![c] = 0]
    /\ held' = [held EXCEPT ![c] = {}]
    /\ acked' = [acked EXCEPT ![c] = [n \in Node |-> 0]]
    /\ until' = [until EXCEPT ![c] = [n \in Node |-> now + Lease]]
    /\ UNCHANGED <<now, commit, committedAt, log, applied, lease, grants, answered, steps>>

\* A heartbeat round of l acknowledged by majority Q, whose members take l's term if theirs is
\* not later, and its log.
Round(l, Q) ==
    /\ steps < MaxSteps /\ steps' = steps + 1
    /\ Leads(l) /\ l \in Q /\ \A q \in Q : term[q] <= term[l]
    /\ term' = [n \in Node |-> IF n \in Q THEN term[l] ELSE term[n]]
    /\ log' = [n \in Node |-> IF n \in Q /\ log[n] < log[l] THEN log[l] ELSE log[n]]
    /\ heard' = [heard EXCEPT ![l] = now + 1]
    /\ UNCHANGED <<now, leaderOf, commit, committedAt, known, applied, lease, held, grants,
                   acked, until, answered>>

\* Leader l commits a change with majority Q in its term.
Commit(l, Q) ==
    /\ Leads(l) /\ commit < MaxChanges /\ l \in Q
    /\ \A q \in Q : term[q] = term[l]
    /\ commit' = commit + 1
    /\ committedAt' = [committedAt EXCEPT ![commit + 1] = now]
    /\ log' = [n \in Node |-> IF n \in Q THEN commit + 1 ELSE log[n]]
    /\ known' = [known EXCEPT ![l] = commit + 1]
    /\ UNCHANGED <<now, term, leaderOf, applied, lease, heard, held, grants, acked, until,
                   answered, steps>>

Apply(n) ==
    /\ applied[n] < log[n]
    /\ applied' = [applied EXCEPT ![n] = @ + 1]
    /\ UNCHANGED <<now, term, leaderOf, commit, committedAt, known, log, lease, heard, held,
                   grants, acked, until, answered, steps>>

\* Node n asks the leader of its own term, l, which holds the request; one request of n held or
\* answered at a time. The request arrives as it leaves: a later arrival only asks a later round of
\* the leader, and the lease still runs from when the node asked.
Ask(n, l) ==
    /\ steps < MaxSteps /\ steps' = steps + 1
    /\ l = leaderOf[term[n]] /\ Leads(l) /\ now + Lease <= MaxTime
    /\ \A m \in Node : \A h \in held[m] : h.node /= n
    /\ \A g \in grants : g.node /= n
    /\ held' = [held EXCEPT ![l] = @ \cup {[node |-> n, at |-> now, idx |-> applied[n]]}]
    /\ UNCHANGED <<now, term, leaderOf, commit, committedAt, known, log, applied, lease, heard,
                   grants, acked, until, answered>>

\* Leader l answers a request it holds: after a majority acknowledged a round sent since the
\* request, or on its belief alone with RoleOnly.
Release(l, h) ==
    /\ Leads(l) /\ h \in held[l]
    /\ RoleOnly \/ heard[l] > h.at
    /\ held' = [held EXCEPT ![l] = @ \ {h}]
    /\ acked' = [acked EXCEPT ![l][h.node] = IF h.idx > @ THEN h.idx ELSE @]
    /\ until' = [until EXCEPT ![l][h.node] = IF now + Lease > @ THEN now + Lease ELSE @]
    /\ grants' = grants \cup {[node |-> h.node, at |-> h.at, commit |-> known[l]]}
    /\ UNCHANGED <<now, term, leaderOf, commit, committedAt, known, log, applied, lease, heard,
                    answered, steps>>

\* The answer reaches its node: a lease from when it asked, if its copy reached the commit index.
Grant(g) ==
    /\ g \in grants
    /\ grants' = grants \ {g}
    /\ lease' = IF applied[g.node] >= g.commit /\ g.at + Lease > lease[g.node]
                THEN [lease EXCEPT ![g.node] = g.at + Lease]
                ELSE lease
    /\ UNCHANGED <<now, term, leaderOf, commit, committedAt, known, log, applied, heard,
                   held, acked, until, answered, steps>>

\* Leader l answers change i: every node of its table acknowledged it or its lease there ended.
Answer(l, i) ==
    /\ Leads(l) /\ i <= known[l] /\ i \notin answered
    /\ \A n \in Node : acked[l][n] >= i \/ until[l][n] <= now
    /\ answered' = answered \cup {i}
    /\ UNCHANGED <<now, term, leaderOf, commit, committedAt, known, log, applied, lease, heard,
                    held, grants, acked, until, steps>>

Next ==
    \/ Tick
    \/ \E c \in Node, t \in 1..MaxTerm, Q \in Majority : Elect(c, t, Q)
    \/ \E l \in Node, Q \in Majority : Round(l, Q) \/ Commit(l, Q)
    \/ \E n \in Node : Apply(n) \/ \E l \in Node : Ask(n, l)
    \/ \E l \in Node : \E h \in held[l] : Release(l, h)
    \/ \E g \in grants : Grant(g)
    \/ \E l \in Node, i \in Index : Answer(l, i)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ commit \in 0..MaxChanges /\ answered \subseteq 1..commit
    /\ \A n \in Node : applied[n] <= log[n] /\ log[n] <= commit /\ term[n] \in Terms

\* Once a change has answered, every node that authorizes has applied it.
FencedAfterAnswer == \A i \in answered : \A n \in Node : CanAuth(n) => applied[n] >= i

\* One Lease after a change committed, every node that authorizes has applied it.
FencedWithinLease ==
    \A i \in 1..commit : now >= committedAt[i] + Lease =>
        \A n \in Node : CanAuth(n) => applied[n] >= i
=============================================================================
