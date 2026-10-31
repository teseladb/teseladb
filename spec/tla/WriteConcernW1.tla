---------------------------- MODULE WriteConcernW1 ----------------------------
(* `w: 1` acknowledgements before the majority, and rollback (005 FR-010, D-034).                  *)
(* Each node has a Raft log and the prefix of it that is on disk (`fdatasync`); a crash keeps only  *)
(* that prefix. A write `w: 1` is acknowledged once the entry is in the leader's log, and with      *)
(* `j: true` once it is also on the leader's disk. An entry is committed when a quorum of the       *)
(* leader's term has it on disk. A node that takes a leader's log writes what it drops to its       *)
(* rollback files, as RefDb does. A leader serves reads only up to the commit index it knows.     *)
(* Invariants: a committed entry is in the log of every leader of its term or a later one; a write  *)
(* acknowledged with `w: 1, j: true` is never lost without a trace (it is committed, still in a log *)
(* or in a rollback file); and reads only return committed entries (DV-010).                       *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS Node, Value, MaxTerm

VARIABLES
    term,       \* each node's term
    leader,     \* each node believes it leads its term
    log,        \* each node's log: entries [t |-> term, v |-> value, j |-> journaled ack]
    synced,     \* length of the prefix of each log on disk
    known,      \* commit index each leader knows
    committed,  \* entries committed so far
    acked,      \* entries acknowledged with w: 1
    rolledBack, \* entries written to rollback files
    reads,      \* entries returned by reads
    written     \* values already written

vars == <<term, leader, log, synced, known, committed, acked, rolledBack, reads, written>>

Majority(Q) == 2 * Cardinality(Q) > Cardinality(Node)
Last(s) == IF s = <<>> THEN 0 ELSE s[Len(s)].t
UpToDate(c, n) == Last(log[c]) > Last(log[n]) \/ (Last(log[c]) = Last(log[n]) /\ Len(log[c]) >= Len(log[n]))
InLog(e, s) == \E k \in 1..Len(s) : s[k] = e
Entries(s, a, b) == {s[k] : k \in a..b}
Min(a, b) == IF a < b THEN a ELSE b
\* Length of the common prefix of two logs.
Common(s, r) == LET k == {i \in 0..Min(Len(s), Len(r)) : \A m \in 1..i : s[m] = r[m]}
                IN CHOOSE i \in k : \A x \in k : x <= i

Init ==
    /\ term = [n \in Node |-> 0]
    /\ leader = [n \in Node |-> FALSE]
    /\ log = [n \in Node |-> <<>>]
    /\ synced = [n \in Node |-> 0]
    /\ known = [n \in Node |-> 0]
    /\ committed = {}
    /\ acked = {}
    /\ rolledBack = {}
    /\ reads = {}
    /\ written = {}

Write(l, v, j) ==
    /\ leader[l] /\ v \notin written
    /\ log' = [log EXCEPT ![l] = Append(@, [t |-> term[l], v |-> v, j |-> j])]
    /\ written' = written \cup {v}
    /\ UNCHANGED <<term, leader, synced, known, committed, acked, rolledBack, reads>>

\* fdatasync of a node's whole log.
Sync(n) ==
    /\ synced[n] < Len(log[n])
    /\ synced' = [synced EXCEPT ![n] = Len(log[n])]
    /\ UNCHANGED <<term, leader, log, known, committed, acked, rolledBack, reads, written>>

\* `w: 1`: the entry is in the leader's log, and on its disk if `j: true`.
AckW1(l, k) ==
    /\ leader[l] /\ k \in 1..Len(log[l])
    /\ log[l][k].t = term[l]
    /\ log[l][k].j => k <= synced[l]
    /\ acked' = acked \cup {log[l][k]}
    /\ UNCHANGED <<term, leader, log, synced, known, committed, rolledBack, reads, written>>

\* AppendEntries carrying the leader's whole log: the follower keeps the common prefix, drops the
\* rest to its rollback files and takes the leader's entries (not yet on its disk).
Replicate(l, f) ==
    /\ leader[l] /\ f /= l /\ term[f] <= term[l]
    /\ LET c == Common(log[l], log[f]) IN
       /\ rolledBack' = rolledBack \cup Entries(log[f], c + 1, Len(log[f]))
       /\ synced' = [synced EXCEPT ![f] = Min(@, c)]
    /\ log' = [log EXCEPT ![f] = log[l]]
    /\ term' = [term EXCEPT ![f] = term[l]]
    /\ leader' = [leader EXCEPT ![f] = FALSE]
    /\ UNCHANGED <<known, committed, acked, reads, written>>

\* The leader commits up to k: an entry of its term on the disk of a quorum of its term.
Commit(l, k, q) ==
    /\ leader[l] /\ l \in q /\ Majority(q)
    /\ k \in (known[l] + 1)..Len(log[l])
    /\ log[l][k].t = term[l]
    /\ \A n \in q : term[n] = term[l] /\ synced[n] >= k /\ Len(log[n]) >= k /\ log[n][k] = log[l][k]
    /\ known' = [known EXCEPT ![l] = k]
    /\ committed' = committed \cup Entries(log[l], 1, k)
    /\ UNCHANGED <<term, leader, log, synced, acked, rolledBack, reads, written>>

\* A read on the leader returns what it knows to be committed.
Read(l) ==
    /\ leader[l] /\ known[l] > 0
    /\ reads' = reads \cup Entries(log[l], 1, known[l])
    /\ UNCHANGED <<term, leader, log, synced, known, committed, acked, rolledBack, written>>

\* A crash keeps only what is on disk.
Crash(n) ==
    /\ log' = [log EXCEPT ![n] = SubSeq(@, 1, synced[n])]
    /\ leader' = [leader EXCEPT ![n] = FALSE]
    /\ known' = [known EXCEPT ![n] = 0]
    /\ UNCHANGED <<term, synced, committed, acked, rolledBack, reads, written>>

\* Candidate c wins term t with the votes of quorum q, whose logs are not newer than its own.
Elect(c, q) ==
    LET t == term[c] + 1 IN
    /\ t <= MaxTerm /\ c \in q /\ Majority(q)
    /\ \A n \in q : term[n] < t /\ UpToDate(c, n)
    /\ term' = [n \in Node |-> IF n \in q THEN t ELSE term[n]]
    /\ leader' = [n \in Node |-> IF n = c THEN TRUE ELSE IF n \in q THEN FALSE ELSE leader[n]]
    /\ known' = [known EXCEPT ![c] = 0]
    /\ UNCHANGED <<log, synced, committed, acked, rolledBack, reads, written>>

Next ==
    \/ \E n \in Node : Sync(n) \/ Crash(n) \/ Read(n)
    \/ \E l \in Node, v \in Value, j \in BOOLEAN : Write(l, v, j)
    \/ \E l \in Node, k \in 1..Cardinality(Value) : AckW1(l, k)
    \/ \E l, f \in Node : Replicate(l, f)
    \/ \E l \in Node, k \in 1..Cardinality(Value), q \in SUBSET Node : Commit(l, k, q)
    \/ \E c \in Node, q \in SUBSET Node : Elect(c, q)

Spec == Init /\ [][Next]_vars

CommittedInLeaders ==
    \A l \in Node : leader[l] => \A e \in committed : e.t <= term[l] => InLog(e, log[l])

JournaledW1NeverLostSilently ==
    \A e \in acked : e.j => (e \in committed \/ e \in rolledBack \/ \E n \in Node : InLog(e, log[n]))

ReadsAreCommitted == reads \subseteq committed

TypeOK ==
    /\ term \in [Node -> 0..MaxTerm]
    /\ \A n \in Node : synced[n] <= Len(log[n]) /\ Len(log[n]) <= Cardinality(Value)
=============================================================================
