---------------------------- MODULE HlcUncertainty ----------------------------
(* The uncertainty window of reads (008 FR-008, FR-015, T002, T024, T031, T033, T035).            *)
(* Real time advances in ticks; each node's physical clock is real time plus a fixed offset of at *)
(* most Skew ticks from the median (D-004: 125 ms, so two nodes differ by up to 2 * Skew); an HLC  *)
(* value is physical * Scale + logical, and a node's HLC never goes below its physical clock nor  *)
(* below the timestamps it receives. One key has a leader. When the lease moves, the new leader's *)
(* read cache starts 2 * Skew above its clock (005 FR-005) and its HLC goes there, above every    *)
(* write of the previous leader. Transactions write the key one at a time: an intent above the    *)
(* leader's HLC and the key's newest commit, then a commit at the highest of the intent's mark,   *)
(* the pushes it got (its minimum commit) and the clock of the node that coordinates it. The      *)
(* commit is acknowledged once that node's clock reaches the commit's mark (commit wait), and its *)
(* intent is resolved into a version at the commit's mark some time later. A read takes its       *)
(* timestamp from the HLC of the node that runs the command, goes to the leader and, the first    *)
(* time it visits a node, records that node's HLC (the observed timestamp, which the visit also   *)
(* pushes up to the read's timestamp). At the leader, a version or an intent in (t, t + Window]   *)
(* makes the read restart at its mark, whatever the read observed: a commit takes its mark from   *)
(* its coordinator's clock, which may be above the leader's, so an observed timestamp does not    *)
(* bound what the leader holds (mutant observed_bounds_window). An intent at or below t of a       *)
(* pending transaction is pushed to succ(t), so its commit goes above the read, and the read goes *)
(* on below it; of a committed one, the read takes its value at a commit at or below t and        *)
(* restarts at a commit in its window, as at a resolved version there. A stale route can take the *)
(* read to a node that is not the leader, which it also observes.                                 *)
(* Invariants: a read never misses a write acknowledged before it started (real-time order), what *)
(* a finished read saw at its mark stays what that mark shows, and no two commits share a mark.   *)
(* Mutant (cfg constant): "none"; "zero_uncertainty" (no window); "median_window" (a window of    *)
(* Skew, the deviation from the median, instead of 2 * Skew, with the clocks skewed in opposite   *)
(* directions); "observed_bounds_window" (the observed timestamp of the leader bounds the window, *)
(* with the start of the lease if it began after the observation, as FR-008 first had it);       *)
(* "no_commit_wait" (a commit is acknowledged at once);                                          *)
(* "push_without_succ" (a push lifts the commit to the read's mark instead of the one above it);  *)
(* "known_commit_ignores_window" (a read that learns of a commit in its window from an intent at  *)
(* or below its mark reads below the intent); and "intent_below_commit" (an intent goes above the *)
(* leader's HLC only, not above the key's newest commit).                                        *)
EXTENDS Integers, FiniteSets, Sequences

CONSTANTS Nodes, Skew, MaxReal, MaxWrites, MaxTransfers, Scale, Mutant

\* The window: 2 * Skew, or the faulty ones.
Window == CASE Mutant = "zero_uncertainty" -> 0
            [] Mutant = "median_window" -> Skew
            [] OTHER -> 2 * Skew

Idle == "idle"
Reading == "reading"
Done == "done"
\* No mark, or no acknowledgment, yet.
None == -1

VARIABLES
    real,        \* real time
    off,         \* each node's clock offset (fixed)
    hlc,         \* each node's HLC
    leader,      \* the key's leader
    leaseStart,  \* start of its lease
    txns,        \* the key's transactions in order: [its, minC, cts, ack, by, res] (intent's mark,
                 \* minimum commit, commit's mark, real time of the acknowledgment, coordinator,
                 \* resolved)
    transfers,   \* lease transfers so far
    rd,          \* the read: [st, node, rts, before, observed, seen] (before: the commit marks
                 \* acknowledged when it started; seen: the transaction it read, 0 for none)
    broken       \* ghost: a read missed a write acknowledged before it started

vars == <<real, off, hlc, leader, leaseStart, txns, transfers, rd, broken>>

Max(a, b) == IF a > b THEN a ELSE b
Min(a, b) == IF a < b THEN a ELSE b
SetMax(s) == CHOOSE m \in s : \A x \in s : x <= m
\* A node's physical clock as an HLC value at real time r (2 * Skew keeps every value natural).
PhysAt(n, r) == (r + off[n] + 2 * Skew) * Scale
Phys(n) == PhysAt(n, real)
\* The mark just above t; the faulty push lifts to t itself.
PushTo(t) == IF Mutant = "push_without_succ" THEN t ELSE t + 1
Last == Len(txns)
Committed(i) == txns[i].cts # None

TypeOK ==
    /\ real \in 0..MaxReal
    /\ leader \in Nodes
    /\ Len(txns) <= MaxWrites
    /\ rd.st \in {Idle, Reading, Done}
    /\ broken \in BOOLEAN

Init ==
    /\ real = 0
    /\ off \in [Nodes -> -Skew..Skew]
    /\ hlc = [n \in Nodes |-> PhysAt(n, 0)]
    /\ leader \in Nodes
    /\ leaseStart = 0
    /\ txns = <<>>
    /\ transfers = 0
    /\ rd = [st |-> Idle, node |-> CHOOSE n \in Nodes : TRUE, rts |-> 0, before |-> {},
             observed |-> [n \in Nodes |-> None], seen |-> 0]
    /\ broken = FALSE

\* Commit wait: a committed transaction is acknowledged once its coordinator's clock is at its
\* commit's mark at real time r.
Ack(t, r) == IF t.cts # None /\ t.ack = None /\ PhysAt(t.by, r) >= t.cts
                THEN [t EXCEPT !.ack = r] ELSE t

\* Real time goes on; each HLC follows its physical clock; commits whose clock got there answer.
Tick ==
    /\ real < MaxReal
    /\ real' = real + 1
    /\ hlc' = [n \in Nodes |-> Max(hlc[n], PhysAt(n, real'))]
    /\ txns' = [i \in 1..Last |-> Ack(txns[i], real')]
    /\ UNCHANGED <<off, leader, leaseStart, transfers, rd, broken>>

\* A new transaction writes its intent at the leader, once the previous one committed: above the
\* leader's HLC and above the previous commit, whose intent it resolves first.
Intent ==
    /\ Last < MaxWrites
    /\ IF Last = 0 THEN TRUE ELSE Committed(Last)
    /\ LET newest == IF Last = 0 \/ Mutant = "intent_below_commit" THEN 0 ELSE txns[Last].cts
           its == Max(hlc[leader], newest) + 1
           resolved == IF Last = 0 THEN txns ELSE [txns EXCEPT ![Last].res = TRUE]
       IN /\ its >= leaseStart
          /\ txns' = Append(resolved, [its |-> its, minC |-> 0, cts |-> None, ack |-> None,
                                       by |-> leader, res |-> FALSE])
          /\ hlc' = [hlc EXCEPT ![leader] = its]
    /\ UNCHANGED <<real, off, leader, leaseStart, transfers, rd, broken>>

\* Coordinator w commits the pending transaction at the highest of its intent's mark, its
\* minimum commit and w's clock; it answers once w's clock is there.
Commit(w) ==
    /\ Last > 0 /\ ~Committed(Last)
    /\ LET t == txns[Last]
           cts == Max(Max(t.its, t.minC), Phys(w))
           ack == IF Mutant = "no_commit_wait" \/ Phys(w) >= cts THEN real ELSE None
       IN txns' = [txns EXCEPT ![Last] = [t EXCEPT !.cts = cts, !.ack = ack, !.by = w]]
    /\ UNCHANGED <<real, off, hlc, leader, leaseStart, transfers, rd, broken>>

\* A committed intent becomes a version at its commit's mark.
Resolve(i) ==
    /\ i \in 1..Last /\ Committed(i) /\ ~txns[i].res
    /\ txns' = [txns EXCEPT ![i].res = TRUE]
    /\ UNCHANGED <<real, off, hlc, leader, leaseStart, transfers, rd, broken>>

\* The lease moves to node n: the voters carry the previous leader's clock to it (D-069), so the
\* lease starts above every write and every read of the previous leader; the new leader's read
\* cache starts 2 * Skew above its clock and its HLC goes there.
Transfer(n) ==
    /\ n # leader /\ transfers < MaxTransfers
    /\ transfers' = transfers + 1
    /\ LET marks == {txns[i].its : i \in 1..Last} \cup {txns[i].cts : i \in 1..Last}
           top == SetMax(marks \cup {0, hlc[leader]})
           start == Max(hlc[n], top) + 1
       IN /\ leader' = n
          /\ leaseStart' = start
          /\ hlc' = [hlc EXCEPT ![n] = Max(start, (real + off[n] + 4 * Skew) * Scale)]
    /\ UNCHANGED <<real, off, txns, rd, broken>>

\* A read starts on node n at its HLC.
Start(n) ==
    /\ rd.st = Idle
    /\ rd' = [st |-> Reading, node |-> n, rts |-> hlc[n],
              before |-> {txns[i].cts : i \in {j \in 1..Last : txns[j].ack # None}},
              observed |-> [m \in Nodes |-> None], seen |-> 0]
    /\ UNCHANGED <<real, off, hlc, leader, leaseStart, txns, transfers, broken>>

\* The upper end of the window at the leader for a read with these observations.
Upper(r, obs) ==
    LET full == (r.rts \div Scale + Window) * Scale + Scale - 1
        bound == IF leaseStart <= obs THEN obs ELSE Max(obs, leaseStart)
    IN IF Mutant = "observed_bounds_window" THEN Min(full, bound) ELSE full

\* The mark at which transaction i makes a read at t with window up to u restart, or None: an
\* intent in the window; a commit in the window, once its intent at or below t is known (the
\* faulty reader reads below an unresolved one); a resolved version in the window.
Uncertain(i, t, u) ==
    LET x == txns[i]
        mark == IF x.res THEN x.cts ELSE x.its
    IN IF mark > t
          THEN IF mark <= u THEN mark ELSE None
       ELSE IF ~x.res /\ x.cts # None /\ x.cts > t /\ x.cts <= u
               /\ Mutant # "known_commit_ignores_window"
          THEN x.cts
       ELSE None

\* The read at the leader: it observes the leader's clock on its first visit, restarts at the
\* highest mark in its window, or is served: it pushes the pending intent at or below its mark
\* above it and reads the newest commit at or below its mark.
Visit ==
    /\ rd.st = Reading
    /\ LET obs == IF rd.observed[leader] # None THEN rd.observed[leader] ELSE hlc[leader]
           r1 == [rd EXCEPT !.observed[leader] = obs]
           u == Upper(r1, obs)
           marks == {Uncertain(i, rd.rts, u) : i \in 1..Last} \ {None}
           visible == {i \in 1..Last : Committed(i) /\ txns[i].cts <= rd.rts}
       IN IF marks # {}
             THEN /\ rd' = [r1 EXCEPT !.rts = SetMax(marks)]
                  /\ UNCHANGED <<txns, broken>>
             ELSE /\ rd' = [r1 EXCEPT !.st = Done,
                                      !.seen = IF visible = {} THEN 0 ELSE SetMax(visible)]
                  /\ txns' = IF Last > 0 /\ ~Committed(Last) /\ txns[Last].its <= rd.rts
                                THEN [txns EXCEPT ![Last].minC = Max(@, PushTo(rd.rts))]
                                ELSE txns
                  /\ broken' = (\/ broken
                                \/ \E ts \in rd.before : ts > rd.rts)
       \* The visit carries the read's timestamp: the leader's HLC goes above it.
    /\ hlc' = [hlc EXCEPT ![leader] = Max(@, rd.rts)]
    /\ UNCHANGED <<real, off, leader, leaseStart, transfers>>

\* A stale route takes the read to node n, which is not (or no longer) the leader: the read
\* records its clock and goes on to the leader.
Observe(n) ==
    /\ rd.st = Reading /\ n # leader /\ rd.observed[n] = None
    /\ rd' = [rd EXCEPT !.observed[n] = hlc[n]]
    /\ hlc' = [hlc EXCEPT ![n] = Max(@, rd.rts)]
    /\ UNCHANGED <<real, off, leader, leaseStart, txns, transfers, broken>>

Next ==
    \/ Tick \/ Intent \/ Visit
    \/ \E n \in Nodes : Transfer(n) \/ Start(n) \/ Observe(n) \/ Commit(n)
    \/ \E i \in 1..MaxWrites : Resolve(i)

Spec == Init /\ [][Next]_vars

\* Real-time order: no read misses a write acknowledged before it started.
ReadsSeeEarlierWrites == ~broken

\* What a finished read saw at its mark is what that mark shows now: no commit appears at or
\* below the mark of a read that went past its intent.
ReadsAreStable ==
    rd.st = Done =>
        LET visible == {i \in 1..Last : Committed(i) /\ txns[i].cts <= rd.rts}
        IN rd.seen = IF visible = {} THEN 0 ELSE SetMax(visible)

\* No two commits of the key share a mark.
CommitMarksAreUnique ==
    \A i, j \in 1..Last : i # j /\ Committed(i) /\ Committed(j) => txns[i].cts # txns[j].cts
=============================================================================
